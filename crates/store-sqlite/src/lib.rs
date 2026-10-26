//! The SQLite backend: one file, one server instance.
//!
//! SQLite has no UUID, timestamp, or JSON types, so this backend stores them
//! as text: hyphenated UUIDs, fixed-width UTC timestamps (which sort and
//! compare correctly as text), and serialized JSON.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use mnix_protocol::Facts;
use mnix_store_core::{
    disconnect_reason, group_columns, truncate_to_micros, Backend, ClosedSession, Error, Event,
    Machine, NewEnrollmentToken, NewSession, Organization, Redemption, Result, Store, Table,
    MIGRATIONS_TABLE,
};
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use time::format_description::FormatItem;
use time::OffsetDateTime;
use uuid::Uuid;

static MIGRATOR: Migrator = sqlx::migrate!("../../migrations/sqlite");

/// How long a connection waits for the write lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with(SqliteConnectOptions::from_str(url)?).await
    }

    /// Opens the database with the settings manage.nix requires (WAL,
    /// foreign keys, busy timeout), creating the file if it is missing.
    pub async fn connect_with(options: SqliteConnectOptions) -> Result<Self> {
        let options = options
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// `2026-10-22T08:00:00.123456Z`: always UTC and always the same width.
const TIMESTAMP_FORMAT: &[FormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]Z"
);

fn ts(t: OffsetDateTime) -> String {
    truncate_to_micros(t)
        .format(TIMESTAMP_FORMAT)
        .expect("UTC timestamps always format")
}

fn parse_ts(s: &str) -> Result<OffsetDateTime> {
    time::PrimitiveDateTime::parse(s, TIMESTAMP_FORMAT)
        .map(|t| t.assume_utc())
        .map_err(|e| Error::Corrupt(format!("timestamp {s:?}: {e}")))
}

fn parse_opt_ts(s: Option<String>) -> Result<Option<OffsetDateTime>> {
    s.as_deref().map(parse_ts).transpose()
}

fn id(u: Uuid) -> String {
    u.hyphenated().to_string()
}

fn parse_id(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| Error::Corrupt(format!("UUID {s:?}: {e}")))
}

struct MachineRow {
    id: String,
    organization_id: String,
    name: String,
    created_at: String,
    enrolled_at: Option<String>,
    agent_version: Option<String>,
    facts: Option<String>,
    facts_updated_at: Option<String>,
    last_seen_at: Option<String>,
    connected: bool,
}

impl TryFrom<MachineRow> for Machine {
    type Error = Error;

    fn try_from(row: MachineRow) -> Result<Self> {
        Ok(Self {
            id: parse_id(&row.id)?,
            organization_id: parse_id(&row.organization_id)?,
            name: row.name,
            created_at: parse_ts(&row.created_at)?,
            enrolled_at: parse_opt_ts(row.enrolled_at)?,
            agent_version: row.agent_version,
            facts: row.facts.as_deref().map(serde_json::from_str).transpose()?,
            facts_updated_at: parse_opt_ts(row.facts_updated_at)?,
            last_seen_at: parse_opt_ts(row.last_seen_at)?,
            connected: row.connected,
        })
    }
}

/// Selects a [`MachineRow`] with `connected` computed from open sessions;
/// append a `WHERE` clause.
macro_rules! select_machines {
    ($($where:tt)*) => {
        sqlx::query_as!(
            MachineRow,
            r#"SELECT id, organization_id, name, created_at, enrolled_at, agent_version,
                      facts, facts_updated_at, last_seen_at,
                      EXISTS (
                          SELECT 1 FROM agent_sessions AS s
                          WHERE s.machine_id = machines.id AND s.disconnected_at IS NULL
                      ) AS "connected!: bool"
               FROM machines "# + $($where)*
        )
    };
}

#[async_trait]
impl Store for SqliteStore {
    fn backend(&self) -> Backend {
        Backend::Sqlite
    }

