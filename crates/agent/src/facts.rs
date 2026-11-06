//! Collecting [`Facts`] about the running machine.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
        services: Some(crate::services::collect()),
        flatpak_updates: flatpak_updates(),
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

/// When Flatpak was last checked, the refs with updates (`None` if
/// unknown), and whether a check is running.
type FlatpakCache = (Option<Instant>, Option<Vec<String>>, bool);
static FLATPAK_CACHE: Mutex<FlatpakCache> = Mutex::new((None, None, false));

// Network checks run off the heartbeat path. A missing binary, timeout, or
// failed remote stays unknown; a successful empty list means up to date.
fn flatpak_updates() -> Option<Vec<String>> {
    let mut cache = FLATPAK_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if !cache.2
        && cache
            .0
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(900))
    {
        cache.2 = true;
        std::thread::spawn(|| {
            let result = collect_flatpak_updates();
            let mut cache = FLATPAK_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            *cache = (Some(Instant::now()), result, false);
        });
    }
    cache.1.clone()
}

fn parse_flatpak_updates(output: &str) -> Vec<String> {
    let mut refs: Vec<_> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    refs.sort();
    refs.dedup();
    refs
}

/// `flatpak` for the system installation (`user` `None`), or for a user's
/// own installation, run as that user, within `timeout`.
fn flatpak(user: Option<&str>, timeout: &str) -> Command {
    let mut command = Command::new("timeout");
    command.arg(timeout);
    if let Some(user) = user {
        command.args(["runuser", "-u", user, "--", "flatpak", "--user"]);
    } else {
        command.args(["flatpak", "--system"]);
    }
    command.env("LC_ALL", "C").stdin(Stdio::null());
    command
}

/// Users with their own Flatpak installation in a standard home directory.
fn flatpak_users() -> Option<Vec<String>> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    Some(
        passwd
            .lines()
            .map(|line| line.split(':').collect::<Vec<_>>())
            .filter(|fields| fields.len() >= 7)
            .filter(|fields| {
                Path::new(fields[5])
                    .join(".local/share/flatpak/repo")
                    .is_dir()
            })
            .map(|fields| fields[0].to_owned())
            .collect(),
    )
}

/// Forgets the last Flatpak check, so the next facts start a new one.
pub fn refresh_flatpak_updates() {
    let mut cache = FLATPAK_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.0 = None;
}

/// Updates the system installation's apps and runtimes, then each user's.
/// Output goes to the job's log.
pub fn update_flatpaks() -> anyhow::Result<()> {
    let run = |user: Option<&str>| -> anyhow::Result<()> {
        let who = user.map_or("the system installation".to_owned(), |u| {
            format!("{u}'s installation")
        });
        println!("updating Flatpaks in {who}");
        let status = flatpak(user, "3600")
            .args(["update", "--noninteractive", "--assumeyes"])
            .status()
            .with_context(|| format!("running flatpak for {who}"))?;
        anyhow::ensure!(
            status.success(),
            "flatpak update failed for {who} ({status})"
        );
        Ok(())
    };
    let mut failed = Vec::new();
    if let Err(e) = run(None) {
        eprintln!("{e:#}");
        failed.push(e.to_string());
    }
    for user in flatpak_users().unwrap_or_default() {
        if let Err(e) = run(Some(&user)) {
            eprintln!("{e:#}");
            failed.push(e.to_string());
        }
    }
    anyhow::ensure!(failed.is_empty(), "{}", failed.join("; "));
    Ok(())
}

fn collect_flatpak_updates() -> Option<Vec<String>> {
    let query = |user: Option<&str>| -> Option<Vec<String>> {
        let out = flatpak(user, "30")
            .args(["remote-ls", "--updates", "--columns=ref"])
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        Some(parse_flatpak_updates(&String::from_utf8_lossy(&out.stdout)))
    };
    let mut updates = query(None)?;
    // The root service must query user installations as their owner.
    for user in flatpak_users()? {
        updates.extend(
            query(Some(&user))?
                .into_iter()
                .map(|reference| format!("{user}: {reference}")),
        );
    }
    Some(updates)
}

#[cfg(test)]
mod tests {
    #[test]
    fn flatpak_refs_are_sorted_and_deduplicated() {
        assert_eq!(
            super::parse_flatpak_updates("app/b/x/stable\n\napp/a/x/stable\napp/b/x/stable\n"),
            vec!["app/a/x/stable", "app/b/x/stable"]
        );
    }

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
