//! Collecting [`Facts`] about the running machine.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::Context;
use mnix_protocol::{Facts, Generation, Os};

use crate::host::{generation_number, BOOTED_SYSTEM, CURRENT_SYSTEM, OS, SYSTEM_PROFILE};
use mnix_nix::StorePath;

pub fn collect() -> anyhow::Result<Facts> {
    let current = StorePath::resolve(Path::new(CURRENT_SYSTEM))?;
    let booted = match OS {
        Os::Nixos => StorePath::resolve(Path::new(BOOTED_SYSTEM)).ok(),
        Os::Darwin => None,
    };
    let version_file = match OS {
        Os::Nixos => "nixos-version",
        Os::Darwin => "darwin-version",
    };
    Ok(Facts {
        hostname: hostname()?,
        os: OS,
        kernel: kernel()?,
        os_version: read_trimmed(current.as_path().join(version_file)).ok(),
        configuration_revision: configuration_revision(&current),
        reboot_required: booted
            .as_ref()
            .is_some_and(|booted| reboot_required(booted.as_path(), current.as_path())),
        current_system: current.to_string(),
        booted_system: booted.map(|b| b.to_string()),
        generations: generations(Path::new(SYSTEM_PROFILE))?,
    })
}

#[cfg(not(target_os = "macos"))]
fn hostname() -> anyhow::Result<String> {
    read_trimmed("/proc/sys/kernel/hostname")
}

#[cfg(target_os = "macos")]
fn hostname() -> anyhow::Result<String> {
    output(Command::new("/bin/hostname").arg("-s"))
}

#[cfg(not(target_os = "macos"))]
fn kernel() -> anyhow::Result<String> {
    read_trimmed("/proc/sys/kernel/osrelease")
}

#[cfg(target_os = "macos")]
fn kernel() -> anyhow::Result<String> {
    output(Command::new("/usr/bin/uname").arg("-r"))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn output(command: &mut Command) -> anyhow::Result<String> {
    let out = command.stdin(Stdio::null()).output()?;
    anyhow::ensure!(out.status.success(), "{:?} failed", command.get_program());
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The system's `configurationRevision`, from `nixos-version --json` or
/// `darwin-version --json`. Asked once per system: facts are collected at
/// every heartbeat.
fn configuration_revision(system: &StorePath) -> Option<String> {
    static CACHE: Mutex<Option<(String, Option<String>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((path, revision)) = cache.as_ref() {
        if path == system.as_str() {
            return revision.clone();
        }
    }
    let program = system.as_path().join("sw/bin").join(match OS {
        Os::Nixos => "nixos-version",
        Os::Darwin => "darwin-version",
    });
    let revision = Command::new(program)
        .arg("--json")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| parse_revision(&out.stdout));
    *cache = Some((system.to_string(), revision.clone()));
    revision
}

fn parse_revision(json: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(json).ok()?;
    let revision = value.get("configurationRevision")?.as_str()?.trim();
    (!revision.is_empty()).then(|| revision.to_owned())
}

fn read_trimmed(path: impl AsRef<Path>) -> anyhow::Result<String> {
    let path = path.as_ref();
    Ok(std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?
        .trim()
        .to_owned())
}

/// The same test NixOS uses: a reboot is needed when the kernel, initrd, or
/// kernel modules of the booted and current systems differ.
pub fn reboot_required(booted: &Path, current: &Path) -> bool {
    ["kernel", "initrd", "kernel-modules"].iter().any(|part| {
        let resolve = |system: &Path| std::fs::canonicalize(system.join(part)).ok();
        resolve(booted) != resolve(current)
    })
}

/// Lists `<profile>-<n>-link` generations next to `profile`, oldest first.
pub fn generations(profile: &Path) -> anyhow::Result<Vec<Generation>> {
    let (Some(dir), Some(name)) = (
        profile.parent(),
        profile.file_name().and_then(|n| n.to_str()),
    ) else {
        anyhow::bail!("invalid profile path {}", profile.display());
    };
    let current = std::fs::read_link(profile)
        .ok()
        .and_then(|link| link.to_str().and_then(|link| generation_number(name, link)));
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    let mut generations = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(number) = file_name.to_str().and_then(|f| generation_number(name, f)) else {
            continue;
        };
        let Ok(store_path) = StorePath::resolve(&entry.path()) else {
            continue;
        };
        let created = std::fs::symlink_metadata(entry.path())?.modified()?;
        generations.push(Generation {
            number,
            store_path: store_path.to_string(),
            created_at: humantime::format_rfc3339_seconds(created).to_string(),
            current: current == Some(number),
        });
    }
    generations.sort_by_key(|g| g.number);
    Ok(generations)
}

#[cfg(test)]
mod tests {
    #[test]
    fn revisions_come_from_the_version_json() {
        use super::parse_revision;
        assert_eq!(
            parse_revision(br#"{"nixosVersion":"26.11","configurationRevision":"abc"}"#),
            Some("abc".to_owned())
        );
        assert_eq!(parse_revision(br#"{"darwinLabel":"x"}"#), None);
        assert_eq!(parse_revision(br#"{"configurationRevision":""}"#), None);
        assert_eq!(parse_revision(b"not json"), None);
    }
}