    async fn migrate(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    async fn schema(&self) -> Result<Vec<Table>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT t.name, c.name
             FROM sqlite_schema AS t, pragma_table_info(t.name) AS c
             WHERE t.type = 'table' AND t.name NOT LIKE 'sqlite_%' AND t.name <> ?
             ORDER BY t.name, c.cid",
        )
        .bind(MIGRATIONS_TABLE)
        .fetch_all(&self.pool)
        .await?;
        Ok(group_columns(rows))
    }

    async fn ensure_organization(&self, name: &str, now: OffsetDateTime) -> Result<Organization> {
        let (new_id, now) = (id(Uuid::new_v4()), ts(now));
        sqlx::query!(
            "INSERT INTO organizations (id, name, created_at) VALUES (?, ?, ?)
             ON CONFLICT (name) DO NOTHING",
            new_id,
            name,
            now
        )
        .execute(&self.pool)
        .await?;
        let row = sqlx::query!(
            "SELECT id, name, created_at FROM organizations WHERE name = ?",
            name
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(Organization {
            id: parse_id(&row.id)?,
            name: row.name,
            created_at: parse_ts(&row.created_at)?,
        })
    }

    async fn create_machine(
        &self,
        organization_id: Uuid,
        name: &str,
        now: OffsetDateTime,
    ) -> Result<Machine> {
        let machine_id = Uuid::new_v4();
        let (machine_id_s, org, now_s) = (id(machine_id), id(organization_id), ts(now));
        sqlx::query!(
            "INSERT INTO machines (id, organization_id, name, created_at) VALUES (?, ?, ?, ?)",
            machine_id_s,
            org,
            name,
            now_s
        )
        .execute(&self.pool)
        .await
        .map_err(|e| match Error::from(e) {
            Error::Conflict(_) => Error::Conflict(format!("machine {name:?}")),
            e => e,
        })?;
        Ok(Machine {
            id: machine_id,
            organization_id,
            name: name.to_owned(),
            created_at: truncate_to_micros(now),
            enrolled_at: None,
            agent_version: None,
            facts: None,
            facts_updated_at: None,
            last_seen_at: None,
            connected: false,
        })
    }

    async fn machine(&self, machine_id: Uuid) -> Result<Option<Machine>> {
        let machine_id = id(machine_id);
        select_machines!("WHERE id = ?", machine_id)
            .fetch_optional(&self.pool)
            .await?
            .map(Machine::try_from)
            .transpose()
    }

    async fn machines(&self, organization_id: Uuid) -> Result<Vec<Machine>> {
        let org = id(organization_id);
        select_machines!("WHERE organization_id = ? ORDER BY name", org)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(Machine::try_from)
            .collect()
    }

    async fn machine_by_credential(&self, credential_hash: &str) -> Result<Option<Machine>> {
        select_machines!("WHERE credential_hash = ?", credential_hash)
            .fetch_optional(&self.pool)
            .await?
            .map(Machine::try_from)
            .transpose()
    }

    async fn create_enrollment_token(&self, token: &NewEnrollmentToken) -> Result<()> {
        let (token_id, machine_id) = (id(Uuid::new_v4()), id(token.machine_id));
        let (created_at, expires_at) = (ts(token.created_at), ts(token.expires_at));
        sqlx::query!(
            "INSERT INTO enrollment_tokens (id, machine_id, token_hash, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?)",
            token_id,
            machine_id,
            token.token_hash,
            created_at,
            expires_at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn redeem_enrollment_token(
        &self,
        token_hash: &str,
        credential_hash: &str,
        now: OffsetDateTime,
    ) -> Result<Redemption> {
        let now = ts(now);
        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query_scalar!(
            "UPDATE enrollment_tokens SET used_at = ?
             WHERE token_hash = ? AND used_at IS NULL AND expires_at > ?
             RETURNING machine_id",
            now,
            token_hash,
            now
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(machine_id) = claimed else {
            let token = sqlx::query!(
                "SELECT machine_id, used_at FROM enrollment_tokens WHERE token_hash = ?",
                token_hash
            )
            .fetch_optional(&mut *tx)
            .await?;
            return Ok(match token {
                None => Redemption::Unknown,
                Some(t) if t.used_at.is_some() => Redemption::AlreadyUsed {
                    machine_id: parse_id(&t.machine_id)?,
                },
                Some(t) => Redemption::Expired {
                    machine_id: parse_id(&t.machine_id)?,
                },
            });
        };
        sqlx::query!(
            "UPDATE machines SET credential_hash = ?, enrolled_at = ? WHERE id = ?",
            credential_hash,
            now,
            machine_id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Redemption::Enrolled {
            machine_id: parse_id(&machine_id)?,
        })
    }

    async fn open_session(&self, session: &NewSession) -> Result<Uuid> {
        let session_id = Uuid::new_v4();
        let (session_id_s, machine_id) = (id(session_id), id(session.machine_id));
        let now = ts(session.connected_at);
        let facts = serde_json::to_string(&session.facts)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "UPDATE agent_sessions SET disconnected_at = ?, disconnect_reason = ?
             WHERE machine_id = ? AND disconnected_at IS NULL",
            now,
            disconnect_reason::REPLACED,
            machine_id
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "INSERT INTO agent_sessions (id, machine_id, agent_version, connected_at, last_heartbeat_at)
             VALUES (?, ?, ?, ?, ?)",
            session_id_s,
            machine_id,
            session.agent_version,
            now,
            now
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE machines
             SET agent_version = ?, facts = ?, facts_updated_at = ?, last_seen_at = ?
             WHERE id = ?",
            session.agent_version,
            facts,
            now,
            now,
            machine_id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(session_id)
    }

    async fn heartbeat(&self, session_id: Uuid, now: OffsetDateTime) -> Result<bool> {
        let (session_id, now) = (id(session_id), ts(now));
        let mut tx = self.pool.begin().await?;
        let machine_id = sqlx::query_scalar!(
            "UPDATE agent_sessions SET last_heartbeat_at = ?
             WHERE id = ? AND disconnected_at IS NULL
             RETURNING machine_id",
            now,
            session_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(machine_id) = machine_id else {
            return Ok(false);
        };
        sqlx::query!(
            "UPDATE machines SET last_seen_at = ? WHERE id = ?",
            now,
            machine_id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn update_facts(
        &self,
        machine_id: Uuid,
        facts: &Facts,
        now: OffsetDateTime,
    ) -> Result<()> {
        let (machine_id, now) = (id(machine_id), ts(now));
        let facts = serde_json::to_string(facts)?;
        sqlx::query!(
            "UPDATE machines SET facts = ?, facts_updated_at = ? WHERE id = ?",
            facts,
            now,
            machine_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn close_session(
        &self,
        session_id: Uuid,
        reason: &str,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let (session_id, now) = (id(session_id), ts(now));
        let result = sqlx::query!(
            "UPDATE agent_sessions SET disconnected_at = ?, disconnect_reason = ?
             WHERE id = ? AND disconnected_at IS NULL",
            now,
            reason,
            session_id
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn close_stale_sessions(
        &self,
        cutoff: OffsetDateTime,
        reason: &str,
        now: OffsetDateTime,
    ) -> Result<Vec<ClosedSession>> {
        let (cutoff, now) = (ts(cutoff), ts(now));
        sqlx::query!(
            "UPDATE agent_sessions SET disconnected_at = ?, disconnect_reason = ?
             WHERE disconnected_at IS NULL AND last_heartbeat_at < ?
             RETURNING id, machine_id",
            now,
            reason,
            cutoff
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(ClosedSession {
                session_id: parse_id(&row.id)?,
                machine_id: parse_id(&row.machine_id)?,
            })
        })
        .collect()
    }

    async fn record_event(&self, event: &Event) -> Result<()> {
        let (event_id, org) = (id(event.id), id(event.organization_id));
        let machine_id = event.machine_id.map(id);
        let data = serde_json::to_string(&event.data)?;
        let created_at = ts(event.created_at);
        sqlx::query!(
            "INSERT INTO events (id, organization_id, machine_id, kind, data, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            event_id,
            org,
            machine_id,
            event.kind,
            data,
            created_at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn machine_events(&self, machine_id: Uuid, limit: u32) -> Result<Vec<Event>> {
        let machine_id = id(machine_id);
        sqlx::query!(
            "SELECT id, organization_id, machine_id, kind, data, created_at
             FROM events WHERE machine_id = ?
             ORDER BY created_at DESC, rowid DESC LIMIT ?",
            machine_id,
            limit
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(Event {
                id: parse_id(&row.id)?,
                organization_id: parse_id(&row.organization_id)?,
                machine_id: row.machine_id.as_deref().map(parse_id).transpose()?,
                kind: row.kind,
                data: serde_json::from_str(&row.data)?,
                created_at: parse_ts(&row.created_at)?,
            })
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_have_a_fixed_width_and_round_trip() {
        let t = time::macros::datetime!(2026-10-22 10:00:00.5 +02:00);
        assert_eq!(ts(t), "2026-10-22T08:00:00.500000Z");
        assert_eq!(parse_ts(&ts(t)).unwrap(), t);
        let whole = time::macros::datetime!(2026-10-22 08:00:00 UTC);
        assert_eq!(ts(whole).len(), ts(t).len());
    }
}
