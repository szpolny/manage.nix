//! Message types shared by `mnix-agent`, `mnix-server`, and `mnix`.
//!
//! Protocol v1:
//!
//! - [`AgentMessage`] and [`ServerMessage`] are the JSON text frames of the
//!   agent's WebSocket ([`AGENT_WS_PATH`]).
//! - [`api`] holds the HTTP API's request and response bodies.
//! - [`Facts`] and [`ActivationReport`] are what the agent reports about
//!   itself; `mnix-agent facts` and `mnix-agent activate` print them too.

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
}

/// What a machine reports about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub hostname: String,
    /// The running system (`/run/current-system`).
    pub current_system: String,
    /// The system the machine booted into (`/run/booted-system`).
    pub booted_system: Option<String>,
    /// `nixos-version` of the running system.
    pub nixos_version: Option<String>,
    /// Release of the running kernel (`uname -r`).
    pub kernel: String,
    /// True when the running system's kernel, initrd, or kernel modules
    /// differ from the booted system's, so a reboot is needed to use them.
    pub reboot_required: bool,
    /// Generations of the system profile, oldest first.
    pub generations: Vec<Generation>,
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

/// How to activate a system, matching `switch-to-configuration`.
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
                "nixos_version": null,
                "kernel": "6.12.0",
                "reboot_required": false,
                "generations": []
            }
        }))
        .unwrap();
        assert!(matches!(hello, AgentMessage::Hello(h) if h.facts.hostname == "web-1"));
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
