//! The SQLite backend: one file, one server instance.
//!
//! SQLite has no UUID, timestamp, or JSON types, so this backend stores them
//! as text: hyphenated UUIDs, fixed-width UTC timestamps (which sort and
//! compare correctly as text), and serialized JSON.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use mnix_protocol::api::{DeploymentKind, DeploymentStatus, MachineDeploymentStatus};
use mnix_protocol::{ActivationMode, ActivationPhase, Facts};
use mnix_store_core::{
    disconnect_reason, group_columns, log_source, new_log_text, parse_enum, truncate_to_micros,
    Backend, ClosedSession, Deployment, DeploymentMachine, Error, Event, Finished, LogEntry,
    Machine, MachineDeploymentRecord, MachineOutcome, NewDeployment, NewEnrollmentToken,
    NewLogEntry, NewSession, NewUserSession, Organization, PendingActivation, Redemption,
    Repository, Result, Store, SystemRevision, Table, User, MIGRATIONS_TABLE,
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
    repository_id: Option<String>,
    configuration: Option<String>,
    /// A JSON array.
    tags: String,
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
            repository_id: row.repository_id.as_deref().map(parse_id).transpose()?,
            configuration: row.configuration,
            tags: serde_json::from_str(&row.tags)?,
        })
    }
}

struct RepositoryRow {
    id: String,
    organization_id: String,
    name: String,
    url: String,
    created_at: String,
}

impl TryFrom<RepositoryRow> for Repository {
    type Error = Error;

    fn try_from(row: RepositoryRow) -> Result<Self> {
        Ok(Self {
            id: parse_id(&row.id)?,
            organization_id: parse_id(&row.organization_id)?,
            name: row.name,
            url: row.url,
            created_at: parse_ts(&row.created_at)?,
        })
    }
}

struct DeploymentRow {
    id: String,
    organization_id: String,
    kind: String,
    status: String,
    message: Option<String>,
    repository_id: Option<String>,
    rev: Option<String>,
    commit_id: Option<String>,
    mode: String,
    requested_by: String,
    created_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
}

struct DeploymentMachineRow {
    machine_id: String,
    machine_name: String,
    configuration: Option<String>,
    status: String,
    phase: Option<String>,
    store_path: Option<String>,
    signatures: String,
    message: Option<String>,
    report: Option<String>,
    released_at: Option<String>,
    accepted_at: Option<String>,
    finished_at: Option<String>,
}

impl TryFrom<DeploymentMachineRow> for DeploymentMachine {
    type Error = Error;

    fn try_from(row: DeploymentMachineRow) -> Result<Self> {
        Ok(Self {
            machine_id: parse_id(&row.machine_id)?,
            machine_name: row.machine_name,
            configuration: row.configuration,
            status: parse_enum(&row.status, MachineDeploymentStatus::parse)?,
            phase: row
                .phase
                .as_deref()
                .map(|p| parse_enum(p, ActivationPhase::parse))
                .transpose()?,
            store_path: row.store_path,
            signatures: serde_json::from_str(&row.signatures)?,
            message: row.message,
            report: row
                .report
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
            released_at: parse_opt_ts(row.released_at)?,
            accepted_at: parse_opt_ts(row.accepted_at)?,
            finished_at: parse_opt_ts(row.finished_at)?,
        })
    }
}

