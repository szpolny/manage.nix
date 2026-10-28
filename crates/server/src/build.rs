//! Building a machine's system: fetch the repository into a mirror, build
//! the machine's NixOS configuration, sign the result, and push it to the
//! binary cache. With [`BuildOn::Machine`], the server only fetches and
//! resolves the revision, and serves the source to the machines as Git
//! bundles ([`Builder::bundle`]); they build their systems themselves.
//!
//! Everything runs through the `git`, `nix`, and `attic` command-line tools
//! with fixed arguments; nothing goes through a shell. Their output goes to
//! the deployment's log.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context;
use mnix_nix::{SecretKey, StorePath};
use mnix_protocol::Os;
use mnix_store::{log_source, NewLogEntry, Store};
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use uuid::Uuid;

/// How often buffered output is written to the log.
const LOG_FLUSH: Duration = Duration::from_secs(1);
/// The most output buffered before it is written.
const LOG_BUFFER: usize = 16 * 1024;

/// Pushes built systems to an Attic cache with the `attic` client.
#[derive(Debug, Clone)]
pub struct Attic {
    /// The cache's name on the server.
    pub cache: String,
    /// `XDG_CONFIG_HOME` holding `attic/config.toml` for the server.
    config_home: PathBuf,
}

/// The name of the Attic server in the generated client configuration.
const ATTIC_SERVER: &str = "mnix";

impl Attic {
    /// Writes a client configuration for `endpoint` with `token` under
    /// `state_dir`, readable only by the server.
    pub fn configure(
        state_dir: &Path,
        endpoint: &str,
        cache: &str,
        token: &str,
    ) -> anyhow::Result<Self> {
        let plain = |s: &str| {
            !s.is_empty() && !s.contains(|c: char| c == '"' || c == '\\' || c.is_control())
        };
        anyhow::ensure!(plain(endpoint), "invalid Attic endpoint {endpoint:?}");
        anyhow::ensure!(plain(token), "invalid Attic token");
        anyhow::ensure!(
            crate::api::is_valid_name(cache),
            "invalid Attic cache name {cache:?}"
        );
        let config_home = state_dir.join("attic");
        let dir = config_home.join("attic");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let config = format!(
            "default-server = \"{ATTIC_SERVER}\"\n\n[servers.{ATTIC_SERVER}]\nendpoint = \"{endpoint}\"\ntoken = \"{token}\"\n"
        );
        crate::write_private(&dir.join("config.toml"), &config)?;
        Ok(Self {
            cache: cache.to_owned(),
            config_home,
        })
    }
}

/// Where deployments build systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum BuildOn {
    /// The server builds, signs, and pushes each system to the cache (or
    /// keeps it, when the machine is the server's own).
    Server,
    /// Each machine builds its own system from source the server sends.
    Machine,
}

pub struct Builder {
    /// Mirrors in `repositories/`, garbage collector roots in `gcroots/`.
    pub state_dir: PathBuf,
    pub signing_key: SecretKey,
    pub attic: Option<Attic>,
    pub build_on: BuildOn,
    /// SSH private keys to fetch repositories with, by repository.
    pub ssh_keys: HashMap<Uuid, PathBuf>,
    /// Serializes fetches and bundles, which share the mirrors' refs.
    pub git_lock: tokio::sync::Mutex<()>,
}

/// A system built for one machine.
#[derive(Debug, Clone)]
pub struct Built {
    pub store_path: StorePath,
    pub signature: String,
}

impl Builder {
    pub fn mirror(&self, repository_id: Uuid) -> PathBuf {
        self.state_dir
            .join("repositories")
            .join(format!("{repository_id}.git"))
    }

    /// `git`, with the repository's SSH key if it has one.
    fn git_for(&self, repository_id: Uuid) -> Command {
        let mut command = git();
        if let Some(key) = self.ssh_keys.get(&repository_id) {
            let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
            command.env(
                "GIT_SSH_COMMAND",
                format!(
                    "ssh -i {} -o IdentitiesOnly=yes -o BatchMode=yes \
                     -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile={}",
                    quote(key),
                    quote(&self.state_dir.join("known_hosts"))
                ),
            );
        }
        command
    }

