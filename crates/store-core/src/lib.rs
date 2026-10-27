//! The [`Store`] trait and the records it stores, shared by the SQLite and
//! Postgres backends (`mnix-store-sqlite`, `mnix-store-postgres`).
//!
//! The backends live in separate crates because sqlx's checked query macros
//! pick the database from one `DATABASE_URL` per crate, so each backend's
//! offline query data (`.sqlx/`) is prepared against its own database.
//! Server code should depend on the `mnix-store` facade instead.

use async_trait::async_trait;
use mnix_protocol::api::{DeploymentKind, DeploymentStatus, MachineDeploymentStatus};
use mnix_protocol::{ActivationMode, ActivationPhase, ActivationReport, Facts};
use time::OffsetDateTime;
use uuid::Uuid;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported database URL {0:?}: expected sqlite:// or postgres://")]
    UnsupportedUrl(String),
    /// A unique constraint was violated, for example a duplicate machine name.
    #[error("{0} already exists")]
    Conflict(String),
    #[error("invalid value in the database: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Database(sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                Self::Conflict(db.table().unwrap_or("record").to_owned())
            }
            _ => Self::Database(e),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
}

impl Backend {
    /// Picks the backend from a database URL's scheme.
    pub fn from_url(url: &str) -> Result<Self> {
        let scheme = url.split_once(':').map(|(scheme, _)| scheme);
        match scheme {
            Some("sqlite") => Ok(Self::Sqlite),
            Some("postgres" | "postgresql") => Ok(Self::Postgres),
            _ => Err(Error::UnsupportedUrl(url.to_owned())),
        }
    }
}

/// A table and its column names, in declaration order. Used to compare the
/// schemas the two backends' migrations produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organization {
    pub id: Uuid,
    pub name: String,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub created_at: OffsetDateTime,
    /// When the machine redeemed an enrollment token; `None` until then.
    pub enrolled_at: Option<OffsetDateTime>,
    /// The agent version from the most recent `hello`.
    pub agent_version: Option<String>,
    pub facts: Option<Facts>,
    pub facts_updated_at: Option<OffsetDateTime>,
    /// The last `hello` or `heartbeat` from the agent.
    pub last_seen_at: Option<OffsetDateTime>,
    /// Whether an agent session is open (not yet closed or reaped).
    pub connected: bool,
    /// The repository the machine's system is built from.
    pub repository_id: Option<Uuid>,
    /// The machine's name under `nixosConfigurations`.
    pub configuration: Option<String>,
    /// Sorted.
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub name: String,
    pub url: String,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDeployment {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub kind: DeploymentKind,
    pub status: DeploymentStatus,
    pub repository_id: Option<Uuid>,
    pub rev: Option<String>,
    pub mode: ActivationMode,
    pub requested_by: String,
    pub created_at: OffsetDateTime,
    pub machines: Vec<NewDeploymentMachine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDeploymentMachine {
    pub machine_id: Uuid,
    pub configuration: Option<String>,
    /// `queued` to build first, or `activating` to hand the request to the
    /// agent at once (a rollback).
    pub status: MachineDeploymentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub kind: DeploymentKind,
    pub status: DeploymentStatus,
    pub message: Option<String>,
    pub repository_id: Option<Uuid>,
    pub rev: Option<String>,
    pub commit_id: Option<String>,
    pub mode: ActivationMode,
    pub requested_by: String,
    pub created_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
    /// Sorted by machine name.
    pub machines: Vec<DeploymentMachine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentMachine {
    pub machine_id: Uuid,
    pub machine_name: String,
    pub configuration: Option<String>,
    pub status: MachineDeploymentStatus,
    pub phase: Option<ActivationPhase>,
    pub store_path: Option<String>,
    pub signatures: Vec<String>,
    pub message: Option<String>,
    pub report: Option<ActivationReport>,
    pub released_at: Option<OffsetDateTime>,
    pub accepted_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
}

/// An activation the machine's agent has not acknowledged yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingActivation {
    pub deployment_id: Uuid,
    pub kind: DeploymentKind,
    pub mode: ActivationMode,
    /// `None` for a rollback.
    pub store_path: Option<String>,
    pub signatures: Vec<String>,
}

/// How a machine's part of a deployment ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineOutcome {
    pub status: MachineDeploymentStatus,
    pub message: Option<String>,
    pub report: Option<ActivationReport>,
}

