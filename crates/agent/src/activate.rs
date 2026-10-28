//! Activating a system safely: verify, switch, check health, confirm, roll
//! back.
//!
//! The steps talk to the machine only through [`Host`], so the decisions
//! (what to refuse, when to roll back, and how) are unit tested against a
//! fake machine. [`crate::host::LocalHost`] is the real implementation.
//! Progress and the final confirmation go through an [`Observer`]: the
//! command line has none, and an activation for the server waits for the
//! server's `confirm` (see [`crate::job`]).

use std::path::{Path, PathBuf};

use anyhow::Context;
use mnix_protocol::{
    ActivationMode, ActivationPhase, ActivationReport, ActivationStatus, ActivationTarget, Os,
};

use crate::config::{AgentConfig, HealthChecks};
use mnix_nix::{PathInfo, StorePath, TrustedKeys};

/// A generation of the system profile: its number and system.
pub type ProfileGeneration = (u64, StorePath);

/// Everything activation needs to do to the machine.
pub trait Host {
    fn os(&self) -> Os;
    /// Makes sure `path` is in the local store, substituting it if needed.
    fn realise(&self, path: &StorePath) -> anyhow::Result<()>;
    /// Builds a flake installable and returns its output.
    fn build(&self, installable: &str) -> anyhow::Result<StorePath>;
    fn path_info(&self, path: &StorePath) -> anyhow::Result<PathInfo>;
    /// Whether `path` is a system of this machine's kind ([`Host::os`])
    /// that can be activated.
    fn is_system(&self, path: &StorePath) -> bool;
    /// The agent configuration installed under `root` (`/` or a system).
    fn agent_config(&self, root: &Path) -> anyhow::Result<Option<AgentConfig>>;
    fn current_system(&self) -> anyhow::Result<StorePath>;
    /// The generation the system profile points at, if it exists.
    fn profile_generation(&self) -> anyhow::Result<Option<ProfileGeneration>>;
    /// Every generation of the system profile.
    fn profile_generations(&self) -> anyhow::Result<Vec<ProfileGeneration>>;
    fn set_profile(&self, path: &StorePath) -> anyhow::Result<()>;
    fn switch_profile_generation(&self, number: u64) -> anyhow::Result<()>;
    /// Runs `<system>/bin/switch-to-configuration <action>`.
    fn switch_to_configuration(&self, system: &StorePath, action: &str) -> anyhow::Result<()>;
    fn wait_until_healthy(&self, checks: &HealthChecks) -> Result<(), Vec<String>>;
}

/// What to activate and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub target: ActivationTarget,
    pub mode: ActivationMode,
    /// Signatures on the target obtained some other way than from Nix (the
    /// server sends them with the request).
    pub signatures: Vec<String>,
    /// The Git repository holding the commit of a
    /// [`ActivationTarget::Build`] target.
    pub source: Option<PathBuf>,
}

/// Follows an activation.
pub trait Observer {
    /// The activation moved on to `phase`.
    fn phase(&self, _phase: ActivationPhase) {}

