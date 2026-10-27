//! Collecting [`Facts`] about the running machine.

use std::path::Path;

use anyhow::Context;
use mnix_protocol::{Facts, Generation};

use crate::host::{generation_number, BOOTED_SYSTEM, CURRENT_SYSTEM, SYSTEM_PROFILE};
use mnix_nix::StorePath;

pub fn collect() -> anyhow::Result<Facts> {
    let current = StorePath::resolve(Path::new(CURRENT_SYSTEM))?;
    let booted = StorePath::resolve(Path::new(BOOTED_SYSTEM)).ok();
    Ok(Facts {
        hostname: read_trimmed("/proc/sys/kernel/hostname")?,
        kernel: read_trimmed("/proc/sys/kernel/osrelease")?,
        nixos_version: read_trimmed(current.as_path().join("nixos-version")).ok(),
        reboot_required: booted
            .as_ref()
            .is_some_and(|booted| reboot_required(booted.as_path(), current.as_path())),
        current_system: current.to_string(),
        booted_system: booted.map(|b| b.to_string()),
        generations: generations(Path::new(SYSTEM_PROFILE))?,
    })
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
