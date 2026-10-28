//! Activations requested by the server ("jobs").
//!
//! The service writes the server's request into
//! `<state dir>/activations/<deployment id>/` and starts
//! `mnix-agent activation-job <dir>` in a transient systemd unit (NixOS) or
//! its own launchd daemon (macOS). The activation, its health checks, the
//! wait for confirmation, and any rollback then carry on when the switch
//! restarts the agent service or cuts the machine off the network. Jobs
//! wait for each other: one machine activates one system at a time, even
//! when several servers ask. The job and the service talk through files in
//! that directory:
//!
//! | File | Written by | Contents |
//! |---|---|---|
//! | `request.json` | service | the server's [`Activate`] message |
//! | `server` | service | the name of the server that asked |
//! | `log` | the job's stdout and stderr | everything the activation printed |
//! | `status.json` | job | the current [`ActivationPhase`] and when it began |
//! | `confirmed` | service | exists once the server confirmed over a connection opened after the switch |
//! | `report.json` | job | the final [`ActivationReport`] |
//!
//! The service deletes the directory when the server has stored the report.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use mnix_protocol::{
    Activate, ActivationPhase, ActivationReport, ActivationStatus, ActivationTarget,
};
use serde::{Deserialize, Serialize};

use crate::activate::{self, Observer, Request};
use crate::config::AgentConfig;
use crate::host::LocalHost;

/// Job directories, relative to the state directory.
const JOBS_DIR: &str = "activations";
const REQUEST: &str = "request.json";
const SERVER: &str = "server";
const STATUS: &str = "status.json";
const CONFIRMED: &str = "confirmed";
const REPORT: &str = "report.json";
const LOG: &str = "log";
/// How often a job waiting for confirmation looks for it.
const CONFIRM_POLL: Duration = Duration::from_millis(250);
/// A job without a report whose unit is gone this long after it was
/// requested was interrupted (the machine rebooted, or the job crashed).
const START_GRACE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Status {
    phase: ActivationPhase,
    /// RFC 3339 with microseconds.
    since: String,
}

/// What the service knows about one job.
#[derive(Debug, Clone)]
pub struct Job {
    pub deployment_id: String,
    /// The server that asked for it; `None` for jobs from agents before
    /// 0.2, which had one server.
    pub server: Option<String>,
    pub dir: PathBuf,
    pub phase: Option<ActivationPhase>,
    /// When the current phase began.
    pub since: Option<SystemTime>,
    pub report: Option<ActivationReport>,
}

impl Job {
    pub fn log_path(&self) -> PathBuf {
        self.dir.join(LOG)
    }

    /// When the job started waiting for confirmation, if it is waiting.
    pub fn awaiting_since(&self) -> Option<SystemTime> {
        match (self.report.as_ref(), self.phase) {
            (None, Some(ActivationPhase::AwaitingConfirmation)) => self.since,
            _ => None,
        }
    }
}

/// The service's side: starting jobs and following them.
#[derive(Debug, Clone)]
pub struct Jobs {
    root: PathBuf,
}

