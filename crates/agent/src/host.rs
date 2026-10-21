//! [`Host`] for a real NixOS machine, using the Nix and systemd command-line
//! tools. Only fixed commands with validated store paths as arguments are
//! run; nothing goes through a shell.

use std::ffi::OsStr;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;

use crate::activate::{Host, ProfileGeneration};
use crate::config::{AgentConfig, HealthChecks};
use crate::health;
use crate::signature::PathInfo;
use crate::store_path::StorePath;

pub const SYSTEM_PROFILE: &str = "/nix/var/nix/profiles/system";
pub const CURRENT_SYSTEM: &str = "/run/current-system";
pub const BOOTED_SYSTEM: &str = "/run/booted-system";

pub struct NixosHost;

impl Host for NixosHost {
    fn realise(&self, path: &StorePath) -> anyhow::Result<()> {
        run(Command::new("nix-store")
            .arg("--realise")
            .arg(path.as_str()))
        .map(drop)
    }

    fn path_info(&self, path: &StorePath) -> anyhow::Result<PathInfo> {
        let query =
            |flag: &str| run(Command::new("nix-store").args(["--query", flag, path.as_str()]));
        let nar_size = query("--size")?;
        // `nix path-info --json` has changed format between Nix releases;
        // the plain-text output and `nix-store --query` have not.
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

    fn is_nixos_system(&self, path: &StorePath) -> bool {
        switch_to_configuration_path(path).is_file()
    }

    fn agent_config(&self, root: &Path) -> anyhow::Result<Option<AgentConfig>> {
        AgentConfig::load_from(root)
    }

    fn current_system(&self) -> anyhow::Result<StorePath> {
        StorePath::resolve(Path::new(CURRENT_SYSTEM))
    }

    fn profile_generation(&self) -> anyhow::Result<Option<ProfileGeneration>> {
        let profile = Path::new(SYSTEM_PROFILE);
        let link = match std::fs::read_link(profile) {
            Ok(link) => link,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context(SYSTEM_PROFILE),
        };
        let number = link
            .to_str()
            .and_then(|link| generation_number("system", link))
            .with_context(|| format!("unexpected profile link {}", link.display()))?;
        Ok(Some((number, StorePath::resolve(profile)?)))
    }

    fn set_profile(&self, path: &StorePath) -> anyhow::Result<()> {
        run(Command::new("nix-env")
            .args(["--profile", SYSTEM_PROFILE, "--set"])
            .arg(path.as_str()))
        .map(drop)
    }

    fn switch_profile_generation(&self, number: u64) -> anyhow::Result<()> {
        run(Command::new("nix-env")
            .args(["--profile", SYSTEM_PROFILE, "--switch-generation"])
            .arg(number.to_string()))
        .map(drop)
    }

    fn switch_to_configuration(&self, system: &StorePath, action: &str) -> anyhow::Result<()> {
        tracing::info!(
            "{} {action}",
            switch_to_configuration_path(system).display()
        );
        // Like nixos-rebuild, run it in its own transient unit so that it
        // finishes even if the agent is stopped or restarted by the switch.
        // Its output goes to our stderr; stdout is reserved for the report.
        let stderr = std::io::stderr().as_fd().try_clone_to_owned()?;
        let status = Command::new("systemd-run")
            .args(["-E", "LOCALE_ARCHIVE", "-E", "NIXOS_INSTALL_BOOTLOADER"])
            .args(["--collect", "--no-ask-password", "--pipe", "--quiet"])
            .args([
                "--service-type=exec",
                "--unit=mnix-switch-to-configuration",
                "--",
            ])
            .arg(switch_to_configuration_path(system))
            .arg(action)
            .stdin(Stdio::null())
            .stdout(stderr)
            .status()
            .context("running systemd-run")?;
        anyhow::ensure!(
            status.success(),
            "switch-to-configuration {action} of {system} failed ({status})"
        );
        Ok(())
    }

    fn wait_until_healthy(&self, checks: &HealthChecks) -> Result<(), Vec<String>> {
        health::wait_until_healthy(checks.timeout(), checks.interval(), || {
            health::run_round(checks)
        })
    }
}

fn switch_to_configuration_path(system: &StorePath) -> PathBuf {
    system.as_path().join("bin/switch-to-configuration")
}

/// Parses a generation link name such as `system-42-link` (for the profile
/// `system`) into 42.
pub fn generation_number(profile_name: &str, link_name: &str) -> Option<u64> {
    link_name
        .strip_prefix(profile_name)?
        .strip_prefix('-')?
        .strip_suffix("-link")?
        .parse()
        .ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_generation_links() {
        assert_eq!(generation_number("system", "system-42-link"), Some(42));
        assert_eq!(generation_number("system", "system-link"), None);
        assert_eq!(generation_number("system", "system-x-link"), None);
        assert_eq!(generation_number("system", "other-1-link"), None);
        assert_eq!(generation_number("system", "system-profiles-1-link"), None);
    }
}
