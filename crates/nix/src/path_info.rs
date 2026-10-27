//! Reading a store path's signed metadata from the local Nix store.

use std::ffi::OsStr;
use std::process::{Command, Stdio};

use anyhow::Context;

use crate::{PathInfo, StorePath};

/// Reads what Nix signs for `path`, and the signatures it already has, with
/// the Nix command-line tools. `path` must be valid in the local store.
///
/// `nix path-info --json` has changed format between Nix releases; the
/// plain-text output and `nix-store --query` have not.
pub fn query_path_info(path: &StorePath) -> anyhow::Result<PathInfo> {
    let query = |flag: &str| run(Command::new("nix-store").args(["--query", flag, path.as_str()]));
    let nar_size = query("--size")?;
    let sigs = run(Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "path-info",
            "--sigs",
        ])
        .arg(path.as_str()))?;
    Ok(PathInfo {
        path: path.to_string(),
        nar_hash: query("--hash")?.trim().to_owned(),
        nar_size: nar_size
            .trim()
            .parse()
            .with_context(|| format!("unexpected NAR size {nar_size:?}"))?,
        references: query("--references")?.lines().map(str::to_owned).collect(),
        signatures: sigs.split_whitespace().skip(1).map(str::to_owned).collect(),
    })
}

/// Runs a command and returns its stdout, failing with its stderr.
fn run(command: &mut Command) -> anyhow::Result<String> {
    let description = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(OsStr::to_string_lossy)
        .collect::<Vec<_>>()
        .join(" ");
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {description}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{description} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).with_context(|| format!("{description} printed invalid UTF-8"))
}