impl Jobs {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            root: state_dir.join(JOBS_DIR),
        }
    }

    fn dir(&self, deployment_id: &str) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(
            is_uuid(deployment_id),
            "invalid deployment ID {deployment_id:?}"
        );
        Ok(self.root.join(deployment_id))
    }

    /// Starts a job for `request` from `server` unless one exists for its
    /// deployment (the server repeats requests it has no answer to).
    /// Returns whether a job was started.
    pub fn start(&self, request: &Activate, server: &str) -> anyhow::Result<bool> {
        let dir = self.dir(&request.deployment_id)?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
        }
        write_atomically(&dir.join(SERVER), server.as_bytes())?;
        write_atomically(&dir.join(REQUEST), &serde_json::to_vec_pretty(request)?)?;
        if let Err(e) = spawn(&dir, &request.deployment_id) {
            tracing::error!("starting the activation: {e:#}");
            let report = failed_report(request, format!("could not start the activation: {e:#}"));
            write_atomically(&dir.join(REPORT), &serde_json::to_vec_pretty(&report)?)?;
        }
        Ok(true)
    }

    /// Every job, oldest first. Jobs that were interrupted get a report.
    pub fn list(&self) -> Vec<Job> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                tracing::error!("listing {}: {e}", self.root.display());
                return Vec::new();
            }
        };
        let mut jobs: Vec<(SystemTime, Job)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let id = entry.file_name().to_str()?.to_owned();
                is_uuid(&id).then(|| self.load(&id, entry.path()))
            })
            .filter_map(|job| match job {
                Ok(job) => job,
                Err(e) => {
                    tracing::error!("reading an activation: {e:#}");
                    None
                }
            })
            .collect();
        jobs.sort_by_key(|(created, _)| *created);
        jobs.into_iter().map(|(_, job)| job).collect()
    }

    fn load(&self, id: &str, dir: PathBuf) -> anyhow::Result<Option<(SystemTime, Job)>> {
        let request_path = dir.join(REQUEST);
        let created = match std::fs::metadata(&request_path) {
            Ok(meta) => meta.modified()?,
            // Being created right now.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", request_path.display())),
        };
        let mut report = read_json::<ActivationReport>(&dir.join(REPORT))?;
        let status = read_json::<Status>(&dir.join(STATUS))?;
        if report.is_none() && !unit_is_active(id) && age(created) > START_GRACE {
            let request: Activate = read_json(&request_path)?.context("missing request")?;
            let interrupted = ActivationReport {
                system_changed: true,
                ..failed_report(
                    &request,
                    "the activation was interrupted (the machine restarted or the job \
                     crashed); check the machine's current system"
                        .to_owned(),
                )
            };
            tracing::error!("activation {id} was interrupted");
            write_atomically(&dir.join(REPORT), &serde_json::to_vec_pretty(&interrupted)?)?;
            report = Some(interrupted);
        }
        let since = status
            .as_ref()
            .and_then(|s| humantime::parse_rfc3339(&s.since).ok());
        let server = std::fs::read_to_string(dir.join(SERVER)).ok();
        Ok(Some((
            created,
            Job {
                deployment_id: id.to_owned(),
                server,
                dir,
                phase: status.map(|s| s.phase),
                since,
                report,
            },
        )))
    }

    /// Tells a waiting job that the server confirmed it.
    pub fn confirm(&self, deployment_id: &str) -> anyhow::Result<()> {
        let path = self.dir(deployment_id)?.join(CONFIRMED);
        std::fs::write(&path, deployment_id).with_context(|| format!("writing {}", path.display()))
    }

    /// Forgets a finished job.
    pub fn remove(&self, deployment_id: &str) -> anyhow::Result<()> {
        let dir = self.dir(deployment_id)?;
        unload(deployment_id);
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", dir.display()))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn unit_name(deployment_id: &str) -> String {
    format!("mnix-activation-{deployment_id}.service")
}

/// Starts `mnix-agent activation-job <dir>` in its own transient unit, so
/// that it is not part of (and not stopped with) the agent service.
#[cfg(not(target_os = "macos"))]
fn spawn(dir: &Path, deployment_id: &str) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("finding the agent executable")?;
    let log = dir.join(LOG);
    let log = log.to_str().context("non-UTF-8 state directory")?;
    let status = Command::new("systemd-run")
        .arg(format!("--unit={}", unit_name(deployment_id)))
        .arg(format!(
            "--description=manage.nix activation for deployment {deployment_id}"
        ))
        .args([
            "--service-type=exec",
            "--collect",
            "--no-ask-password",
            "--quiet",
        ])
        .arg(format!("--property=StandardOutput=append:{log}"))
        .arg(format!("--property=StandardError=append:{log}"))
        // Like nixos-rebuild, pass on what switch-to-configuration needs.
        .args([
            "--setenv=PATH",
            "--setenv=LOCALE_ARCHIVE",
            "--setenv=RUST_LOG",
        ])
        .arg("--")
        .arg(exe)
        .arg("activation-job")
        .arg(dir)
        .stdin(Stdio::null())
        .status()
        .context("running systemd-run")?;
    anyhow::ensure!(status.success(), "systemd-run failed ({status})");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn unit_is_active(deployment_id: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", &unit_name(deployment_id)])
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Transient units are collected when they finish.
#[cfg(not(target_os = "macos"))]
fn unload(_deployment_id: &str) {}

#[cfg(target_os = "macos")]
fn label(deployment_id: &str) -> String {
    format!("org.manage-nix.activation.{deployment_id}")
}

/// Starts `mnix-agent activation-job <dir>` as its own launchd daemon,
/// from a property list in the job directory, so that it is not stopped
/// when activating a system reloads the agent's daemon.
#[cfg(target_os = "macos")]
fn spawn(dir: &Path, deployment_id: &str) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("finding the agent executable")?;
    let plist = dir.join("job.plist");
    write_atomically(&plist, launchd_plist(&exe, dir, deployment_id)?.as_bytes())?;
    let status = Command::new("launchctl")
        .arg("bootstrap")
        .arg("system")
        .arg(&plist)
        .stdin(Stdio::null())
        .status()
        .context("running launchctl")?;
    anyhow::ensure!(status.success(), "launchctl bootstrap failed ({status})");
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn launchd_plist(exe: &Path, dir: &Path, deployment_id: &str) -> anyhow::Result<String> {
    fn escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    let string = |path: &Path| -> anyhow::Result<String> {
        Ok(format!(
            "<string>{}</string>",
            escape(path.to_str().context("non-UTF-8 path")?)
        ))
    };
    let log = string(&dir.join(LOG))?;
    let mut env = String::new();
    for name in ["PATH", "RUST_LOG"] {
        if let Ok(value) = std::env::var(name) {
            env.push_str(&format!(
                "<key>{name}</key><string>{}</string>",
                escape(&value)
            ));
        }
    }
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
<key>Label</key><string>org.manage-nix.activation.{deployment_id}</string>
<key>ProgramArguments</key><array>{}<string>activation-job</string>{}</array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><false/>
<key>LaunchOnlyOnce</key><true/>
<key>StandardOutPath</key>{log}
<key>StandardErrorPath</key>{log}
<key>EnvironmentVariables</key><dict>{env}</dict>
</dict>
</plist>
"#,
        string(exe)?,
        string(dir)?,
    ))
}

