//! The [`Store`] trait and the records it stores, shared by the SQLite and
//! Postgres backends (`mnix-store-sqlite`, `mnix-store-postgres`).
//!
//! The backends live in separate crates because sqlx's checked query macros
//! pick the database from one `DATABASE_URL` per crate, so each backend's
//! offline query data (`.sqlx/`) is prepared against its own database.
//! Server code should depend on the `mnix-store` facade instead.

use async_trait::async_trait;
use mnix_protocol::Facts;
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
    fn truncates_to_microseconds_in_utc() {
        let t = time::macros::datetime!(2026-10-22 10:00:00.123456789 +02:00);
        assert_eq!(
            truncate_to_micros(t),
            time::macros::datetime!(2026-10-22 08:00:00.123456 UTC)
        );
    }
}