    /// Clones the repository on first use and fetches it afterwards.
    /// Returns the mirror's path.
    pub async fn fetch(
        &self,
        repository_id: Uuid,
        url: &str,
        log: &mut Log,
    ) -> anyhow::Result<PathBuf> {
        let _git = self.git_lock.lock().await;
        let dir = self.state_dir.join("repositories");
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
        let mirror = dir.join(format!("{repository_id}.git"));
        if mirror.exists() {
            log.line(format!("fetching {url}")).await;
            run(
                self.git_for(repository_id)
                    .arg("-C")
                    .arg(&mirror)
                    .args(["remote", "set-url", "origin", "--"])
                    .arg(url),
                log,
            )
            .await?;
            run(
                self.git_for(repository_id)
                    .arg("-C")
                    .arg(&mirror)
                    .args(["fetch", "--prune", "--quiet", "origin"]),
                log,
            )
            .await?;
        } else {
            log.line(format!("cloning {url}")).await;
            let partial = dir.join(format!("{repository_id}.git.partial"));
            let _ = tokio::fs::remove_dir_all(&partial).await;
            run(
                self.git_for(repository_id)
                    .args(["clone", "--mirror", "--quiet", "--"])
                    .arg(url)
                    .arg(&partial),
                log,
            )
            .await?;
            tokio::fs::rename(&partial, &mirror).await?;
        }
        Ok(mirror)
    }

    /// The commit `rev` names in the mirror.
    pub async fn resolve(&self, mirror: &Path, rev: &str, log: &mut Log) -> anyhow::Result<String> {
        let out = run(
            git()
                .arg("-C")
                .arg(mirror)
                .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
                .arg(format!("{rev}^{{commit}}")),
            log,
        )
        .await
        .with_context(|| format!("{rev:?} is not a branch, tag, or commit of the repository"))?;
        let commit = out.trim().to_owned();
        anyhow::ensure!(
            commit.len() >= 40 && commit.chars().all(|c| c.is_ascii_hexdigit()),
            "unexpected commit {commit:?}"
        );
        Ok(commit)
    }

    /// Builds `nixosConfigurations.<configuration>` at `commit`, signs it,
    /// and pushes it to the cache.
    pub async fn build(
        &self,
        mirror: &Path,
        commit: &str,
        machine_id: Uuid,
        configuration: &str,
        log: &mut Log,
    ) -> anyhow::Result<Built> {
        let installable = installable(mirror, commit, &Os::Nixos.flake_attribute(configuration))?;
        // Keeps the latest system of each machine from being garbage
        // collected.
        let out_link = self.state_dir.join("gcroots").join(machine_id.to_string());
        let store_path = self.nix_build(&installable, &out_link, log).await?;

        let info = {
            let path = store_path.clone();
            tokio::task::spawn_blocking(move || mnix_nix::query_path_info(&path)).await??
        };
        let signature = self.signing_key.sign(&info);
        log.line(format!("signed with {}", self.signing_key.name()))
            .await;

        if let Some(attic) = &self.attic {
            log.line(format!("pushing to the Attic cache {}", attic.cache))
                .await;
            run(
                Command::new("attic")
                    .env("XDG_CONFIG_HOME", &attic.config_home)
                    .args(["push", "--"])
                    .arg(format!("{ATTIC_SERVER}:{}", attic.cache))
                    .arg(store_path.as_str()),
                log,
            )
            .await?;
        }
        Ok(Built {
            store_path,
            signature,
        })
    }
}

impl Builder {
    /// `nix build`s `installable`, linking the result from `out_link`.
    pub async fn nix_build(
        &self,
        installable: &str,
        out_link: &Path,
        log: &mut Log,
    ) -> anyhow::Result<StorePath> {
        if let Some(dir) = out_link.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        log.line(format!("building {installable}")).await;
        let started = Instant::now();
        let out = run(
            Command::new("nix")
                .args([
                    "--extra-experimental-features",
                    "nix-command flakes",
                    "build",
                    "--print-build-logs",
                    "--print-out-paths",
                    "--no-write-lock-file",
                    "--out-link",
                ])
                .arg(out_link)
                .arg(installable),
            log,
        )
        .await?;
        let store_path = out
            .lines()
            .last()
            .map(str::trim)
            .context("nix build printed no store path")?;
        let store_path = StorePath::parse(store_path)?;
        log.line(format!(
            "built {store_path} in {}",
            humantime::format_duration(Duration::from_secs(started.elapsed().as_secs()))
        ))
        .await;
        Ok(store_path)
    }

