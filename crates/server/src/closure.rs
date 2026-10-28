//! Comparing two systems' closures with `nix store diff-closures`, for the
//! machine page's generation diff.
//!
//! The server has every system it built. Others (a machine's original
//! system, generations from `nixos-rebuild`) it tries to substitute from its
//! Nix daemon's substituters first; if that fails there is no diff.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use mnix_nix::StorePath;
use tokio::process::Command;

/// How long fetching one system, or computing the diff, may take.
const TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum DiffError {
    #[error("the server does not have {0} and could not fetch it: {1}")]
    Unavailable(String, String),
    #[error("nix store diff-closures failed: {0}")]
    Failed(String),
}

pub async fn diff(from: &StorePath, to: &StorePath) -> Result<String, DiffError> {
    for path in [from, to] {
        ensure_present(path).await?;
    }
    let output = run(Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "store",
            "diff-closures",
        ])
        .arg(from.as_str())
        .arg(to.as_str()))
    .await
    .map_err(DiffError::Failed)?;
    Ok(strip_ansi(&output))
}

/// Substitutes `path` if it is not in the store.
async fn ensure_present(path: &StorePath) -> Result<(), DiffError> {
    if Path::new(path.as_str()).exists() {
        return Ok(());
    }
    tracing::info!("fetching {path} to compare it");
    run(Command::new("nix-store")
        .args(["--realise", "--"])
        .arg(path.as_str()))
    .await
    .map(drop)
    .map_err(|e| DiffError::Unavailable(path.to_string(), e))
}

/// Runs a command with [`TIMEOUT`] and returns its stdout, or its stderr
/// as the error.
async fn run(command: &mut Command) -> Result<String, String> {
    let child = command
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let output = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("timed out after {}", humantime::format_duration(TIMEOUT)))?
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last = stderr.trim().lines().last().unwrap_or("").trim();
        Err(format!("{} ({})", strip_ansi(last), output.status))
    }
}

/// Removes ANSI escape sequences (`ESC [ ... letter`), which Nix may print
/// even when told not to.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colors() {
        assert_eq!(
            strip_ansi("\u{1b}[1mfoo\u{1b}[0m: 1.0 → \u{1b}[32;1m1.1\u{1b}[0m"),
            "foo: 1.0 → 1.1"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
