//! Message types shared by `mnix-agent` and `mnix-server`.
//!
//! Protocol v1 messages are added in M2. M1 defines the payloads the agent
//! already produces locally: [`Facts`] and [`ActivationReport`].

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