    /// Called once a `switch` or `test` activation is healthy. An error
    /// rolls the machine back.
    fn confirm(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Nobody is watching: no progress, and nothing to wait for.
pub struct Unobserved;

impl Observer for Unobserved {}

/// The verified request and the state to restore on failure.
#[derive(Debug)]
struct Plan {
    target: StorePath,
    health_checks: HealthChecks,
    previous_system: StorePath,
    previous_generation: Option<ProfileGeneration>,
}

#[derive(Debug)]
struct Failure {
    message: String,
    failed_checks: Vec<String>,
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Self {
            message: format!("{e:#}"),
            failed_checks: Vec::new(),
        }
    }
}

pub fn activate(host: &dyn Host, request: &Request, observer: &dyn Observer) -> ActivationReport {
    let mode = request.mode;
    let mut report = ActivationReport {
        store_path: match &request.target {
            ActivationTarget::System { store_path } => store_path.clone(),
            ActivationTarget::Build { .. } | ActivationTarget::PreviousGeneration => String::new(),
        },
        mode,
        status: ActivationStatus::Failed,
        system_changed: false,
        previous_system: None,
        message: None,
        failed_checks: Vec::new(),
    };

    observer.phase(match request.target {
        ActivationTarget::Build { .. } => ActivationPhase::Building,
        _ => ActivationPhase::Activating,
    });
    let plan = match plan(host, request, observer) {
        Ok(plan) => plan,
        Err(e) => {
            tracing::error!("refusing to activate {}: {e:#}", describe(&request.target));
            report.message = Some(format!("refused: {e:#}"));
            return report;
        }
    };
    report.store_path = plan.target.to_string();
    report.previous_system = Some(plan.previous_system.to_string());

    let Err(failure) = apply(host, &plan, mode, observer) else {
        tracing::info!("activated {} ({mode})", plan.target);
        report.status = ActivationStatus::Succeeded;
        report.system_changed = true;
        return report;
    };
    tracing::error!("activation failed, rolling back: {}", failure.message);
    report.failed_checks = failure.failed_checks;
    observer.phase(ActivationPhase::RollingBack);

    match roll_back(host, &plan, mode) {
        Ok(()) => {
            tracing::info!("rolled back to {}", plan.previous_system);
            report.status = ActivationStatus::RolledBack;
            report.message = Some(failure.message);
        }
        Err(e) => {
            tracing::error!("rollback failed: {e:#}");
            report.system_changed = true;
            report.message = Some(format!("{}; rollback failed: {e:#}", failure.message));
        }
    }
    report
}

fn describe(target: &ActivationTarget) -> String {
    match target {
        ActivationTarget::System { store_path } => store_path.clone(),
        ActivationTarget::Build { commit, attribute } => format!("{attribute} at {commit}"),
        ActivationTarget::PreviousGeneration => "the previous generation".to_owned(),
    }
}

/// The flake installable for a build target in `source`. The commit must be
/// a full hash and the attribute a system of this machine's kind, so the
/// server can only choose which configuration to build.
pub fn installable(
    os: Os,
    source: &Path,
    source_ref: &str,
    commit: &str,
    attribute: &str,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit()),
        "invalid commit {commit:?}"
    );
    let prefix = match os {
        Os::Nixos => "nixosConfigurations.",
        Os::Darwin => "darwinConfigurations.",
    };
    anyhow::ensure!(
        attribute.starts_with(prefix)
            && attribute
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-\"".contains(c)),
        "{attribute:?} is not a system this machine can build (expected {prefix}...)"
    );
    let source = source.to_str().context("non-UTF-8 source path")?;
    Ok(format!(
        "git+file://{source}?ref={source_ref}&rev={commit}#{attribute}"
    ))
}

/// Checks everything that can be checked without changing the machine,
/// building the system first if asked to.
fn plan(host: &dyn Host, request: &Request, observer: &dyn Observer) -> anyhow::Result<Plan> {
    let config = host
        .agent_config(Path::new("/"))?
        .context("the agent is not configured (no /etc/manage-nix/agent.json)")?;
    let os = host.os();
    anyhow::ensure!(
        crate::host::supports(os, request.mode),
        "nix-darwin has no boot default; use switch or test"
    );

    let target = match &request.target {
        ActivationTarget::System { store_path } => {
            let target = StorePath::parse(store_path)?;
            let keys = TrustedKeys::parse(&config.trusted_public_keys)?;
            anyhow::ensure!(
                !keys.is_empty(),
                "no trusted public keys are configured \
                 (services.manage-nix.agent.trustedPublicKeys)"
            );
            host.realise(&target)
                .with_context(|| format!("fetching {target}"))?;
            let key = keys.verify(&host.path_info(&target)?, &request.signatures)?;
            tracing::info!("{target} is signed by trusted key {key}");
            target
        }
        // The machine built it from the source of a server it trusts to
        // make it build; there is nobody else to vouch for it.
        ActivationTarget::Build { commit, attribute } => {
            let source = request
                .source
                .as_deref()
                .context("no source repository for the build")?;
            let installable = installable(os, source, crate::source::LOCAL_REF, commit, attribute)?;
            let target = host
                .build(&installable)
                .with_context(|| format!("building {attribute}"))?;
            tracing::info!("built {target}");
            observer.phase(ActivationPhase::Activating);
            target
        }
        // Root put every profile generation there; it needs no signature.
        ActivationTarget::PreviousGeneration => {
            let (number, target) = previous_generation(host)?;
            tracing::info!("rolling back to generation {number}: {target}");
            target
        }
    };

    anyhow::ensure!(
        host.is_system(&target),
        "{target} is not a {} system",
        match os {
            Os::Nixos => "NixOS",
            Os::Darwin => "nix-darwin",
        }
    );
    let target_config = host.agent_config(target.as_path())?.context(
        "the new system does not enable services.manage-nix.agent; \
         activating it would leave the machine unmanaged",
    )?;

    Ok(Plan {
        health_checks: target_config.health_checks,
        previous_system: host.current_system()?,
        previous_generation: host.profile_generation()?,
        target,
    })
}

