//! Request and response bodies of the HTTP API.
//!
//! Errors are returned as [`ErrorBody`] with a 4xx or 5xx status. Timestamps
//! are RFC 3339 strings in UTC.

use serde::{Deserialize, Serialize};

use crate::Facts;

/// `POST /api/machines`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateMachine {
    pub name: String,
    /// How long the enrollment token is valid; the server's default if
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrollment_token_ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedMachine {
    pub machine: Machine,
    pub enrollment_token: EnrollmentToken,
}

/// `POST /api/machines/:id/enrollment-tokens`: a new token for a machine
/// that has not enrolled yet, or that needs to enroll again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateEnrollmentToken {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

/// A one-time token the agent exchanges for its credential. Only its hash
/// is stored, so it is shown once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollmentToken {
    pub token: String,
    pub expires_at: String,
}

/// `GET /api/machines` returns a list of these; `GET /api/machines/:id`
/// returns one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub status: MachineStatus,
    pub created_at: String,
    pub enrolled_at: Option<String>,
    /// The last `hello` or `heartbeat`.
    pub last_seen_at: Option<String>,
    pub agent_version: Option<String>,
    /// The running system, from `facts`.
    pub current_system: Option<String>,
    pub facts: Option<Facts>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineStatus {
    /// Created, but no agent has enrolled yet.
    Pending,
    /// The agent is connected and its heartbeats are on time.
    Online,
    /// Enrolled, but no agent is connected (or its heartbeats stopped).
    Offline,
}

impl MachineStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Online => "online",
            Self::Offline => "offline",
        }
    }
}

impl std::fmt::Display for MachineStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `POST /api/agent/enroll`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub machine_id: String,
    /// The machine's long-lived credential for the agent WebSocket.
    pub credential: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}
