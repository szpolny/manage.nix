//! Request and response bodies of the HTTP API.
//!
//! Errors are returned as [`ErrorBody`] with a 4xx or 5xx status. Timestamps
//! are RFC 3339 strings in UTC.

use serde::{Deserialize, Serialize};

use crate::{ActivationMode, ActivationPhase, ActivationReport, Facts};

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
    /// The repository the machine's configuration is built from.
    #[serde(default)]
    pub repository: Option<String>,
    /// The machine's name under `nixosConfigurations` in that repository.
    #[serde(default)]
    pub configuration: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The revision the running system was built from, if a deployment
    /// built it.
    #[serde(default)]
    pub revision: Option<Revision>,
    /// The machine's most recent deployment.
    #[serde(default)]
    pub last_deployment: Option<MachineDeployment>,
}

/// Where a system came from: the newest deployment that built it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub deployment_id: String,
    pub repository: Option<String>,
    /// The revision as requested (`main`, `v1.2`, a commit).
    pub rev: Option<String>,
    pub commit: String,
}

/// `GET /api/machines/:machine/deployments` returns these, newest first:
/// the machine's part in each deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineDeployment {
    pub deployment_id: String,
    pub kind: DeploymentKind,
    pub mode: ActivationMode,
    pub rev: Option<String>,
    pub commit: Option<String>,
    pub requested_by: String,
    /// The machine's status in the deployment.
    pub status: MachineDeploymentStatus,
    pub store_path: Option<String>,
    pub message: Option<String>,
    pub created_at: String,
    pub finished_at: Option<String>,
}

/// `GET /api/machines/:machine/generations` returns the machine's system
/// profile generations (from its facts), newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationView {
    pub number: u64,
    pub store_path: String,
    pub created_at: String,
    /// The generation the profile points at (the boot default).
    pub current: bool,
    /// Whether this is the running system.
    pub running: bool,
    /// Whether the machine booted this system.
    pub booted: bool,
    pub revision: Option<Revision>,
}

/// `GET /api/machines/:machine/diff?from=<generation>&to=<generation>`:
/// `nix store diff-closures` between two of the machine's generations,
/// computed on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosureDiff {
    pub from: String,
    pub to: String,
    /// One line per changed package: `name: 1.0 → 1.1, +12.3 KiB`.
    pub diff: String,
}

/// `POST /api/session`: log in. The response sets the session cookie.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
    pub username: String,
    pub password: String,
}

/// `GET /api/session`: who is logged in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub username: String,
}

/// `PUT /api/session/password`: change the logged-in user's password.
/// Ends the user's other sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangePassword {
    pub current_password: String,
    pub new_password: String,
}

/// `PUT /api/machines/:machine/configuration`: where the machine's system
/// is built from. `configuration` defaults to the machine's name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetConfiguration {
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<String>,
}

/// `PUT /api/machines/:machine/tags`: replaces the machine's tags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetTags {
    pub tags: Vec<String>,
}

/// `POST /api/repositories`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRepository {
    pub name: String,
    /// Anything `git clone` accepts: `https://...`, `ssh://...`,
    /// `user@host:path`, `file:///...`, or an absolute path.
    pub url: String,
}

/// A Git repository containing a flake with `nixosConfigurations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    pub id: String,
    pub name: String,
    pub url: String,
    pub created_at: String,
}

/// `POST /api/deployments`: build `rev` for the targets and activate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateDeployment {
    /// Machine names or IDs, or `tag:<tag>` for every machine with a tag.
    /// All machines must be built from the same repository.
    pub targets: Vec<String>,
    /// A branch, tag, or commit; `HEAD` (the default branch) if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default = "default_mode")]
    pub mode: ActivationMode,
    /// Who asked, for the deployment history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
}

/// `POST /api/machines/:machine/rollback`: activate the machine's previous
/// system profile generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRollback {
    #[serde(default = "default_mode")]
    pub mode: ActivationMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
}

/// `POST /api/machines/:machine/reboot`: restart the machine. Refused while
/// the machine is offline, so a reboot never waits for it to come back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateReboot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
}

/// `GET /api/machines/:machine/updates?rev=<rev>`: what the machine's
/// repository has that its running system was not built from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Updates {
    pub repository: String,
    /// The revision compared with: `rev` if given, otherwise the branch the
    /// running system was deployed from, otherwise `HEAD`.
    pub rev: String,
    /// The commit `rev` names now.
    pub commit: String,
    /// The commit the running system was built from, if known: from its
    /// deployment, or its `system.configurationRevision`.
    pub current_commit: Option<String>,
    /// Commits in `rev` that the running system lacks; `None` if the
    /// running commit is unknown (or not in the repository).
    pub behind: Option<u64>,
    /// Commits in the running system that `rev` lacks.
    pub ahead: Option<u64>,
    /// The newest of the commits behind, newest first (at most 50).
    pub commits: Vec<Commit>,
    /// When the repository was last fetched.
    pub fetched_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub commit: String,
    pub summary: String,
    pub author: String,
    pub date: String,
}