/// The newest generation older than the profile's current one whose system
/// is not the running system.
fn previous_generation(host: &dyn Host) -> anyhow::Result<ProfileGeneration> {
    let (current, _) = host
        .profile_generation()?
        .context("there is no system profile to roll back")?;
    let running = host.current_system()?;
    host.profile_generations()?
        .into_iter()
        .filter(|(number, system)| *number < current && *system != running)
        .max_by_key(|(number, _)| *number)
        .with_context(|| {
            format!("no generation before {current} has a different system to roll back to")
        })
}

fn apply(
    host: &dyn Host,
    plan: &Plan,
    mode: ActivationMode,
    observer: &dyn Observer,
) -> Result<(), Failure> {
    if mode.sets_profile() {
        host.set_profile(&plan.target)?;
    }
    host.switch_to_configuration(&plan.target, mode.as_str())?;
    if !mode.activates_now() {
        tracing::info!(
            "{} will be used on next boot; skipping health checks",
            plan.target
        );
        return Ok(());
    }
    if plan.health_checks.is_empty() {
        tracing::warn!("no health checks are configured");
    } else {
        observer.phase(ActivationPhase::CheckingHealth);
        host.wait_until_healthy(&plan.health_checks)
            .map_err(|failed_checks| Failure {
                message: format!(
                    "health checks did not pass within {}s",
                    plan.health_checks.timeout_secs
                ),
                failed_checks,
            })?;
    }
    observer.confirm().map_err(|message| Failure {
        message,
        failed_checks: Vec::new(),
    })
}

