//! Message types shared by `mnix-agent`, `mnix-server`, and `mnix`.
//!
//! Protocol v1:
//!
//! - [`AgentMessage`] and [`ServerMessage`] are the JSON text frames of the
//!   agent's WebSocket ([`AGENT_WS_PATH`]).
//! - [`api`] holds the HTTP API's request and response bodies.
//! - [`Facts`] and [`ActivationReport`] are what the agent reports about
//!   itself; `mnix-agent facts` and `mnix-agent activate` print them too.
//! - [`Activate`] is the one command the server can give an agent: it
//!   activates a system, or restarts the machine.

pub mod api;

use serde::{Deserialize, Serialize};

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(1);

/// A protocol version, sent by both sides when a session starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u32);

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// Where agents enroll: `POST` an [`api::EnrollRequest`].
pub const ENROLL_PATH: &str = "/api/agent/enroll";

/// The agent's WebSocket endpoint. The upgrade request carries the machine's
/// credential as `Authorization: Bearer <credential>`.
pub const AGENT_WS_PATH: &str = "/api/agent/ws";

/// Where an agent downloads the source of a [`ActivationTarget::Build`]: a
/// Git bundle (`application/x-git-bundle`) whose one ref,
/// [`SOURCE_BUNDLE_REF`], points at the deployment's commit. Authenticated
/// like the WebSocket. Each `have=<commit>` query parameter names a commit
/// the agent already has, so the bundle can leave out its history.
pub fn agent_source_path(deployment_id: &str) -> String {
    format!("/api/agent/deployments/{deployment_id}/source")
}

/// The ref in a source bundle.
pub const SOURCE_BUNDLE_REF: &str = "refs/mnix/source";

/// A message from the agent to the server.
///
/// Receivers ignore (and log) messages whose `type` they do not know, so
/// new message types can be added within a protocol version. Changing an
/// existing message needs a new version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessage {
    /// The first message of every session.
    Hello(Hello),
    /// Sent every `heartbeat_interval_secs` from [`ServerMessage::Welcome`].
    Heartbeat,
    /// The machine's facts changed since the last `hello` or `facts`.
    Facts { facts: Facts },
    /// An activation the server asked for has reached a new phase. Sent on
    /// every new session for activations that have not finished, so the
    /// server knows the agent has the request.
    ActivationStatus {
        deployment_id: String,
        phase: ActivationPhase,
    },
    /// Output of an activation: the bytes of the agent's activation log
    /// starting at `offset`. The agent sends each log from the start on every
    /// new session; the server keeps what it has not seen.
    ActivationLog {
        deployment_id: String,
        offset: u64,
        text: String,
    },
    /// An activation finished. Sent on every new session until the server
    /// answers `result_received`.
    ActivationResult {
        deployment_id: String,
        report: ActivationReport,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: ProtocolVersion,
    pub agent_version: String,
    pub facts: Facts,
}

/// A message from the server to the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The answer to a valid `hello`; the session is open.
    Welcome {
        protocol_version: ProtocolVersion,
        machine_id: String,
        /// How often the agent sends `heartbeat`.
        heartbeat_interval_secs: u64,
        /// How long the server waits for a heartbeat before it considers the
        /// machine offline. The agent uses the same timeout for the server.
        heartbeat_timeout_secs: u64,
    },
    /// The server is about to close the connection.
    Error { message: String },
    /// Activate a system. The server sends it again on every new session
    /// until the agent reports a phase for it, so agents ignore requests
    /// for deployments they already know.
    Activate(Activate),
    /// The answer to `activation_status` with phase `awaiting_confirmation`:
    /// the machine reached the server over a new connection after the
    /// switch, so it can keep the new system.
    Confirm { deployment_id: String },
    /// The server has stored an `activation_result`; the agent can forget
    /// the activation.
    ResultReceived { deployment_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activate {
    /// The deployment this activation belongs to (a UUID).
    pub deployment_id: String,
    pub target: ActivationTarget,
    pub mode: ActivationMode,
    /// Signatures on the target's top-level path, checked like the ones Nix
    /// stores with the path. The server signs what it builds.
    #[serde(default)]
    pub signatures: Vec<String>,
    /// How long the agent waits for `confirm` after a `switch` or `test`
    /// activation is healthy before rolling back on its own.
    pub confirm_timeout_secs: u64,
}

/// What to activate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActivationTarget {
    /// A system's top-level store path, signed by a trusted key.
    System { store_path: String },
    /// Build `attribute` of the flake at `commit` on the machine, then
    /// activate the result. The agent downloads the source from the server
    /// that asked ([`agent_source_path`]) and accepts this only from servers
    /// it allows to make it build.
    Build {
        /// The full commit hash.
        commit: String,
        /// The flake output, for example
        /// `nixosConfigurations."web-1".config.system.build.toplevel`.
        attribute: String,
    },
    /// The newest earlier generation of the system profile that differs
    /// from the running system (`mnix rollback`). Profile generations need
    /// no signature: root installed them.
    PreviousGeneration,
    /// Restart the machine (`mnix reboot`). The agent reports success once
    /// it runs again in a new boot; `mode` is ignored.
    Reboot,
}