    /// A Git bundle of `commit` from the repository's mirror, without the
    /// history of the `haves` it knows: what an agent needs to build the
    /// commit ([`mnix_protocol::agent_source_path`]).
    pub async fn bundle(
        &self,
        repository_id: Uuid,
        commit: &str,
        haves: &[String],
    ) -> anyhow::Result<Vec<u8>> {
        let is_commit = |c: &str| c.len() == 40 && c.chars().all(|c| c.is_ascii_hexdigit());
        anyhow::ensure!(is_commit(commit), "invalid commit {commit:?}");
        let mirror = self.mirror(repository_id);
        let _git = self.git_lock.lock().await;
        let mut excluded = Vec::new();
        for have in haves.iter().filter(|h| is_commit(h)) {
            let known = git()
                .arg("-C")
                .arg(&mirror)
                .args(["cat-file", "-e", &format!("{have}^{{commit}}")])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await?
                .success();
            // Leaving out an ancestor of a have the agent lacks would
            // make the bundle unusable, so only exclude what is known.
            if known && have != commit {
                excluded.push(format!("^{have}"));
            }
        }
        let quiet = |command: &mut Command| {
            command
                .arg("-C")
                .arg(&mirror)
                .stdin(Stdio::null())
                .stderr(Stdio::piped());
        };
        let mut update = git();
        quiet(&mut update);
        let out = update
            .args(["update-ref", mnix_protocol::SOURCE_BUNDLE_REF, commit])
            .output()
            .await?;
        anyhow::ensure!(
            out.status.success(),
            "git update-ref: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        let mut bundle = git();
        quiet(&mut bundle);
        let out = bundle
            .args([
                "bundle",
                "create",
                "--quiet",
                "-",
                mnix_protocol::SOURCE_BUNDLE_REF,
            ])
            .args(&excluded)
            .stdout(Stdio::piped())
            .output()
            .await;
        let mut delete = git();
        quiet(&mut delete);
        let _ = delete
            .args(["update-ref", "-d", mnix_protocol::SOURCE_BUNDLE_REF])
            .status()
            .await;
        let out = out?;
        anyhow::ensure!(
            out.status.success(),
            "git bundle: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(out.stdout)
    }
}

/// The flake output `attribute` at `commit` of a mirror.
pub fn installable(mirror: &Path, commit: &str, attribute: &str) -> anyhow::Result<String> {
    let mirror = mirror.to_str().context("non-UTF-8 state directory")?;
    Ok(format!("git+file://{mirror}?rev={commit}#{attribute}"))
}

pub fn git() -> Command {
    let mut command = Command::new("git");
    // Never wait for a password prompt.
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

/// Runs a command, streaming its stderr to `log`, and returns its stdout.
async fn run(command: &mut Command, log: &mut Log) -> anyhow::Result<String> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {program}"))?;
    let mut stdout = child.stdout.take().expect("piped");
    let stdout = tokio::spawn(async move {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).await.map(|_| out)
    });
    let mut lines = BufReader::new(child.stderr.take().expect("piped")).lines();
    let mut last = String::new();
    while let Some(line) = lines.next_line().await? {
        log.line(line.clone()).await;
        last = line;
    }
    let status = child.wait().await?;
    let out = String::from_utf8_lossy(&stdout.await??).into_owned();
    anyhow::ensure!(
        status.success(),
        "{program} failed ({status}){}",
        if last.is_empty() {
            String::new()
        } else {
            format!(": {last}")
        }
    );
    Ok(out)
}

/// Buffers output for a deployment's log and writes it in batches.
pub struct Log {
    /// `None` for output that only goes to the server's own log.
    store: Option<std::sync::Arc<dyn Store>>,
    deployment_id: Uuid,
    machine_id: Option<Uuid>,
    buffer: String,
    flushed: Instant,
}

impl Log {
    pub fn new(
        store: std::sync::Arc<dyn Store>,
        deployment_id: Uuid,
        machine_id: Option<Uuid>,
    ) -> Self {
        Self {
            store: Some(store),
            deployment_id,
            machine_id,
            buffer: String::new(),
            flushed: Instant::now(),
        }
    }

    /// Output that belongs to no deployment: it is only traced.
    pub fn discard() -> Self {
        Self {
            store: None,
            deployment_id: Uuid::nil(),
            machine_id: None,
            buffer: String::new(),
            flushed: Instant::now(),
        }
    }

    pub async fn line(&mut self, line: impl Into<String>) {
        let line = line.into().replace('\0', " ");
        tracing::debug!(deployment = %self.deployment_id, "{line}");
        self.buffer.push_str(&line);
        self.buffer.push('\n');
        if self.buffer.len() >= LOG_BUFFER || self.flushed.elapsed() >= LOG_FLUSH {
            self.flush().await;
        }
    }

    pub async fn flush(&mut self) {
        self.flushed = Instant::now();
        let Some(store) = &self.store else {
            self.buffer.clear();
            return;
        };
        if self.buffer.is_empty() {
            return;
        }
        let entry = NewLogEntry {
            deployment_id: self.deployment_id,
            machine_id: self.machine_id,
            source: log_source::SERVER.to_owned(),
            text: std::mem::take(&mut self.buffer),
            created_at: OffsetDateTime::now_utc(),
        };
        // Losing log output must not fail the deployment.
        if let Err(e) = store.append_log(&entry).await {
            tracing::error!("writing the log of deployment {}: {e}", self.deployment_id);
        }
    }
}