/// What [`Store::finish_deployment_machine`] changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished {
    /// False if the machine had already finished; nothing changed.
    pub machine: bool,
    /// The deployment's final status, if this was its last machine.
    pub deployment: Option<DeploymentStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLogEntry {
    pub deployment_id: Uuid,
    pub machine_id: Option<Uuid>,
    pub source: String,
    pub text: String,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub id: i64,
    pub machine_id: Option<Uuid>,
    pub machine_name: Option<String>,
    pub source: String,
    pub text: String,
    pub created_at: OffsetDateTime,
}

/// Where log entries come from.
pub mod log_source {
    /// The server: fetching, building, signing, pushing.
    pub const SERVER: &str = "server";
    /// The agent's activation output.
    pub const AGENT: &str = "agent";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEnrollmentToken {
    pub machine_id: Uuid,
    /// SHA-256 (hex) of the token; the token itself is never stored.
    pub token_hash: String,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}

/// The result of redeeming an enrollment token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redemption {
    /// The token was valid; the machine now has the new credential.
    Enrolled { machine_id: Uuid },
    /// No such token.
    Unknown,
    /// The token was already redeemed.
    AlreadyUsed { machine_id: Uuid },
    /// The token expired before it was redeemed.
    Expired { machine_id: Uuid },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub machine_id: Uuid,
    pub agent_version: String,
    pub facts: Facts,
    pub connected_at: OffsetDateTime,
}

/// A session closed by [`Store::close_stale_sessions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClosedSession {
    pub session_id: Uuid,
    pub machine_id: Uuid,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub machine_id: Option<Uuid>,
    /// What happened, in snake case (`machine_enrolled`, `agent_connected`).
    pub kind: String,
    pub data: serde_json::Value,
    pub created_at: OffsetDateTime,
}

/// Why a session ended.
pub mod disconnect_reason {
    /// The agent or the network closed the connection.
    pub const CLOSED: &str = "closed";
    /// No heartbeat within the timeout.
    pub const HEARTBEAT_TIMEOUT: &str = "heartbeat_timeout";
    /// The agent opened a new session.
    pub const REPLACED: &str = "replaced";
    /// The agent broke the protocol.
    pub const PROTOCOL_ERROR: &str = "protocol_error";
    /// The server is shutting down.
    pub const SERVER_SHUTDOWN: &str = "server_shutdown";
}