/// How far an activation has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationPhase {
    /// The agent has the request and will start it.
    Accepted,
    /// Fetching the source and building the system on the machine.
    Building,
    /// Fetching and checking the system, then switching to it.
    Activating,
    /// Running the new system's health checks.
    CheckingHealth,
    /// Healthy; waiting for `confirm` over a new connection.
    AwaitingConfirmation,
    /// Something failed; restoring the previous system.
    RollingBack,
    /// The machine is restarting.
    Rebooting,
}

impl ActivationPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Building => "building",
            Self::Activating => "activating",
            Self::CheckingHealth => "checking_health",
            Self::AwaitingConfirmation => "awaiting_confirmation",
            Self::RollingBack => "rolling_back",
            Self::Rebooting => "rebooting",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Accepted,
            Self::Building,
            Self::Activating,
            Self::CheckingHealth,
            Self::AwaitingConfirmation,
            Self::RollingBack,
            Self::Rebooting,
        ]
        .into_iter()
        .find(|p| p.as_str() == s)
    }
}

/// What a machine reports about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub hostname: String,
    /// What kind of system the machine runs; agents before 0.2 only ran on
    /// NixOS.
    #[serde(default)]
    pub os: Os,
    /// The running system (`/run/current-system`).
    pub current_system: String,
    /// The system the machine booted into (`/run/booted-system`), on NixOS.
    pub booted_system: Option<String>,
    /// `nixos-version` or `darwin-version` of the running system.
    #[serde(alias = "nixos_version")]
    pub os_version: Option<String>,
    /// The running configuration's `system.configurationRevision`: the
    /// commit it was built from, when the flake sets it.
    #[serde(default)]
    pub configuration_revision: Option<String>,
    /// Release of the running kernel (`uname -r`).
    pub kernel: String,
    /// True when the running system's kernel, initrd, or kernel modules
    /// differ from the booted system's, so a reboot is needed to use them.
    pub reboot_required: bool,
    /// Generations of the system profile, oldest first.
    pub generations: Vec<Generation>,
}

/// The kind of system a machine runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    /// NixOS, built from `nixosConfigurations`.
    #[default]
    Nixos,
    /// macOS with nix-darwin, built from `darwinConfigurations`.
    Darwin,
}

impl Os {
    /// The flake output that builds the system of `configuration`.
    pub fn flake_attribute(self, configuration: &str) -> String {
        match self {
            Self::Nixos => {
                format!("nixosConfigurations.\"{configuration}\".config.system.build.toplevel")
            }
            Self::Darwin => format!("darwinConfigurations.\"{configuration}\".system"),
        }
    }
}

/// One generation of the system profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Generation {
    pub number: u64,
    pub store_path: String,
    /// When the generation was created, RFC 3339 in UTC.
    pub created_at: String,
    /// True for the generation the profile points at (the boot default).
    pub current: bool,
}

/// How to activate a system, matching `switch-to-configuration`. nix-darwin
/// has no boot default apart from the running system, so on macOS `boot`
/// is refused and `test` activates without adding a profile generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationMode {
    /// Make it the boot default and activate it now.
    Switch,
    /// Make it the boot default only.
    Boot,
    /// Activate it now without changing the boot default.
    Test,
}

impl ActivationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Switch => "switch",
            Self::Boot => "boot",
            Self::Test => "test",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Self::Switch, Self::Boot, Self::Test]
            .into_iter()
            .find(|m| m.as_str() == s)
    }

    /// Whether the system profile (and so the boot default) changes.
    pub fn sets_profile(self) -> bool {
        matches!(self, Self::Switch | Self::Boot)
    }

    /// Whether the running system changes, so health checks apply.
    pub fn activates_now(self) -> bool {
        matches!(self, Self::Switch | Self::Test)
    }
}

impl std::fmt::Display for ActivationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How an activation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationStatus {
    /// The system was activated and is healthy.
    Succeeded,
    /// The activation was refused before anything changed, or it failed and
    /// so did the rollback (see `system_changed`).
    Failed,
    /// The activation failed and the previous system was restored.
    RolledBack,
}