/// `GET /api/updates` returns one of these for every machine with a
/// configuration: [`Updates`] in short, compared with the default revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateSummary {
    pub machine: String,
    pub rev: Option<String>,
    pub commit: Option<String>,
    pub behind: Option<u64>,
    /// Why there is no answer for this machine.
    pub error: Option<String>,
}

/// `POST /api/machines/:machine/updates/preview`: build what `rev` would
/// give the machine, on the server, and compare it with the running
/// system. `GET` on the same path returns the latest preview.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePreview {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewStatus {
    Building,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    pub status: PreviewStatus,
    pub rev: String,
    /// The commit `rev` resolved to, once fetched.
    pub commit: Option<String>,
    /// The running system it is compared with.
    pub from: String,
    /// The system `rev` builds, once built.
    pub to: Option<String>,
    /// `nix store diff-closures` from the running system to the new one.
    pub diff: Option<String>,
    /// Why it failed.
    pub message: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

fn default_mode() -> ActivationMode {
    ActivationMode::Switch
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentKind {
    /// Build a revision and activate it.
    Deploy,
    /// Activate each machine's previous system profile generation.
    Rollback,
    /// Restart each machine.
    Reboot,
}

/// The state of a deployment as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStatus {
    /// Waiting for the server's builder.
    Queued,
    /// Fetching the revision and building systems.
    Building,
    /// Machines are activating their systems.
    Activating,
    /// Every machine succeeded.
    Succeeded,
    /// At least one machine failed or rolled back.
    Failed,
}

/// The state of one machine in a deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineDeploymentStatus {
    Queued,
    /// Building, signing, and pushing the machine's system.
    Building,
    /// Sent to the agent (or waiting for it to connect).
    Activating,
    Succeeded,
    /// The agent activated the system, found it unhealthy or unconfirmed,
    /// and restored the previous one.
    RolledBack,
    /// The build, the activation, or a rollback failed, or the machine did
    /// not answer in time.
    Failed,
}

macro_rules! string_enum {
    ($ty:ty { $($variant:ident = $s:literal),* $(,)? }) => {
        impl $ty {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $s),* }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some(Self::$variant),)* _ => None }
            }
        }

        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum!(DeploymentKind {
    Deploy = "deploy",
    Rollback = "rollback",
    Reboot = "reboot",
});
string_enum!(DeploymentStatus {
    Queued = "queued",
    Building = "building",
    Activating = "activating",
    Succeeded = "succeeded",
    Failed = "failed",
});
string_enum!(MachineDeploymentStatus {
    Queued = "queued",
    Building = "building",
    Activating = "activating",
    Succeeded = "succeeded",
    RolledBack = "rolled_back",
    Failed = "failed",
});

impl DeploymentStatus {
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

impl MachineDeploymentStatus {
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::RolledBack | Self::Failed)
    }
}

/// `GET /api/deployments` returns a list of these, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    pub id: String,
    pub kind: DeploymentKind,
    pub status: DeploymentStatus,
    /// Why the deployment failed before reaching the machines.
    pub message: Option<String>,
    pub repository: Option<String>,
    /// The revision as requested (`main`, `v1.2`, a commit).
    pub rev: Option<String>,
    /// The commit `rev` resolved to.
    pub commit: Option<String>,
    pub mode: ActivationMode,
    pub requested_by: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub machines: Vec<DeploymentMachine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentMachine {
    pub machine_id: String,
    pub machine: String,
    pub configuration: Option<String>,
    pub status: MachineDeploymentStatus,
    /// The phase the agent last reported.
    pub phase: Option<ActivationPhase>,
    /// The system built for (or rolled back to on) the machine.
    pub store_path: Option<String>,
    pub message: Option<String>,
    /// The agent's report, once it finished.
    pub report: Option<ActivationReport>,
    pub finished_at: Option<String>,
}

/// `GET /api/deployments/:id/logs?after=<id>` returns these, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// Increases with every entry; pass the last one as `after`.
    pub id: i64,
    /// The machine the output is about, or null for the whole deployment.
    pub machine: Option<String>,
    /// `server` (fetching, building, pushing) or `agent` (activation).
    pub source: String,
    pub text: String,
    pub created_at: String,
}

/// `GET /api/signing-key`: the key the server signs systems with. Add it
/// to `services.manage-nix.agent.trustedPublicKeys` on every machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningKey {
    pub public_key: String,
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
