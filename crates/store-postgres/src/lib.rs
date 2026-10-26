//! The Postgres backend, for teams and replicated deployments.

use std::str::FromStr;

use async_trait::async_trait;
use mnix_protocol::Facts;
use mnix_store_core::{
    disconnect_reason, group_columns, truncate_to_micros, Backend, ClosedSession, Error, Event,
    Machine, NewEnrollmentToken, NewSession, Organization, Redemption, Result, Store, Table,
    MIGRATIONS_TABLE,
};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::types::Json;
use time::OffsetDateTime;
use uuid::Uuid;

static MIGRATOR: Migrator = sqlx::migrate!("../../migrations/postgres");

#[derive(Debug, Clone)]
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with(PgConnectOptions::from_str(url)?).await
    }

    pub async fn connect_with(options: PgConnectOptions) -> Result<Self> {
        let pool = PgPoolOptions::new().connect_with(options).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

struct MachineRow {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    created_at: OffsetDateTime,
    enrolled_at: Option<OffsetDateTime>,
    agent_version: Option<String>,
    facts: Option<Json<Facts>>,
    facts_updated_at: Option<OffsetDateTime>,
    last_seen_at: Option<OffsetDateTime>,
    connected: bool,
}

impl From<MachineRow> for Machine {
    fn from(row: MachineRow) -> Self {
        Self {
            id: row.id,
            organization_id: row.organization_id,
            name: row.name,
            created_at: row.created_at,
            enrolled_at: row.enrolled_at,
            agent_version: row.agent_version,
            facts: row.facts.map(|Json(facts)| facts),
            facts_updated_at: row.facts_updated_at,
            last_seen_at: row.last_seen_at,
            connected: row.connected,
        }
    }
}

/// Selects a [`MachineRow`] with `connected` computed from open sessions;
/// append a `WHERE` clause.
macro_rules! select_machines {
    ($($where:tt)*) => {
        sqlx::query_as!(
            MachineRow,
            r#"SELECT id, organization_id, name, created_at, enrolled_at, agent_version,
                      facts AS "facts: Json<Facts>", facts_updated_at, last_seen_at,
                      EXISTS (
                          SELECT 1 FROM agent_sessions AS s
                          WHERE s.machine_id = machines.id AND s.disconnected_at IS NULL
                      ) AS "connected!"
               FROM machines "# + $($where)*
        )
    };
}

#[async_trait]
impl Store for PostgresStore {
    fn backend(&self) -> Backend {
        Backend::Postgres
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
            "SELECT c.table_name::text, c.column_name::text
             FROM information_schema.columns AS c
             JOIN information_schema.tables AS t
               ON t.table_schema = c.table_schema AND t.table_name = c.table_name
             WHERE c.table_schema = current_schema()
               AND t.table_type = 'BASE TABLE'
               AND c.table_name <> $1
             ORDER BY c.table_name, c.ordinal_position",
        )
        .bind(MIGRATIONS_TABLE)
        .fetch_all(&self.pool)
        .await?;
        Ok(group_columns(rows))
    }

    async fn ensure_organization(&self, name: &str, now: OffsetDateTime) -> Result<Organization> {
        sqlx::query!(
            "INSERT INTO organizations (id, name, created_at) VALUES ($1, $2, $3)
             ON CONFLICT (name) DO NOTHING",
            Uuid::new_v4(),
            name,
            truncate_to_micros(now)
        )
        .execute(&self.pool)
        .await?;
        Ok(sqlx::query_as!(
            Organization,
            "SELECT id, name, created_at FROM organizations WHERE name = $1",
            name
        )
        .fetch_one(&self.pool)
        .await?)
    }

    async fn create_machine(
        &self,
        organization_id: Uuid,
        name: &str,
        now: OffsetDateTime,
    ) -> Result<Machine> {
        let machine_id = Uuid::new_v4();
        let now = truncate_to_micros(now);
        sqlx::query!(
            "INSERT INTO machines (id, organization_id, name, created_at) VALUES ($1, $2, $3, $4)",
            machine_id,
            organization_id,
            name,
            now
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
            created_at: now,
            enrolled_at: None,
            agent_version: None,
            facts: None,
            facts_updated_at: None,
            last_seen_at: None,
            connected: false,
        })
    }

    async fn machine(&self, machine_id: Uuid) -> Result<Option<Machine>> {
        Ok(select_machines!("WHERE id = $1", machine_id)
            .fetch_optional(&self.pool)
            .await?
            .map(Machine::from))
    }

    async fn machines(&self, organization_id: Uuid) -> Result<Vec<Machine>> {
        // `COLLATE "C"` sorts in byte order, like the SQLite backend.
        Ok(select_machines!(
            r#"WHERE organization_id = $1 ORDER BY name COLLATE "C""#,
            organization_id
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Machine::from)
        .collect())
    }

    async fn machine_by_credential(&self, credential_hash: &str) -> Result<Option<Machine>> {
        Ok(
            select_machines!("WHERE credential_hash = $1", credential_hash)
                .fetch_optional(&self.pool)
                .await?
                .map(Machine::from),
        )
    }

    async fn create_enrollment_token(&self, token: &NewEnrollmentToken) -> Result<()> {
        sqlx::query!(
            "INSERT INTO enrollment_tokens (id, machine_id, token_hash, created_at, expires_at)
             VALUES ($1, $2, $3, $4, $5)",
            Uuid::new_v4(),
            token.machine_id,
            token.token_hash,
            truncate_to_micros(token.created_at),
            truncate_to_micros(token.expires_at)
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
        let now = truncate_to_micros(now);
        let mut tx = self.pool.begin().await?;
        // The row lock taken by the UPDATE makes concurrent redemptions of
        // the same token wait, and the second then sees `used_at` set.
        let claimed = sqlx::query_scalar!(
            "UPDATE enrollment_tokens SET used_at = $1
             WHERE token_hash = $2 AND used_at IS NULL AND expires_at > $1
             RETURNING machine_id",
            now,
            token_hash
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(machine_id) = claimed else {
            let token = sqlx::query!(
                "SELECT machine_id, used_at FROM enrollment_tokens WHERE token_hash = $1",
                token_hash
            )
            .fetch_optional(&mut *tx)
            .await?;
            return Ok(match token {
                None => Redemption::Unknown,
                Some(t) if t.used_at.is_some() => Redemption::AlreadyUsed {
                    machine_id: t.machine_id,
                },
                Some(t) => Redemption::Expired {
                    machine_id: t.machine_id,
                },
            });
        };
        sqlx::query!(
            "UPDATE machines SET credential_hash = $1, enrolled_at = $2 WHERE id = $3",
            credential_hash,
            now,
            machine_id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Redemption::Enrolled { machine_id })
    }

    async fn open_session(&self, session: &NewSession) -> Result<Uuid> {
        let session_id = Uuid::new_v4();
        let now = truncate_to_micros(session.connected_at);
        let mut tx = self.pool.begin().await?;
        // Lock the machine row so concurrent `hello`s from the same machine
        // (on different replicas) leave exactly one session open.
        sqlx::query!(
            "SELECT id FROM machines WHERE id = $1 FOR UPDATE",
            session.machine_id
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE agent_sessions SET disconnected_at = $1, disconnect_reason = $2
             WHERE machine_id = $3 AND disconnected_at IS NULL",
            now,
            disconnect_reason::REPLACED,
            session.machine_id
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "INSERT INTO agent_sessions (id, machine_id, agent_version, connected_at, last_heartbeat_at)
             VALUES ($1, $2, $3, $4, $4)",
            session_id,
            session.machine_id,
            session.agent_version,
            now
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE machines
             SET agent_version = $1, facts = $2, facts_updated_at = $3, last_seen_at = $3
             WHERE id = $4",
            session.agent_version,
            Json(&session.facts) as _,
            now,
            session.machine_id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(session_id)
    }

    async fn heartbeat(&self, session_id: Uuid, now: OffsetDateTime) -> Result<bool> {
        let now = truncate_to_micros(now);
        let mut tx = self.pool.begin().await?;
        let machine_id = sqlx::query_scalar!(
            "UPDATE agent_sessions SET last_heartbeat_at = $1
             WHERE id = $2 AND disconnected_at IS NULL
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
            "UPDATE machines SET last_seen_at = $1 WHERE id = $2",
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
        sqlx::query!(
            "UPDATE machines SET facts = $1, facts_updated_at = $2 WHERE id = $3",
            Json(facts) as _,
            truncate_to_micros(now),
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
        let result = sqlx::query!(
            "UPDATE agent_sessions SET disconnected_at = $1, disconnect_reason = $2
             WHERE id = $3 AND disconnected_at IS NULL",
            truncate_to_micros(now),
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
        Ok(sqlx::query!(
            "UPDATE agent_sessions SET disconnected_at = $1, disconnect_reason = $2
             WHERE disconnected_at IS NULL AND last_heartbeat_at < $3
             RETURNING id, machine_id",
            truncate_to_micros(now),
            reason,
            truncate_to_micros(cutoff)
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| ClosedSession {
            session_id: row.id,
            machine_id: row.machine_id,
        })
        .collect())
    }

    async fn record_event(&self, event: &Event) -> Result<()> {
        sqlx::query!(
            "INSERT INTO events (id, organization_id, machine_id, kind, data, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
            event.id,
            event.organization_id,
            event.machine_id,
            event.kind,
            event.data,
            truncate_to_micros(event.created_at)
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn machine_events(&self, machine_id: Uuid, limit: u32) -> Result<Vec<Event>> {
        Ok(sqlx::query_as!(
            Event,
            "SELECT id, organization_id, machine_id, kind, data, created_at
             FROM events WHERE machine_id = $1
             ORDER BY created_at DESC LIMIT $2",
            machine_id,
            i64::from(limit)
        )
        .fetch_all(&self.pool)
        .await?)
    }
}