/// The outcome of one `activate` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationReport {
    /// The system that was (to be) activated; empty if a
    /// `previous_generation` target could not be resolved.
    pub store_path: String,
    pub mode: ActivationMode,
    pub status: ActivationStatus,
    /// Whether the machine is no longer on its previous system: true after
    /// success and after a failed rollback (the machine needs attention),
    /// false after a refusal or a successful rollback.
    pub system_changed: bool,
    /// The running system before activation.
    pub previous_system: Option<String>,
    /// Human-readable reason for a failure or rollback.
    pub message: Option<String>,
    /// Health checks still failing when the deadline passed.
    pub failed_checks: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_serializes_as_a_bare_number() {
        assert_eq!(serde_json::to_string(&PROTOCOL_VERSION).unwrap(), "1");
        assert_eq!(PROTOCOL_VERSION.to_string(), "v1");
    }

    #[test]
    fn messages_are_tagged_by_type() {
        assert_eq!(
            serde_json::to_value(AgentMessage::Heartbeat).unwrap(),
            serde_json::json!({"type": "heartbeat"})
        );
        let hello: AgentMessage = serde_json::from_value(serde_json::json!({
            "type": "hello",
            "protocol_version": 1,
            "agent_version": "0.1.0",
            "facts": {
                "hostname": "web-1",
                "current_system": "/nix/store/x-nixos-system",
                "booted_system": null,
                "nixos_version": "26.11",
                "kernel": "6.12.0",
                "reboot_required": false,
                "generations": []
            }
        }))
        .unwrap();
        // Facts from agents before 0.2: NixOS, `nixos_version`.
        assert!(matches!(hello, AgentMessage::Hello(ref h)
            if h.facts.hostname == "web-1"
                && h.facts.os == Os::Nixos
                && h.facts.os_version.as_deref() == Some("26.11")));
        let welcome = ServerMessage::Welcome {
            protocol_version: PROTOCOL_VERSION,
            machine_id: "m".to_owned(),
            heartbeat_interval_secs: 15,
            heartbeat_timeout_secs: 45,
        };
        assert_eq!(
            serde_json::to_value(&welcome).unwrap()["type"],
            serde_json::json!("welcome")
        );
    }

    #[test]
    fn activate_carries_a_tagged_target() {
        let activate = ServerMessage::Activate(Activate {
            deployment_id: "d".to_owned(),
            target: ActivationTarget::System {
                store_path: "/nix/store/x-nixos-system".to_owned(),
            },
            mode: ActivationMode::Switch,
            signatures: vec!["k:sig".to_owned()],
            confirm_timeout_secs: 60,
        });
        let json = serde_json::to_value(&activate).unwrap();
        assert_eq!(json["type"], "activate");
        assert_eq!(
            json["target"],
            serde_json::json!({"type": "system", "store_path": "/nix/store/x-nixos-system"})
        );
        let rollback: ServerMessage = serde_json::from_value(serde_json::json!({
            "type": "activate",
            "deployment_id": "d",
            "target": {"type": "previous_generation"},
            "mode": "switch",
            "confirm_timeout_secs": 60
        }))
        .unwrap();
        assert!(matches!(
            rollback,
            ServerMessage::Activate(Activate { target: ActivationTarget::PreviousGeneration, ref signatures, .. })
                if signatures.is_empty()
        ));
    }

    #[test]
    fn build_targets_name_a_commit_and_an_attribute() {
        let target: ActivationTarget = serde_json::from_value(serde_json::json!({
            "type": "build",
            "commit": "0123456789abcdef0123456789abcdef01234567",
            "attribute": Os::Darwin.flake_attribute("mac"),
        }))
        .unwrap();
        assert!(
            matches!(target, ActivationTarget::Build { ref attribute, .. }
            if attribute == "darwinConfigurations.\"mac\".system")
        );
        assert_eq!(
            Os::Nixos.flake_attribute("web-1"),
            "nixosConfigurations.\"web-1\".config.system.build.toplevel"
        );
        assert_eq!(agent_source_path("d"), "/api/agent/deployments/d/source");
    }

    #[test]
    fn phases_and_modes_round_trip_as_strings() {
        for phase in [
            ActivationPhase::Accepted,
            ActivationPhase::Building,
            ActivationPhase::AwaitingConfirmation,
            ActivationPhase::RollingBack,
            ActivationPhase::Rebooting,
        ] {
            assert_eq!(ActivationPhase::parse(phase.as_str()), Some(phase));
            assert_eq!(
                serde_json::to_value(phase).unwrap(),
                serde_json::json!(phase.as_str())
            );
        }
        assert_eq!(ActivationMode::parse("test"), Some(ActivationMode::Test));
        assert_eq!(ActivationMode::parse("reboot"), None);
    }

    #[test]
    fn enums_serialize_in_snake_case() {
        assert_eq!(
            serde_json::to_string(&ActivationStatus::RolledBack).unwrap(),
            r#""rolled_back""#
        );
        assert_eq!(
            serde_json::to_string(&ActivationMode::Switch).unwrap(),
            r#""switch""#
        );
    }
}