#[cfg(target_os = "macos")]
fn unit_is_active(deployment_id: &str) -> bool {
    Command::new("launchctl")
        .arg("print")
        .arg(format!("system/{}", label(deployment_id)))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("state = running")
        })
}

/// Removes the job's finished launchd daemon.
#[cfg(target_os = "macos")]
fn unload(deployment_id: &str) {
    let _ = Command::new("launchctl")
        .arg("bootout")
        .arg(format!("system/{}", label(deployment_id)))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn age(t: SystemTime) -> Duration {
    SystemTime::now().duration_since(t).unwrap_or_default()
}

pub fn failed_report(request: &Activate, message: String) -> ActivationReport {
    ActivationReport {
        store_path: match &request.target {
            ActivationTarget::System { store_path } => store_path.clone(),
            ActivationTarget::Build { .. } | ActivationTarget::PreviousGeneration => String::new(),
        },
        mode: request.mode,
        status: ActivationStatus::Failed,
        system_changed: false,
        previous_system: None,
        message: Some(message),
        failed_checks: Vec::new(),
    }
}

/// `mnix-agent activation-job <dir>`: runs the activation in `dir` and
/// writes its report there.
pub fn run(dir: &Path) -> anyhow::Result<ActivationReport> {
    let request: Activate = read_json(&dir.join(REQUEST))?
        .with_context(|| format!("no {REQUEST} in {}", dir.display()))?;
    let _ = std::fs::remove_file(dir.join(CONFIRMED));
    let observer = JobObserver {
        dir,
        confirm_timeout: Duration::from_secs(request.confirm_timeout_secs),
    };
    let report = match crate::wait_for_lock() {
        Ok(_lock) => match prepare(dir, &request, &observer) {
            Ok(source) => activate::activate(
                &LocalHost,
                &Request {
                    target: request.target.clone(),
                    mode: request.mode,
                    signatures: request.signatures.clone(),
                    source,
                },
                &observer,
            ),
            Err(e) => {
                tracing::error!("{e:#}");
                failed_report(&request, format!("{e:#}"))
            }
        },
        Err(e) => failed_report(&request, format!("{e:#}")),
    };
    write_atomically(&dir.join(REPORT), &serde_json::to_vec_pretty(&report)?)?;
    Ok(report)
}

/// Fetches the source of a build from the server that asked for it.
fn prepare(
    dir: &Path,
    request: &Activate,
    observer: &JobObserver,
) -> anyhow::Result<Option<PathBuf>> {
    let ActivationTarget::Build { commit, .. } = &request.target else {
        return Ok(None);
    };
    observer.phase(ActivationPhase::Building);
    // <state dir>/activations/<id>
    let state_dir = dir
        .parent()
        .and_then(Path::parent)
        .context("unexpected job directory")?;
    let name = std::fs::read_to_string(dir.join(SERVER))
        .context("the job does not say which server asked for it")?;
    let config = AgentConfig::load_from(Path::new("/"))?.unwrap_or_default();
    let server = config
        .servers
        .iter()
        .find(|s| s.name == name)
        .with_context(|| format!("the server {name:?} is no longer configured"))?;
    anyhow::ensure!(
        server.allow_builds,
        "the server {name:?} may not make this machine build"
    );
    let credential = crate::credential::load(&crate::daemon::server_dir(state_dir, &name))?
        .with_context(|| format!("not enrolled with {name:?}"))?;
    let repo = crate::source::fetch(
        state_dir,
        server,
        &credential,
        &request.deployment_id,
        commit,
        dir,
    )?;
    Ok(Some(repo))
}

struct JobObserver<'a> {
    dir: &'a Path,
    confirm_timeout: Duration,
}