#[async_trait]
pub trait Store: Send + Sync + 'static {
    fn backend(&self) -> Backend;

    /// Applies pending migrations. Safe to call on every start.
    async fn migrate(&self) -> Result<()>;

    /// Checks that the database is reachable.
    async fn ping(&self) -> Result<()>;

    /// Lists application tables (excluding migration bookkeeping), sorted by
    /// name.
    async fn schema(&self) -> Result<Vec<Table>>;

    /// Returns the organization called `name`, creating it if needed.
    async fn ensure_organization(&self, name: &str, now: OffsetDateTime) -> Result<Organization>;

    /// Creates a machine that has not enrolled yet. Fails with
    /// [`Error::Conflict`] if the organization already has one with that
    /// name.
    async fn create_machine(
        &self,
        organization_id: Uuid,
        name: &str,
        now: OffsetDateTime,
    ) -> Result<Machine>;

    async fn machine(&self, id: Uuid) -> Result<Option<Machine>>;

    /// The organization's machines, sorted by name.
    async fn machines(&self, organization_id: Uuid) -> Result<Vec<Machine>>;

    /// The machine whose credential hashes to `credential_hash`.
    async fn machine_by_credential(&self, credential_hash: &str) -> Result<Option<Machine>>;

    async fn create_enrollment_token(&self, token: &NewEnrollmentToken) -> Result<()>;

    /// Atomically marks the token used and gives its machine the new
    /// credential, if the token exists, is unused, and has not expired.
    /// Replaces any previous credential.
    async fn redeem_enrollment_token(
        &self,
        token_hash: &str,
        credential_hash: &str,
        now: OffsetDateTime,
    ) -> Result<Redemption>;

    /// Opens a session after the agent's `hello`, closing any session the
    /// machine still has open, and records the facts it sent. Returns the
    /// session ID.
    async fn open_session(&self, session: &NewSession) -> Result<Uuid>;

    /// Records a heartbeat. Returns false if the session is no longer open
    /// (it was replaced or reaped), in which case the connection should end.
    async fn heartbeat(&self, session_id: Uuid, now: OffsetDateTime) -> Result<bool>;

    async fn update_facts(
        &self,
        machine_id: Uuid,
        facts: &Facts,
        now: OffsetDateTime,
    ) -> Result<()>;

    /// Closes a session. Returns false if it was already closed.
    async fn close_session(
        &self,
        session_id: Uuid,
        reason: &str,
        now: OffsetDateTime,
    ) -> Result<bool>;

    /// Closes open sessions whose last heartbeat is older than `cutoff`.
    async fn close_stale_sessions(
        &self,
        cutoff: OffsetDateTime,
        reason: &str,
        now: OffsetDateTime,
    ) -> Result<Vec<ClosedSession>>;

    async fn record_event(&self, event: &Event) -> Result<()>;

    /// A machine's most recent events, newest first.
    async fn machine_events(&self, machine_id: Uuid, limit: u32) -> Result<Vec<Event>>;

    /// Fails with [`Error::Conflict`] if the name is taken.
    async fn create_repository(
        &self,
        organization_id: Uuid,
        name: &str,
        url: &str,
        now: OffsetDateTime,
    ) -> Result<Repository>;

    /// The organization's repositories, sorted by name.
    async fn repositories(&self, organization_id: Uuid) -> Result<Vec<Repository>>;

    async fn repository(&self, id: Uuid) -> Result<Option<Repository>>;

    /// Sets where the machine's system is built from.
    async fn set_machine_configuration(
        &self,
        machine_id: Uuid,
        repository_id: Uuid,
        configuration: &str,
    ) -> Result<()>;

    /// Replaces the machine's tags.
    async fn set_machine_tags(&self, machine_id: Uuid, tags: &[String]) -> Result<()>;

    /// Creates a deployment and its machines. Machines created as
    /// `activating` are released to their agents at once.
    async fn create_deployment(&self, deployment: &NewDeployment) -> Result<()>;

    async fn deployment(&self, id: Uuid) -> Result<Option<Deployment>>;

    /// The organization's most recent deployments, newest first.
    async fn deployments(&self, organization_id: Uuid, limit: u32) -> Result<Vec<Deployment>>;

    /// Takes the oldest queued deployment for building (status `building`).
    /// Safe to call from several servers at once: each deployment is
    /// claimed once.
    async fn claim_deployment(&self, now: OffsetDateTime) -> Result<Option<Uuid>>;

    async fn set_deployment_commit(&self, id: Uuid, commit_id: &str) -> Result<()>;

    /// Moves an unfinished deployment to `activating`.
    async fn start_activating(&self, id: Uuid) -> Result<()>;

    /// Fails a deployment and every machine in it that has not finished.
    async fn fail_deployment(&self, id: Uuid, message: &str, now: OffsetDateTime) -> Result<()>;

    /// Sets an unfinished machine's status (`building`).
    async fn set_deployment_machine_status(
        &self,
        deployment_id: Uuid,
        machine_id: Uuid,
        status: MachineDeploymentStatus,
    ) -> Result<()>;

    /// Hands a built system to the machine's agent: status `activating`.
    async fn release_activation(
        &self,
        deployment_id: Uuid,
        machine_id: Uuid,
        store_path: &str,
        signatures: &[String],
        now: OffsetDateTime,
    ) -> Result<()>;

    /// Activations for the machine that its agent has not acknowledged,
    /// oldest first.
    async fn pending_activations(&self, machine_id: Uuid) -> Result<Vec<PendingActivation>>;

    /// Records the phase an agent reported, which also acknowledges the
    /// request. Returns false if the machine is not activating (it finished
    /// or timed out), in which case it must not be confirmed.
    async fn record_activation_phase(
        &self,
        deployment_id: Uuid,
        machine_id: Uuid,
        phase: ActivationPhase,
        now: OffsetDateTime,
    ) -> Result<bool>;

    /// Finishes a machine's part of a deployment, unless it already
    /// finished, and finishes the deployment with its last machine. The
    /// report's store path is recorded for rollbacks.
    async fn finish_deployment_machine(
        &self,
        deployment_id: Uuid,
        machine_id: Uuid,
        outcome: &MachineOutcome,
        now: OffsetDateTime,
    ) -> Result<Finished>;

    /// Machines still activating that were released before `cutoff`.
    async fn expired_activations(&self, cutoff: OffsetDateTime) -> Result<Vec<(Uuid, Uuid)>>;

    /// Deployments still building that started before `cutoff`.
    async fn stale_builds(&self, cutoff: OffsetDateTime) -> Result<Vec<Uuid>>;

    async fn append_log(&self, entry: &NewLogEntry) -> Result<()>;

    /// Appends the part of an agent's log chunk (bytes `offset..` of its
    /// activation log) that has not been received before.
    async fn append_agent_log(
        &self,
        deployment_id: Uuid,
        machine_id: Uuid,
        offset: u64,
        text: &str,
        now: OffsetDateTime,
    ) -> Result<()>;

    /// Log entries with IDs greater than `after`, oldest first.
    async fn deployment_logs(
        &self,
        deployment_id: Uuid,
        after: i64,
        limit: u32,
    ) -> Result<Vec<LogEntry>>;
}

