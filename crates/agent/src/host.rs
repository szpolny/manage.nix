//! [`Host`] for the machine the agent runs on, NixOS or nix-darwin, using
//! the Nix, systemd, and launchd command-line tools. Only fixed commands
//! with validated store paths as arguments are run; nothing goes through a
//! shell.

use std::ffi::OsStr;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;
use mnix_nix::{PathInfo, StorePath};
use mnix_protocol::{ActivationMode, Os};

use crate::activate::{Host, ProfileGeneration};
use crate::config::{AgentConfig, HealthChecks};
use crate::health;

pub const SYSTEM_PROFILE: &str = "/nix/var/nix/profiles/system";
pub const CURRENT_SYSTEM: &str = "/run/current-system";
/// NixOS only; nix-darwin has no booted system apart from the current one.
pub const BOOTED_SYSTEM: &str = "/run/booted-system";

/// The system this agent was built for.
pub const OS: Os = if cfg!(target_os = "macos") {
    Os::Darwin
} else {
    Os::Nixos
};

pub struct LocalHost;

impl Host for LocalHost {
    fn os(&self) -> Os {
        OS
    }

    fn realise(&self, path: &StorePath) -> anyhow::Result<()> {
        run(Command::new("nix-store")
            .arg("--realise")
            .arg(path.as_str()))
        .map(drop)
    }

    fn build(&self, installable: &str) -> anyhow::Result<StorePath> {
        tracing::info!("building {installable}");
        // The build log goes to our stderr (the activation log); stdout is
        // the result.
        let stderr = std::io::stderr().as_fd().try_clone_to_owned()?;
        let output = Command::new("nix")
            .args([
                "--extra-experimental-features",
                "nix-command flakes",
                "build",
                "--print-build-logs",
                "--print-out-paths",
                "--no-link",
                "--no-write-lock-file",
                "--",
            ])
            .arg(installable)
            .stdin(Stdio::null())
            .stderr(stderr)
            .output()
            .context("running nix build")?;
        anyhow::ensure!(
            output.status.success(),
            "nix build failed ({})",
            output.status
        );
        let out = String::from_utf8_lossy(&output.stdout);
        let path = out
            .lines()
            .last()
            .map(str::trim)
            .context("nix build printed no store path")?;
        StorePath::parse(path)
    }

    fn path_info(&self, path: &StorePath) -> anyhow::Result<PathInfo> {
        mnix_nix::query_path_info(path)
    }

    fn is_system(&self, path: &StorePath) -> bool {
        match OS {
            Os::Nixos => switch_to_configuration_path(path).is_file(),
            Os::Darwin => {
                path.as_path().join("activate").is_file()
                    && path.as_path().join("darwin-version").is_file()
            }
        }
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

    fn profile_generations(&self) -> anyhow::Result<Vec<ProfileGeneration>> {
        crate::facts::generations(Path::new(SYSTEM_PROFILE))?
            .into_iter()
            .map(|g| Ok((g.number, StorePath::parse(&g.store_path)?)))
            .collect()
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
        match OS {
            Os::Nixos => nixos_switch(system, action),
            Os::Darwin => darwin_activate(system, action),
        }
    }

    fn wait_until_healthy(&self, checks: &HealthChecks) -> Result<(), Vec<String>> {
        health::wait_until_healthy(checks.timeout(), checks.interval(), || {
            health::run_round(checks)
        })
    }
}

/// Whether `mode` can be done on this kind of system.
pub fn supports(os: Os, mode: ActivationMode) -> bool {
    os == Os::Nixos || mode != ActivationMode::Boot
}

fn nixos_switch(system: &StorePath, action: &str) -> anyhow::Result<()> {
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

/// nix-darwin's equivalent of `switch-to-configuration`: `<system>/activate`
/// makes it the running system (`switch` and `test`, which differ only in
/// the profile, set beforehand). There is no boot default to install, so
/// `boot` does nothing; it is used only to restore a profile generation.
fn darwin_activate(system: &StorePath, action: &str) -> anyhow::Result<()> {
    match action {
        "boot" => Ok(()),
        "switch" | "test" => {
            let activate = system.as_path().join("activate");
            tracing::info!("{}", activate.display());
            // Already outside the agent's launchd job (see `crate::job`).
            let stderr = std::io::stderr().as_fd().try_clone_to_owned()?;
            let status = Command::new(&activate)
                .stdin(Stdio::null())
                .stdout(stderr)
                .status()
                .with_context(|| format!("running {}", activate.display()))?;
            anyhow::ensure!(status.success(), "activating {system} failed ({status})");
            Ok(())
        }
        other => anyhow::bail!("nix-darwin cannot {other} a system"),
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