impl SqliteStore {
    async fn load_deployment(&self, row: DeploymentRow) -> Result<Deployment> {
        let machines = sqlx::query_as!(
            DeploymentMachineRow,
            r#"SELECT dm.machine_id, m.name AS machine_name, dm.configuration, dm.status,
                      dm.phase, dm.store_path, dm.signatures, dm.message, dm.report,
                      dm.released_at, dm.accepted_at, dm.finished_at
               FROM deployment_machines AS dm JOIN machines AS m ON m.id = dm.machine_id
               WHERE dm.deployment_id = ?
               ORDER BY m.name"#,
            row.id
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(DeploymentMachine::try_from)
        .collect::<Result<_>>()?;
        Ok(Deployment {
            id: parse_id(&row.id)?,
            organization_id: parse_id(&row.organization_id)?,
            kind: parse_enum(&row.kind, DeploymentKind::parse)?,
            status: parse_enum(&row.status, DeploymentStatus::parse)?,
            message: row.message,
            repository_id: row.repository_id.as_deref().map(parse_id).transpose()?,
            rev: row.rev,
            commit_id: row.commit_id,
            mode: parse_enum(&row.mode, ActivationMode::parse)?,
            requested_by: row.requested_by,
            created_at: parse_ts(&row.created_at)?,
            started_at: parse_opt_ts(row.started_at)?,
            finished_at: parse_opt_ts(row.finished_at)?,
            machines,
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
                      ) AS "connected!: bool",
                      repository_id, configuration,
                      (SELECT json_group_array(tag) FROM (
                          SELECT tag FROM machine_tags AS t
                          WHERE t.machine_id = machines.id ORDER BY tag
                      )) AS "tags!: String"
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
            repository_id: None,
            configuration: None,
            tags: Vec::new(),
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

    async fn create_repository(
        &self,
        organization_id: Uuid,
        name: &str,
        url: &str,
        now: OffsetDateTime,
    ) -> Result<Repository> {
        let repository_id = Uuid::new_v4();
        let (repository_id_s, org, now_s) = (id(repository_id), id(organization_id), ts(now));
        sqlx::query!(
            "INSERT INTO repositories (id, organization_id, name, url, created_at)
             VALUES (?, ?, ?, ?, ?)",
            repository_id_s,
            org,
            name,
            url,
            now_s
        )
        .execute(&self.pool)
        .await
        .map_err(|e| match Error::from(e) {
            Error::Conflict(_) => Error::Conflict(format!("repository {name:?}")),
            e => e,
        })?;
        Ok(Repository {
            id: repository_id,
            organization_id,
            name: name.to_owned(),
            url: url.to_owned(),
            created_at: truncate_to_micros(now),
        })
    }

    async fn repositories(&self, organization_id: Uuid) -> Result<Vec<Repository>> {
        let org = id(organization_id);
        sqlx::query_as!(
            RepositoryRow,
            "SELECT id, organization_id, name, url, created_at FROM repositories
             WHERE organization_id = ? ORDER BY name",
            org
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(Repository::try_from)
        .collect()
    }

    async fn repository(&self, repository_id: Uuid) -> Result<Option<Repository>> {
        let repository_id = id(repository_id);
        sqlx::query_as!(
            RepositoryRow,
            "SELECT id, organization_id, name, url, created_at FROM repositories WHERE id = ?",
            repository_id
        )
        .fetch_optional(&self.pool)
        .await?
        .map(Repository::try_from)
        .transpose()
    }

    async fn set_machine_configuration(
        &self,
        machine_id: Uuid,
        repository_id: Uuid,
        configuration: &str,
    ) -> Result<()> {
        let (machine_id, repository_id) = (id(machine_id), id(repository_id));
        sqlx::query!(
            "UPDATE machines SET repository_id = ?, configuration = ? WHERE id = ?",
            repository_id,
            configuration,
            machine_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_machine_tags(&self, machine_id: Uuid, tags: &[String]) -> Result<()> {
        let machine_id = id(machine_id);
        let mut tx = self.pool.begin().await?;
        sqlx::query!("DELETE FROM machine_tags WHERE machine_id = ?", machine_id)
            .execute(&mut *tx)
            .await?;
        for tag in tags {
            sqlx::query!(
                "INSERT INTO machine_tags (machine_id, tag) VALUES (?, ?) ON CONFLICT DO NOTHING",
                machine_id,
                tag
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn create_deployment(&self, d: &NewDeployment) -> Result<()> {
        let (deployment_id, org) = (id(d.id), id(d.organization_id));
        let repository_id = d.repository_id.map(id);
        let (kind, status, mode) = (d.kind.as_str(), d.status.as_str(), d.mode.as_str());
        let now = ts(d.created_at);
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "INSERT INTO deployments (id, organization_id, kind, status, repository_id, rev,
                                      mode, requested_by, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            deployment_id,
            org,
            kind,
            status,
            repository_id,
            d.rev,
            mode,
            d.requested_by,
            now
        )
        .execute(&mut *tx)
        .await?;
        for m in &d.machines {
            let machine_id = id(m.machine_id);
            let status = m.status.as_str();
            let released_at = (m.status == MachineDeploymentStatus::Activating).then_some(&now);
            sqlx::query!(
                "INSERT INTO deployment_machines
                     (deployment_id, machine_id, configuration, status, signatures, released_at)
                 VALUES (?, ?, ?, ?, '[]', ?)",
                deployment_id,
                machine_id,
                m.configuration,
                status,
                released_at
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn deployment(&self, deployment_id: Uuid) -> Result<Option<Deployment>> {
        let deployment_id = id(deployment_id);
        let row = sqlx::query_as!(
            DeploymentRow,
            "SELECT id, organization_id, kind, status, message, repository_id, rev, commit_id,
                    mode, requested_by, created_at, started_at, finished_at
             FROM deployments WHERE id = ?",
            deployment_id
        )
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(row) => Ok(Some(self.load_deployment(row).await?)),
            None => Ok(None),
        }
    }

    async fn deployments(&self, organization_id: Uuid, limit: u32) -> Result<Vec<Deployment>> {
        let org = id(organization_id);
        let rows = sqlx::query_as!(
            DeploymentRow,
            "SELECT id, organization_id, kind, status, message, repository_id, rev, commit_id,
                    mode, requested_by, created_at, started_at, finished_at
             FROM deployments WHERE organization_id = ?
             ORDER BY created_at DESC, rowid DESC LIMIT ?",
            org,
            limit
        )
        .fetch_all(&self.pool)
        .await?;
        let mut deployments = Vec::with_capacity(rows.len());
        for row in rows {
            deployments.push(self.load_deployment(row).await?);
        }
        Ok(deployments)
    }

    async fn claim_deployment(&self, now: OffsetDateTime) -> Result<Option<Uuid>> {
        let now = ts(now);
        // One statement, so one claim even with several connections.
        sqlx::query_scalar!(
            "UPDATE deployments SET status = 'building', started_at = ?
             WHERE id = (
                 SELECT id FROM deployments WHERE status = 'queued'
                 ORDER BY created_at, rowid LIMIT 1
             )
             RETURNING id",
            now
        )
        .fetch_optional(&self.pool)
        .await?
        .as_deref()
        .map(parse_id)
        .transpose()
    }

    async fn set_deployment_commit(&self, deployment_id: Uuid, commit_id: &str) -> Result<()> {
        let deployment_id = id(deployment_id);
        sqlx::query!(
            "UPDATE deployments SET commit_id = ? WHERE id = ?",
            commit_id,
            deployment_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn start_activating(&self, deployment_id: Uuid) -> Result<()> {
        let deployment_id = id(deployment_id);
        sqlx::query!(
            "UPDATE deployments SET status = 'activating'
             WHERE id = ? AND finished_at IS NULL",
            deployment_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn fail_deployment(
        &self,
        deployment_id: Uuid,
        message: &str,
        now: OffsetDateTime,
    ) -> Result<()> {
        let (deployment_id, now) = (id(deployment_id), ts(now));
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "UPDATE deployment_machines SET status = 'failed', message = ?, finished_at = ?
             WHERE deployment_id = ? AND finished_at IS NULL",
            message,
            now,
            deployment_id
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE deployments SET status = 'failed', message = ?, finished_at = ?
             WHERE id = ? AND finished_at IS NULL",
            message,
            now,
            deployment_id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn set_deployment_machine_status(
        &self,
        deployment_id: Uuid,
        machine_id: Uuid,
        status: MachineDeploymentStatus,
    ) -> Result<()> {
        let (deployment_id, machine_id, status) =
            (id(deployment_id), id(machine_id), status.as_str());
        sqlx::query!(
            "UPDATE deployment_machines SET status = ?
             WHERE deployment_id = ? AND machine_id = ? AND finished_at IS NULL",
            status,
            deployment_id,
            machine_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn release_activation(
        &self,
        deployment_id: Uuid,
        machine_id: Uuid,
        store_path: &str,
        signatures: &[String],
        now: OffsetDateTime,
    ) -> Result<()> {
        let (deployment_id, machine_id, now) = (id(deployment_id), id(machine_id), ts(now));
        let signatures = serde_json::to_string(signatures)?;
        sqlx::query!(
            "UPDATE deployment_machines
             SET status = 'activating', store_path = ?, signatures = ?, released_at = ?
             WHERE deployment_id = ? AND machine_id = ? AND finished_at IS NULL",
            store_path,
            signatures,
            now,
            deployment_id,
            machine_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn pending_activations(&self, machine_id: Uuid) -> Result<Vec<PendingActivation>> {
        let machine_id = id(machine_id);
        sqlx::query!(
            "SELECT dm.deployment_id, d.kind, d.mode, dm.store_path, dm.signatures
             FROM deployment_machines AS dm JOIN deployments AS d ON d.id = dm.deployment_id
             WHERE dm.machine_id = ? AND dm.status = 'activating' AND dm.accepted_at IS NULL
             ORDER BY dm.released_at",
            machine_id
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(PendingActivation {
                deployment_id: parse_id(&row.deployment_id)?,
                kind: parse_enum(&row.kind, DeploymentKind::parse)?,
                mode: parse_enum(&row.mode, ActivationMode::parse)?,
                store_path: row.store_path,
                signatures: serde_json::from_str(&row.signatures)?,
            })
        })
        .collect()
    }

    async fn record_activation_phase(
        &self,
        deployment_id: Uuid,
        machine_id: Uuid,
        phase: ActivationPhase,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let (deployment_id, machine_id, now) = (id(deployment_id), id(machine_id), ts(now));
        let phase = phase.as_str();
        let result = sqlx::query!(
            "UPDATE deployment_machines SET phase = ?, accepted_at = COALESCE(accepted_at, ?)
             WHERE deployment_id = ? AND machine_id = ? AND status = 'activating'",
            phase,
            now,
            deployment_id,
            machine_id
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn finish_deployment_machine(
        &self,
        deployment_id: Uuid,
        machine_id: Uuid,
        outcome: &MachineOutcome,
        now: OffsetDateTime,
    ) -> Result<Finished> {
        let (deployment_id, machine_id, now) = (id(deployment_id), id(machine_id), ts(now));
        let status = outcome.status.as_str();
        let report = outcome
            .report
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let store_path = outcome
            .report
            .as_ref()
            .map(|r| r.store_path.as_str())
            .filter(|p| !p.is_empty());
        // The first statement writes, so the transaction holds the write
        // lock from the start and the count below sees every other finish.
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query!(
            "UPDATE deployment_machines
             SET status = ?, message = ?, report = ?, store_path = COALESCE(store_path, ?),
                 finished_at = ?
             WHERE deployment_id = ? AND machine_id = ? AND finished_at IS NULL",
            status,
            outcome.message,
            report,
            store_path,
            now,
            deployment_id,
            machine_id
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if !changed {
            return Ok(Finished {
                machine: false,
                deployment: None,
            });
        }
        let counts = sqlx::query!(
            r#"SELECT COUNT(*) FILTER (WHERE finished_at IS NULL) AS "open!: i64",
                      COUNT(*) FILTER (WHERE status <> 'succeeded') AS "bad!: i64"
               FROM deployment_machines WHERE deployment_id = ?"#,
            deployment_id
        )
        .fetch_one(&mut *tx)
        .await?;
        let mut deployment = None;
        if counts.open == 0 {
            let final_status = match counts.bad {
                0 => DeploymentStatus::Succeeded,
                _ => DeploymentStatus::Failed,
            };
            let final_status_s = final_status.as_str();
            let result = sqlx::query!(
                "UPDATE deployments SET status = ?, finished_at = ?
                 WHERE id = ? AND finished_at IS NULL",
                final_status_s,
                now,
                deployment_id
            )
            .execute(&mut *tx)
            .await?;
            deployment = (result.rows_affected() > 0).then_some(final_status);
        }
        tx.commit().await?;
        Ok(Finished {
            machine: true,
            deployment,
        })
    }

    async fn expired_activations(&self, cutoff: OffsetDateTime) -> Result<Vec<(Uuid, Uuid)>> {
        let cutoff = ts(cutoff);
        sqlx::query!(
            "SELECT deployment_id, machine_id FROM deployment_machines
             WHERE status = 'activating' AND released_at < ?",
            cutoff
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| Ok((parse_id(&row.deployment_id)?, parse_id(&row.machine_id)?)))
        .collect()
    }

    async fn stale_builds(&self, cutoff: OffsetDateTime) -> Result<Vec<Uuid>> {
        let cutoff = ts(cutoff);
        sqlx::query_scalar!(
            "SELECT id FROM deployments WHERE status = 'building' AND started_at < ?",
            cutoff
        )
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|id| parse_id(id))
        .collect()
    }

    async fn append_log(&self, entry: &NewLogEntry) -> Result<()> {
        let deployment_id = id(entry.deployment_id);
        let machine_id = entry.machine_id.map(id);
        let created_at = ts(entry.created_at);
        sqlx::query!(
            "INSERT INTO deployment_logs (deployment_id, machine_id, source, text, created_at)
             VALUES (?, ?, ?, ?, ?)",
            deployment_id,
            machine_id,
            entry.source,
            entry.text,
            created_at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn append_agent_log(
        &self,
        deployment_id: Uuid,
        machine_id: Uuid,
        offset: u64,
        text: &str,
        now: OffsetDateTime,
    ) -> Result<()> {
        let (deployment_id, machine_id, now) = (id(deployment_id), id(machine_id), ts(now));
        let mut tx = self.pool.begin().await?;
        // Take the write lock first (see finish_deployment_machine).
        let received = sqlx::query_scalar!(
            "UPDATE deployment_machines SET log_bytes = log_bytes
             WHERE deployment_id = ? AND machine_id = ?
             RETURNING log_bytes",
            deployment_id,
            machine_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(received) = received else {
            return Ok(());
        };
        let Some((new, end)) = new_log_text(received as u64, offset, text) else {
            return Ok(());
        };
        let end = end as i64;
        sqlx::query!(
            "UPDATE deployment_machines SET log_bytes = ?
             WHERE deployment_id = ? AND machine_id = ?",
            end,
            deployment_id,
            machine_id
        )
        .execute(&mut *tx)
        .await?;
        if !new.is_empty() {
            let source = log_source::AGENT;
            sqlx::query!(
                "INSERT INTO deployment_logs (deployment_id, machine_id, source, text, created_at)
                 VALUES (?, ?, ?, ?, ?)",
                deployment_id,
                machine_id,
                source,
                new,
                now
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn deployment_logs(
        &self,
        deployment_id: Uuid,
        after: i64,
        limit: u32,
    ) -> Result<Vec<LogEntry>> {
        let deployment_id = id(deployment_id);
        sqlx::query!(
            r#"SELECT l.id AS "id!: i64", l.machine_id, m.name AS "machine_name?",
                      l.source, l.text, l.created_at
               FROM deployment_logs AS l LEFT JOIN machines AS m ON m.id = l.machine_id
               WHERE l.deployment_id = ? AND l.id > ?
               ORDER BY l.id LIMIT ?"#,
            deployment_id,
            after,
            limit
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(LogEntry {
                id: row.id,
                machine_id: row.machine_id.as_deref().map(parse_id).transpose()?,
                machine_name: row.machine_name,
                source: row.source,
                text: row.text,
                created_at: parse_ts(&row.created_at)?,
            })
        })
        .collect()
    }

    async fn machine_deployments(
        &self,
        machine_id: Uuid,
        limit: u32,
    ) -> Result<Vec<MachineDeploymentRecord>> {
        let machine_id = id(machine_id);
        sqlx::query_as!(
            MachineDeploymentRow,
            r#"SELECT dm.deployment_id, dm.machine_id, d.kind, d.mode, d.rev, d.commit_id,
                      d.requested_by, dm.status, dm.store_path, dm.message, d.created_at,
                      dm.finished_at
               FROM deployment_machines AS dm JOIN deployments AS d ON d.id = dm.deployment_id
               WHERE dm.machine_id = ?
               ORDER BY d.created_at DESC, d.rowid DESC LIMIT ?"#,
            machine_id,
            limit
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(MachineDeploymentRecord::try_from)
        .collect()
    }

    async fn latest_machine_deployments(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<MachineDeploymentRecord>> {
        let org = id(organization_id);
        sqlx::query_as!(
            MachineDeploymentRow,
            r#"SELECT dm.deployment_id, dm.machine_id, d.kind, d.mode, d.rev, d.commit_id,
                      d.requested_by, dm.status, dm.store_path, dm.message, d.created_at,
                      dm.finished_at
               FROM deployment_machines AS dm JOIN deployments AS d ON d.id = dm.deployment_id
               WHERE d.organization_id = ? AND d.id = (
                   SELECT d2.id
                   FROM deployment_machines AS dm2
                   JOIN deployments AS d2 ON d2.id = dm2.deployment_id
                   WHERE dm2.machine_id = dm.machine_id
                   ORDER BY d2.created_at DESC, d2.rowid DESC LIMIT 1
               )"#,
            org
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(MachineDeploymentRecord::try_from)
        .collect()
    }

    async fn system_revisions(
        &self,
        organization_id: Uuid,
        store_paths: &[String],
    ) -> Result<Vec<SystemRevision>> {
        let org = id(organization_id);
        let paths = serde_json::to_string(store_paths)?;
        let rows = sqlx::query!(
            r#"SELECT dm.store_path AS "store_path!", d.id, d.repository_id, d.rev,
                      d.commit_id AS "commit_id!", d.created_at
               FROM deployment_machines AS dm JOIN deployments AS d ON d.id = dm.deployment_id
               WHERE d.organization_id = ? AND d.kind = 'deploy' AND d.commit_id IS NOT NULL
                 AND dm.store_path IN (SELECT value FROM json_each(?))
               ORDER BY d.created_at, d.rowid"#,
            org,
            paths
        )
        .fetch_all(&self.pool)
        .await?;
        let mut revisions = Vec::<SystemRevision>::new();
        // Oldest first, so the newest deployment of each path wins.
        for row in rows {
            let revision = SystemRevision {
                store_path: row.store_path,
                deployment_id: parse_id(&row.id)?,
                repository_id: row.repository_id.as_deref().map(parse_id).transpose()?,
                rev: row.rev,
                commit_id: row.commit_id,
                created_at: parse_ts(&row.created_at)?,
            };
            revisions.retain(|r| r.store_path != revision.store_path);
            revisions.push(revision);
        }
        Ok(revisions)
    }

    async fn create_user(
        &self,
        organization_id: Uuid,
        username: &str,
        password_hash: &str,
        now: OffsetDateTime,
    ) -> Result<User> {
        let user_id = Uuid::new_v4();
        let (user_id_s, org, now_s) = (id(user_id), id(organization_id), ts(now));
        sqlx::query!(
            "INSERT INTO users (id, organization_id, username, password_hash, created_at,
                                password_changed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            user_id_s,
            org,
            username,
            password_hash,
            now_s,
            now_s
        )
        .execute(&self.pool)
        .await
        .map_err(|e| match Error::from(e) {
            Error::Conflict(_) => Error::Conflict(format!("user {username:?}")),
            e => e,
        })?;
        Ok(User {
            id: user_id,
            organization_id,
            username: username.to_owned(),
            password_hash: password_hash.to_owned(),
            created_at: truncate_to_micros(now),
            password_changed_at: truncate_to_micros(now),
        })
    }

    async fn user_count(&self, organization_id: Uuid) -> Result<u64> {
        let org = id(organization_id);
        let n = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM users WHERE organization_id = ?"#,
            org
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    async fn user_by_name(&self, organization_id: Uuid, username: &str) -> Result<Option<User>> {
        let org = id(organization_id);
        sqlx::query_as!(
            UserRow,
            "SELECT id, organization_id, username, password_hash, created_at, password_changed_at
             FROM users WHERE organization_id = ? AND username = ?",
            org,
            username
        )
        .fetch_optional(&self.pool)
        .await?
        .map(User::try_from)
        .transpose()
    }

    async fn set_user_password(
        &self,
        user_id: Uuid,
        password_hash: &str,
        keep_session: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<()> {
        let (user_id, now) = (id(user_id), ts(now));
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "UPDATE users SET password_hash = ?, password_changed_at = ? WHERE id = ?",
            password_hash,
            now,
            user_id
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "DELETE FROM user_sessions WHERE user_id = ? AND token_hash IS NOT ?",
            user_id,
            keep_session
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn create_user_session(&self, session: &NewUserSession) -> Result<()> {
        let (session_id, user_id) = (id(Uuid::new_v4()), id(session.user_id));
        let (created_at, expires_at) = (ts(session.created_at), ts(session.expires_at));
        sqlx::query!(
            "INSERT INTO user_sessions (id, user_id, token_hash, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?)",
            session_id,
            user_id,
            session.token_hash,
            created_at,
            expires_at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn session_user(&self, token_hash: &str, now: OffsetDateTime) -> Result<Option<User>> {
        let now = ts(now);
        sqlx::query_as!(
            UserRow,
            "SELECT u.id, u.organization_id, u.username, u.password_hash, u.created_at,
                    u.password_changed_at
             FROM user_sessions AS s JOIN users AS u ON u.id = s.user_id
             WHERE s.token_hash = ? AND s.expires_at > ?",
            token_hash,
            now
        )
        .fetch_optional(&self.pool)
        .await?
        .map(User::try_from)
        .transpose()
    }

    async fn delete_user_session(&self, token_hash: &str) -> Result<()> {
        sqlx::query!("DELETE FROM user_sessions WHERE token_hash = ?", token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_expired_user_sessions(&self, now: OffsetDateTime) -> Result<u64> {
        let now = ts(now);
        let result = sqlx::query!("DELETE FROM user_sessions WHERE expires_at <= ?", now)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

struct MachineDeploymentRow {
    deployment_id: String,
    machine_id: String,
    kind: String,
    mode: String,
    rev: Option<String>,
    commit_id: Option<String>,
    requested_by: String,
    status: String,
    store_path: Option<String>,
    message: Option<String>,
    created_at: String,
    finished_at: Option<String>,
}

impl TryFrom<MachineDeploymentRow> for MachineDeploymentRecord {
    type Error = Error;

    fn try_from(row: MachineDeploymentRow) -> Result<Self> {
        Ok(Self {
            deployment_id: parse_id(&row.deployment_id)?,
            machine_id: parse_id(&row.machine_id)?,
            kind: parse_enum(&row.kind, DeploymentKind::parse)?,
            mode: parse_enum(&row.mode, ActivationMode::parse)?,
            rev: row.rev,
            commit_id: row.commit_id,
            requested_by: row.requested_by,
            status: parse_enum(&row.status, MachineDeploymentStatus::parse)?,
            store_path: row.store_path,
            message: row.message,
            created_at: parse_ts(&row.created_at)?,
            finished_at: parse_opt_ts(row.finished_at)?,
        })
    }
}

struct UserRow {
    id: String,
    organization_id: String,
    username: String,
    password_hash: String,
    created_at: String,
    password_changed_at: String,
}

impl TryFrom<UserRow> for User {
    type Error = Error;

    fn try_from(row: UserRow) -> Result<Self> {
        Ok(Self {
            id: parse_id(&row.id)?,
            organization_id: parse_id(&row.organization_id)?,
            username: row.username,
            password_hash: row.password_hash,
            created_at: parse_ts(&row.created_at)?,
            password_changed_at: parse_ts(&row.password_changed_at)?,
        })
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
