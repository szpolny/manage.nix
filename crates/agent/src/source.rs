//! The source of [`mnix_protocol::ActivationTarget::Build`] targets.
//!
//! The agent keeps a bare Git repository per server in
//! `<state dir>/sources/<server>.git`. Before a build it downloads a bundle
//! of the deployment's commit from the server that asked for it, leaving out
//! the history of commits it already has, and fetches it into that
//! repository. The build then reads the commit through [`LOCAL_REF`].
//!
//! Only the activation job touches these repositories, while it holds the
//! machine's activation lock.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Context;

use crate::config::ServerConfig;
use crate::credential::Credential;

/// The ref the build reads its commit from.
pub const LOCAL_REF: &str = "refs/mnix/build";
/// Refs to recent commits, offered to the server as what the agent has.
const KEEP_REFS: &str = "refs/mnix/keep";
/// How many recent commits are kept.
const KEEP: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a download may take; repositories are small, links slow.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub fn repository(state_dir: &Path, server: &str) -> PathBuf {
    state_dir.join("sources").join(format!("{server}.git"))
}

/// Makes `commit` available at [`LOCAL_REF`] in the server's repository,
/// downloading what is missing. Returns the repository.
pub fn fetch(
    state_dir: &Path,
    server: &ServerConfig,
    credential: &Credential,
    deployment_id: &str,
    commit: &str,
    scratch: &Path,
) -> anyhow::Result<PathBuf> {
    let repo = repository(state_dir, &server.name);
    if !repo.join("HEAD").is_file() {
        std::fs::create_dir_all(&repo).with_context(|| format!("creating {}", repo.display()))?;
        git(&repo, &["init", "--quiet", "--bare"])?;
    }
    if has_commit(&repo, commit) {
        tracing::info!("already have commit {commit}");
    } else {
        let haves = recent_commits(&repo)?;
        let bundle = scratch.join("source.bundle");
        download(server, credential, deployment_id, &haves, &bundle)?;
        let refspec = format!("+{}:{KEEP_REFS}/{commit}", mnix_protocol::SOURCE_BUNDLE_REF);
        let fetched = git(
            &repo,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                bundle.to_str().context("non-UTF-8 path")?,
                &refspec,
            ],
        );
        let _ = std::fs::remove_file(&bundle);
        fetched.context("fetching the source bundle")?;
        anyhow::ensure!(
            has_commit(&repo, commit),
            "the server's source does not contain commit {commit}"
        );
    }
    git(
        &repo,
        &["update-ref", &format!("{KEEP_REFS}/{commit}"), commit],
    )?;
    git(&repo, &["update-ref", LOCAL_REF, commit])?;
    prune(&repo)?;
    Ok(repo)
}

fn has_commit(repo: &Path, commit: &str) -> bool {
    git(repo, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_ok()
}

/// The newest kept commits, newest first.
fn recent_commits(repo: &Path) -> anyhow::Result<Vec<String>> {
    let out = git(
        repo,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(objectname)",
            KEEP_REFS,
        ],
    )?;
    Ok(out.lines().map(str::to_owned).collect())
}

/// Forgets all but the [`KEEP`] newest commits. Git's garbage collection
/// removes their objects eventually.
fn prune(repo: &Path) -> anyhow::Result<()> {
    for commit in recent_commits(repo)?.iter().skip(KEEP) {
        git(
            repo,
            &["update-ref", "-d", &format!("{KEEP_REFS}/{commit}")],
        )?;
    }
    Ok(())
}

fn download(
    server: &ServerConfig,
    credential: &Credential,
    deployment_id: &str,
    haves: &[String],
    to: &Path,
) -> anyhow::Result<()> {
    use ureq::tls::{RootCerts, TlsConfig};
    let mut url = format!(
        "{}{}",
        server.url.trim_end_matches('/'),
        mnix_protocol::agent_source_path(deployment_id)
    );
    for (i, have) in haves.iter().take(KEEP).enumerate() {
        url.push(if i == 0 { '?' } else { '&' });
        url.push_str("have=");
        url.push_str(have);
    }
    tracing::info!("downloading the source from {}", server.url);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into();
    let mut response = agent
        .get(&url)
        .header(
            "Authorization",
            &format!("Bearer {}", credential.credential),
        )
        .call()
        .with_context(|| format!("downloading the source from {}", server.url))?;
    let mut file =
        std::fs::File::create(to).with_context(|| format!("creating {}", to.display()))?;
    let bytes = std::io::copy(&mut response.body_mut().as_reader(), &mut file)
        .context("downloading the source")?;
    file.flush()?;
    tracing::info!("downloaded {} KiB", bytes / 1024);
    Ok(())
}

/// Runs `git -C <repo> <args>`, returning stdout.
fn git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) -> String {
        git(dir, args).unwrap().trim().to_owned()
    }

    #[test]
    fn keeps_the_newest_commits() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        run(&work, &["init", "--quiet"]);
        let mut commits = Vec::new();
        for i in 0..KEEP + 2 {
            let status = Command::new("git")
                .arg("-C")
                .arg(&work)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["commit", "--quiet", "--allow-empty", "-m", &i.to_string()])
                .env("GIT_COMMITTER_DATE", format!("2026-01-01T00:00:{i:02}Z"))
                .status()
                .unwrap();
            assert!(status.success());
            let commit = run(&work, &["rev-parse", "HEAD"]);
            run(
                &work,
                &["update-ref", &format!("{KEEP_REFS}/{commit}"), &commit],
            );
            commits.push(commit);
        }
        assert!(has_commit(&work, &commits[0]));
        assert!(!has_commit(&work, &"0".repeat(40)));
        prune(&work).unwrap();
        let kept = recent_commits(&work).unwrap();
        assert_eq!(kept.len(), KEEP);
        assert_eq!(kept[0], commits[KEEP + 1]);
        assert!(!kept.contains(&commits[0]));
    }
}