/// The part of an agent's log chunk at `offset` that is new when
/// `received` bytes have arrived already, and the new received length.
/// Chunks never overlap a character boundary (the agent sends whole lines),
/// but a cut inside one moves forward to the next boundary.
pub fn new_log_text(received: u64, offset: u64, text: &str) -> Option<(&str, u64)> {
    let end = offset + text.len() as u64;
    if end <= received {
        return None;
    }
    let mut skip = usize::try_from(received.saturating_sub(offset)).ok()?;
    while !text.is_char_boundary(skip) {
        skip += 1;
    }
    Some((&text[skip..], end))
}

/// Parses a status or other enum stored as text.
pub fn parse_enum<T>(value: &str, parse: impl FnOnce(&str) -> Option<T>) -> Result<T> {
    parse(value).ok_or_else(|| Error::Corrupt(format!("unexpected value {value:?}")))
}

/// Name of the table sqlx uses to track applied migrations.
pub const MIGRATIONS_TABLE: &str = "_sqlx_migrations";

/// Both backends store microseconds; truncate so values round-trip exactly.
pub fn truncate_to_micros(t: OffsetDateTime) -> OffsetDateTime {
    let t = t.to_offset(time::UtcOffset::UTC);
    t.replace_nanosecond(t.nanosecond() / 1_000 * 1_000)
        .expect("truncated nanoseconds are in range")
}

pub fn group_columns(rows: Vec<(String, String)>) -> Vec<Table> {
    let mut tables: Vec<Table> = Vec::new();
    for (table, column) in rows {
        match tables.last_mut() {
            Some(last) if last.name == table => last.columns.push(column),
            _ => tables.push(Table {
                name: table,
                columns: vec![column],
            }),
        }
    }
    // Databases may collate names differently; compare in byte order.
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    tables
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_is_chosen_by_scheme() {
        assert_eq!(
            Backend::from_url("sqlite:///var/lib/manage-nix/db.sqlite").unwrap(),
            Backend::Sqlite
        );
        assert_eq!(
            Backend::from_url("postgres://localhost/mnix").unwrap(),
            Backend::Postgres
        );
        assert_eq!(
            Backend::from_url("postgresql://localhost/mnix").unwrap(),
            Backend::Postgres
        );
        assert!(matches!(
            Backend::from_url("mysql://localhost/mnix"),
            Err(Error::UnsupportedUrl(_))
        ));
        assert!(Backend::from_url("no-scheme").is_err());
    }

    #[test]
    fn agent_logs_keep_only_new_bytes() {
        assert_eq!(new_log_text(0, 0, "abc"), Some(("abc", 3)));
        assert_eq!(new_log_text(3, 0, "abc"), None);
        assert_eq!(new_log_text(3, 0, "abcdef"), Some(("def", 6)));
        assert_eq!(new_log_text(3, 3, "def"), Some(("def", 6)));
        // A gap: keep it all.
        assert_eq!(new_log_text(3, 10, "xyz"), Some(("xyz", 13)));
        // Never split a character.
        assert_eq!(new_log_text(1, 0, "\u{e9}x"), Some(("x", 3)));
    }

    #[test]
    fn truncates_to_microseconds_in_utc() {
        let t = time::macros::datetime!(2026-10-22 10:00:00.123456789 +02:00);
        assert_eq!(
            truncate_to_micros(t),
            time::macros::datetime!(2026-10-22 08:00:00.123456 UTC)
        );
    }
}