impl JobObserver<'_> {
    fn set_phase(&self, phase: ActivationPhase) -> anyhow::Result<()> {
        let status = Status {
            phase,
            since: humantime::format_rfc3339_micros(SystemTime::now()).to_string(),
        };
        write_atomically(&self.dir.join(STATUS), &serde_json::to_vec(&status)?)
    }
}

impl Observer for JobObserver<'_> {
    fn phase(&self, phase: ActivationPhase) {
        tracing::info!("phase: {}", phase.as_str());
        if let Err(e) = self.set_phase(phase) {
            tracing::error!("recording the phase: {e:#}");
        }
    }

    /// Waits for the service to write `confirmed`, which it does only after
    /// the server answered over a connection opened after this point.
    fn confirm(&self) -> Result<(), String> {
        self.phase(ActivationPhase::AwaitingConfirmation);
        tracing::info!(
            "waiting up to {} for the server to confirm",
            humantime::format_duration(self.confirm_timeout)
        );
        let deadline = std::time::Instant::now() + self.confirm_timeout;
        let confirmed = self.dir.join(CONFIRMED);
        loop {
            if confirmed.exists() {
                tracing::info!("the server confirmed the new system");
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "the machine did not reach the server within {} of activating \
                     (no confirmation over a new connection)",
                    humantime::format_duration(self.confirm_timeout)
                ));
            }
            std::thread::sleep(CONFIRM_POLL);
        }
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid {}", path.display()))
            .map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_atomically(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// A hyphenated UUID, which keeps server-chosen IDs safe as file names.
fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0b5c3a0e-6f1e-4d2a-9c1b-2f3e4d5c6b7a";

    fn request(id: &str) -> Activate {
        Activate {
            deployment_id: id.to_owned(),
            target: ActivationTarget::System {
                store_path: "/nix/store/00000000000000000000000000000000-nixos-system".to_owned(),
            },
            mode: mnix_protocol::ActivationMode::Switch,
            signatures: vec![],
            confirm_timeout_secs: 1,
        }
    }

    #[test]
    fn deployment_ids_must_be_uuids() {
        assert!(is_uuid(ID));
        for bad in [
            "",
            "../../etc",
            "0b5c3a0e-6f1e-4d2a-9c1b-2f3e4d5c6b7",
            &ID.replace('-', "x"),
        ] {
            assert!(!is_uuid(bad), "{bad}");
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(Jobs::new(dir.path())
            .start(&request("../escape"), "s")
            .is_err());
    }

    #[test]
    fn jobs_are_listed_with_their_phase_and_report() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = Jobs::new(dir.path());
        assert!(jobs.list().is_empty());
        // `systemd-run` is missing here, so the job fails to start and is
        // listed with a report saying so.
        assert!(jobs.start(&request(ID), "s").unwrap());
        assert!(!jobs.start(&request(ID), "s").unwrap(), "started twice");
        let listed = jobs.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].server.as_deref(), Some("s"));
        let report = listed[0].report.as_ref().unwrap();
        assert_eq!(report.status, ActivationStatus::Failed);
        assert!(report.message.as_ref().unwrap().contains("could not start"));

        let observer = JobObserver {
            dir: &listed[0].dir,
            confirm_timeout: Duration::from_millis(1),
        };
        observer.phase(ActivationPhase::CheckingHealth);
        assert!(observer.confirm().is_err());
        let job = &jobs.list()[0];
        assert_eq!(job.phase, Some(ActivationPhase::AwaitingConfirmation));
        assert!(job.since.is_some());
        // It has a report, so it is not waiting any more.
        assert!(job.awaiting_since().is_none());

        jobs.confirm(ID).unwrap();
        assert!(observer.confirm().is_ok());
        jobs.remove(ID).unwrap();
        jobs.remove(ID).unwrap();
        assert!(jobs.list().is_empty());
    }

    #[test]
    fn launchd_jobs_run_once_and_log_to_the_job() {
        let plist = launchd_plist(
            Path::new("/nix/store/x-mnix-agent/bin/mnix-agent"),
            Path::new("/var/lib/manage-nix/activations/a&b"),
            ID,
        )
        .unwrap();
        assert!(plist.contains(&format!("<string>org.manage-nix.activation.{ID}</string>")));
        assert!(plist.contains("<key>KeepAlive</key><false/>"));
        assert!(plist.contains("<string>/var/lib/manage-nix/activations/a&amp;b/log</string>"));
    }
}