/// Restores what `apply` may have changed: the boot default for `switch` and
/// `boot`, and the running system for `switch` and `test`. Attempts every
/// step even if one fails.
fn roll_back(host: &dyn Host, plan: &Plan, mode: ActivationMode) -> anyhow::Result<()> {
    let mut errors = Vec::new();
    if mode.sets_profile() {
        let restored = match &plan.previous_generation {
            Some((number, system)) => host
                .switch_profile_generation(*number)
                .and_then(|()| host.switch_to_configuration(system, "boot"))
                .with_context(|| format!("restoring system profile generation {number}")),
            None => Err(anyhow::anyhow!(
                "there was no system profile generation to restore"
            )),
        };
        errors.extend(restored.err());
    }
    if mode.activates_now() {
        errors.extend(
            host.switch_to_configuration(&plan.previous_system, "test")
                .with_context(|| format!("reactivating {}", plan.previous_system))
                .err(),
        );
    }
    match errors.len() {
        0 => Ok(()),
        _ => Err(anyhow::anyhow!(errors
            .iter()
            .map(|e| format!("{e:#}"))
            .collect::<Vec<_>>()
            .join("; "))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    const OLD: &str = "/nix/store/00000000000000000000000000000000-nixos-system-old";
    const NEW: &str = "/nix/store/11111111111111111111111111111111-nixos-system-new";

    /// A fixed signing key standing in for the deploy key.
    fn signing_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[7; 32])
    }

    fn public_key() -> String {
        use base64::Engine;
        let key = base64::engine::general_purpose::STANDARD.encode(signing_key().verifying_key());
        format!("test-1:{key}")
    }

    /// Signs `path` as `Fake::path_info` describes it, under `name`.
    fn signature(name: &str, path: &str) -> String {
        use base64::Engine;
        use ed25519_dalek::Signer;
        let info = PathInfo {
            signatures: vec![],
            ..fake_path_info(path)
        };
        let sig = signing_key().sign(info.fingerprint().as_bytes());
        format!(
            "{name}:{}",
            base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())
        )
    }

    fn fake_path_info(path: &str) -> PathInfo {
        PathInfo {
            path: path.to_owned(),
            nar_hash: "sha256:0000000000000000000000000000000000000000000000000000".into(),
            nar_size: 1234,
            references: vec![OLD.into()],
            signatures: vec![],
        }
    }

    /// A fake machine that records what was done to it.
    struct Fake {
        os: Os,
        builds: bool,
        configs: BTreeMap<String, AgentConfig>,
        signatures: Vec<String>,
        not_systems: BTreeSet<String>,
        failing_actions: BTreeSet<(String, String)>,
        healthy: bool,
        current: RefCell<StorePath>,
        profile: RefCell<Vec<StorePath>>,
        generation: RefCell<usize>,
        log: RefCell<Vec<String>>,
    }

    fn sp(s: &str) -> StorePath {
        StorePath::parse(s).unwrap()
    }

    fn request(path: &str, mode: ActivationMode) -> Request {
        Request {
            target: ActivationTarget::System {
                store_path: path.to_owned(),
            },
            mode,
            signatures: Vec::new(),
            source: None,
        }
    }

    fn run(host: &Fake, path: &str, mode: ActivationMode) -> ActivationReport {
        activate(host, &request(path, mode), &Unobserved)
    }

    /// Records phases, and confirms (or not) when asked.
    #[derive(Default)]
    struct Watcher {
        phases: RefCell<Vec<ActivationPhase>>,
        refuse: bool,
    }

    impl Observer for Watcher {
        fn phase(&self, phase: ActivationPhase) {
            self.phases.borrow_mut().push(phase);
        }

        fn confirm(&self) -> Result<(), String> {
            self.phases
                .borrow_mut()
                .push(ActivationPhase::AwaitingConfirmation);
            match self.refuse {
                true => Err("no confirmation".to_owned()),
                false => Ok(()),
            }
        }
    }

    fn config(checks: bool) -> AgentConfig {
        AgentConfig {
            version: 1,
            trusted_public_keys: vec![public_key()],
            health_checks: HealthChecks {
                systemd_units: if checks {
                    vec!["app.service".into()]
                } else {
                    vec![]
                },
                ..HealthChecks::default()
            },
            servers: Vec::new(),
        }
    }

    impl Fake {
        fn new() -> Self {
            Self {
                os: Os::Nixos,
                builds: true,
                configs: BTreeMap::from([("/".into(), config(true)), (NEW.into(), config(true))]),
                signatures: vec![signature("test-1", NEW)],
                not_systems: BTreeSet::new(),
                failing_actions: BTreeSet::new(),
                healthy: true,
                current: RefCell::new(sp(OLD)),
                profile: RefCell::new(vec![sp(OLD)]),
                generation: RefCell::new(0),
                log: RefCell::new(Vec::new()),
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.borrow().clone()
        }

        fn profile_system(&self) -> StorePath {
            self.profile.borrow()[*self.generation.borrow()].clone()
        }
    }

    impl Host for Fake {
        fn os(&self) -> Os {
            self.os
        }
        fn realise(&self, _: &StorePath) -> anyhow::Result<()> {
            Ok(())
        }
        fn build(&self, installable: &str) -> anyhow::Result<StorePath> {
            self.log.borrow_mut().push(format!("build {installable}"));
            anyhow::ensure!(self.builds, "build failed");
            Ok(sp(NEW))
        }
        fn path_info(&self, path: &StorePath) -> anyhow::Result<PathInfo> {
            Ok(PathInfo {
                signatures: self.signatures.clone(),
                ..fake_path_info(path.as_str())
            })
        }
        fn is_system(&self, path: &StorePath) -> bool {
            !self.not_systems.contains(path.as_str())
        }
        fn agent_config(&self, root: &Path) -> anyhow::Result<Option<AgentConfig>> {
            Ok(self.configs.get(root.to_str().unwrap()).cloned())
        }
        fn current_system(&self) -> anyhow::Result<StorePath> {
            Ok(self.current.borrow().clone())
        }
        fn profile_generation(&self) -> anyhow::Result<Option<ProfileGeneration>> {
            let n = *self.generation.borrow();
            Ok(Some((n as u64 + 1, self.profile.borrow()[n].clone())))
        }
        fn profile_generations(&self) -> anyhow::Result<Vec<ProfileGeneration>> {
            Ok((1..).zip(self.profile.borrow().iter().cloned()).collect())
        }
        fn set_profile(&self, path: &StorePath) -> anyhow::Result<()> {
            self.log.borrow_mut().push(format!("set-profile {path}"));
            self.profile.borrow_mut().push(path.clone());
            *self.generation.borrow_mut() = self.profile.borrow().len() - 1;
            Ok(())
        }
        fn switch_profile_generation(&self, number: u64) -> anyhow::Result<()> {
            self.log
                .borrow_mut()
                .push(format!("switch-generation {number}"));
            *self.generation.borrow_mut() = number as usize - 1;
            Ok(())
        }
        fn switch_to_configuration(&self, system: &StorePath, action: &str) -> anyhow::Result<()> {
            self.log.borrow_mut().push(format!("{action} {system}"));
            if action != "boot" {
                *self.current.borrow_mut() = system.clone();
            }
            let key = (system.to_string(), action.to_owned());
            anyhow::ensure!(!self.failing_actions.contains(&key), "exit status 4");
            Ok(())
        }
        fn wait_until_healthy(&self, _: &HealthChecks) -> Result<(), Vec<String>> {
            self.log.borrow_mut().push("health".into());
            match *self.current.borrow() == sp(NEW) && !self.healthy {
                true => Err(vec!["systemd unit app.service: is failed".into()]),
                false => Ok(()),
            }
        }
    }

    #[test]
    fn healthy_switch_succeeds() {
        let host = Fake::new();
        let report = run(&host, NEW, ActivationMode::Switch);
        assert_eq!(report.status, ActivationStatus::Succeeded, "{report:?}");
        assert!(report.system_changed);
        assert_eq!(report.previous_system.as_deref(), Some(OLD));
        assert_eq!(
            host.log(),
            [
                format!("set-profile {NEW}").as_str(),
                format!("switch {NEW}").as_str(),
                "health"
            ]
        );
        assert_eq!(*host.current.borrow(), sp(NEW));
        assert_eq!(host.profile_system(), sp(NEW));
    }

    #[test]
    fn unhealthy_switch_rolls_back_profile_and_running_system() {
        let host = Fake {
            healthy: false,
            ..Fake::new()
        };
        let report = run(&host, NEW, ActivationMode::Switch);
        assert_eq!(report.status, ActivationStatus::RolledBack, "{report:?}");
        assert!(!report.system_changed);
        assert_eq!(
            report.failed_checks,
            ["systemd unit app.service: is failed"]
        );
        assert_eq!(
            host.log(),
            [
                format!("set-profile {NEW}").as_str(),
                format!("switch {NEW}").as_str(),
                "health",
                "switch-generation 1",
                format!("boot {OLD}").as_str(),
                format!("test {OLD}").as_str(),
            ]
        );
        assert_eq!(*host.current.borrow(), sp(OLD));
        assert_eq!(host.profile_system(), sp(OLD));
    }

    #[test]
    fn failed_switch_to_configuration_rolls_back() {
        let host = Fake {
            failing_actions: BTreeSet::from([(NEW.into(), "test".into())]),
            ..Fake::new()
        };
        let report = run(&host, NEW, ActivationMode::Test);
        assert_eq!(report.status, ActivationStatus::RolledBack, "{report:?}");
        assert_eq!(
            host.log(),
            [
                format!("test {NEW}").as_str(),
                format!("test {OLD}").as_str()
            ]
        );
        assert_eq!(*host.current.borrow(), sp(OLD));
        assert_eq!(host.profile_system(), sp(OLD));
    }

    #[test]
    fn failed_rollback_is_reported() {
        let host = Fake {
            healthy: false,
            failing_actions: BTreeSet::from([(OLD.into(), "test".into())]),
            ..Fake::new()
        };
        let report = run(&host, NEW, ActivationMode::Switch);
        assert_eq!(report.status, ActivationStatus::Failed);
        assert!(report.system_changed);
        assert!(report.message.unwrap().contains("rollback failed"));
    }

    #[test]
    fn boot_mode_skips_health_checks() {
        let host = Fake {
            healthy: false,
            ..Fake::new()
        };
        let report = run(&host, NEW, ActivationMode::Boot);
        assert_eq!(report.status, ActivationStatus::Succeeded);
        assert_eq!(
            host.log(),
            [
                format!("set-profile {NEW}").as_str(),
                format!("boot {NEW}").as_str()
            ]
        );
        assert_eq!(*host.current.borrow(), sp(OLD));
    }

    #[test]
    fn no_health_checks_means_activation_succeeds() {
        let mut host = Fake {
            healthy: false,
            ..Fake::new()
        };
        host.configs.insert(NEW.into(), config(false));
        let report = run(&host, NEW, ActivationMode::Switch);
        assert_eq!(report.status, ActivationStatus::Succeeded);
    }

    fn assert_refused(host: Fake, path: &str, reason: &str) {
        let report = run(&host, path, ActivationMode::Switch);
        assert_eq!(report.status, ActivationStatus::Failed, "{report:?}");
        assert!(!report.system_changed);
        let message = report.message.unwrap();
        assert!(
            message.contains(reason),
            "{message:?} should mention {reason:?}"
        );
        assert!(host.log().is_empty(), "{:?}", host.log());
    }

    #[test]
    fn refuses_unsafe_requests_without_touching_the_machine() {
        assert_refused(Fake::new(), "/etc/passwd", "not a top-level Nix store path");
        assert_refused(
            Fake {
                signatures: vec![],
                ..Fake::new()
            },
            NEW,
            "no valid signature",
        );
        assert_refused(
            Fake {
                signatures: vec![signature("other-1", NEW)],
                ..Fake::new()
            },
            NEW,
            "no valid signature",
        );
        assert_refused(
            Fake {
                not_systems: BTreeSet::from([NEW.into()]),
                ..Fake::new()
            },
            NEW,
            "not a NixOS system",
        );

        let mut host = Fake::new();
        host.configs.remove(NEW);
        assert_refused(host, NEW, "would leave the machine unmanaged");

        let mut host = Fake::new();
        host.configs.remove("/");
        assert_refused(host, NEW, "not configured");

        let mut host = Fake::new();
        host.configs
            .get_mut("/")
            .unwrap()
            .trusted_public_keys
            .clear();
        assert_refused(host, NEW, "no trusted public keys");
    }

    #[test]
    fn signatures_may_come_with_the_request() {
        let host = Fake {
            signatures: vec![],
            ..Fake::new()
        };
        let mut req = request(NEW, ActivationMode::Switch);
        req.signatures = vec![signature("other-1", NEW)];
        let report = activate(&host, &req, &Unobserved);
        assert!(report.message.unwrap().contains("signed by: other-1"));
        req.signatures.push(signature("test-1", NEW));
        let report = activate(&host, &req, &Unobserved);
        assert_eq!(report.status, ActivationStatus::Succeeded, "{report:?}");
    }

    #[test]
    fn phases_are_reported_in_order() {
        let host = Fake {
            healthy: false,
            ..Fake::new()
        };
        let watcher = Watcher::default();
        activate(&host, &request(NEW, ActivationMode::Switch), &watcher);
        assert_eq!(
            *watcher.phases.borrow(),
            [
                ActivationPhase::Activating,
                ActivationPhase::CheckingHealth,
                ActivationPhase::RollingBack
            ]
        );

        let host = Fake::new();
        let watcher = Watcher::default();
        let report = activate(&host, &request(NEW, ActivationMode::Switch), &watcher);
        assert_eq!(report.status, ActivationStatus::Succeeded);
        assert_eq!(
            *watcher.phases.borrow(),
            [
                ActivationPhase::Activating,
                ActivationPhase::CheckingHealth,
                ActivationPhase::AwaitingConfirmation
            ]
        );
    }

    #[test]
    fn a_missing_confirmation_rolls_back() {
        let host = Fake::new();
        let watcher = Watcher {
            refuse: true,
            ..Watcher::default()
        };
        let report = activate(&host, &request(NEW, ActivationMode::Switch), &watcher);
        assert_eq!(report.status, ActivationStatus::RolledBack, "{report:?}");
        assert_eq!(report.message.as_deref(), Some("no confirmation"));
        assert_eq!(*host.current.borrow(), sp(OLD));
        assert_eq!(host.profile_system(), sp(OLD));
    }

    #[test]
    fn boot_mode_needs_no_confirmation() {
        let host = Fake::new();
        let watcher = Watcher {
            refuse: true,
            ..Watcher::default()
        };
        let report = activate(&host, &request(NEW, ActivationMode::Boot), &watcher);
        assert_eq!(report.status, ActivationStatus::Succeeded, "{report:?}");
    }

    fn rollback() -> Request {
        Request {
            target: ActivationTarget::PreviousGeneration,
            mode: ActivationMode::Switch,
            signatures: Vec::new(),
            source: None,
        }
    }

    #[test]
    fn rolls_back_to_the_previous_generation_without_a_signature() {
        let host = Fake::new();
        run(&host, NEW, ActivationMode::Switch);
        let host = Fake {
            signatures: vec![],
            ..host
        };
        host.log.borrow_mut().clear();
        // The old system enables the agent too.
        let mut host = host;
        host.configs.insert(OLD.into(), config(true));
        let report = activate(&host, &rollback(), &Unobserved);
        assert_eq!(report.status, ActivationStatus::Succeeded, "{report:?}");
        assert_eq!(report.store_path, OLD);
        assert_eq!(report.previous_system.as_deref(), Some(NEW));
        assert_eq!(*host.current.borrow(), sp(OLD));
        assert_eq!(host.profile_system(), sp(OLD));
        // Rolling back again returns to the newer system.
        let report = activate(&host, &rollback(), &Unobserved);
        assert_eq!(report.store_path, NEW, "{report:?}");
    }

    #[test]
    fn rollback_needs_an_earlier_different_generation() {
        let host = Fake::new();
        let report = activate(&host, &rollback(), &Unobserved);
        assert_eq!(report.status, ActivationStatus::Failed);
        assert_eq!(report.store_path, "");
        assert!(report.message.unwrap().contains("no generation before 1"));
        assert!(host.log().is_empty());
    }

    #[test]
    fn trusted_keys_come_from_the_running_system() {
        // The new system trusting some other key does not matter.
        let mut host = Fake {
            signatures: vec![],
            ..Fake::new()
        };
        host.configs.get_mut(NEW).unwrap().trusted_public_keys = vec![];
        assert_refused(host, NEW, "no valid signature");
    }

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn build(attribute: &str, mode: ActivationMode) -> Request {
        Request {
            target: ActivationTarget::Build {
                commit: COMMIT.to_owned(),
                attribute: attribute.to_owned(),
            },
            mode,
            signatures: Vec::new(),
            source: Some("/var/lib/manage-nix/sources/s.git".into()),
        }
    }

    #[test]
    fn built_systems_need_no_signature() {
        let host = Fake {
            signatures: vec![],
            ..Fake::new()
        };
        let watcher = Watcher::default();
        let attribute = Os::Nixos.flake_attribute("web-1");
        let report = activate(&host, &build(&attribute, ActivationMode::Switch), &watcher);
        assert_eq!(report.status, ActivationStatus::Succeeded, "{report:?}");
        assert_eq!(report.store_path, NEW);
        assert_eq!(
            host.log()[0],
            format!(
                "build git+file:///var/lib/manage-nix/sources/s.git?ref={}&rev={COMMIT}#{attribute}",
                crate::source::LOCAL_REF
            )
        );
        assert_eq!(
            watcher.phases.borrow()[..2],
            [ActivationPhase::Building, ActivationPhase::Activating]
        );
    }

    #[test]
    fn builds_are_limited_to_this_machines_systems() {
        let host = Fake::new();
        for attribute in [
            Os::Darwin.flake_attribute("mac"),
            "packages.x86_64-linux.default".to_owned(),
            "nixosConfigurations.x.config.system.build.toplevel --impure".to_owned(),
        ] {
            let report = activate(
                &host,
                &build(&attribute, ActivationMode::Switch),
                &Unobserved,
            );
            assert_eq!(report.status, ActivationStatus::Failed, "{attribute}");
            assert!(!report.system_changed);
        }
        assert!(installable(
            Os::Nixos,
            Path::new("/s"),
            "r",
            "abc",
            "nixosConfigurations.x"
        )
        .is_err());
        assert!(host.log().is_empty(), "{:?}", host.log());

        let host = Fake {
            builds: false,
            ..Fake::new()
        };
        let report = activate(
            &host,
            &build(&Os::Nixos.flake_attribute("web-1"), ActivationMode::Switch),
            &Unobserved,
        );
        assert_eq!(report.status, ActivationStatus::Failed);
        assert!(report.message.unwrap().contains("build failed"));
        assert_eq!(host.log().len(), 1);
    }

    #[test]
    fn darwin_has_no_boot_mode() {
        let host = Fake {
            os: Os::Darwin,
            ..Fake::new()
        };
        let report = run(&host, NEW, ActivationMode::Boot);
        assert_eq!(report.status, ActivationStatus::Failed);
        assert!(report.message.unwrap().contains("no boot default"));
        assert!(host.log().is_empty());
        let report = activate(
            &host,
            &build(&Os::Darwin.flake_attribute("mac"), ActivationMode::Switch),
            &Unobserved,
        );
        assert_eq!(report.status, ActivationStatus::Succeeded, "{report:?}");
    }
}
