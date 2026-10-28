//! The Postgres backend, for teams and replicated deployments.

use std::str::FromStr;

use async_trait::async_trait;
use mnix_protocol::api::{DeploymentKind, DeploymentStatus, MachineDeploymentStatus};
use mnix_protocol::{ActivationMode, ActivationPhase, ActivationReport, Facts};
use mnix_store_core::{
    disconnect_reason, group_columns, log_source, new_log_text, parse_enum, truncate_to_micros,
    Backend, ClosedSession, Deployment, DeploymentMachine, Error, Event, Finished, LogEntry,
    Machine, MachineDeploymentRecord, MachineOutcome, NewDeployment, NewEnrollmentToken,
    NewLogEntry, NewSession, NewUserSession, Organization, PendingActivation, Redemption, Release,
    Repository, Result, Store, SystemRevision, Table, User, MIGRATIONS_TABLE,
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
    repository_id: Option<Uuid>,
    configuration: Option<String>,
    tags: Vec<String>,
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
            repository_id: row.repository_id,
            configuration: row.configuration,
            tags: row.tags,
        }
    }
}

struct DeploymentRow {
    id: Uuid,
    organization_id: Uuid,
    kind: String,
    status: String,
    message: Option<String>,
    repository_id: Option<Uuid>,
    rev: Option<String>,
    commit_id: Option<String>,
    mode: String,
    requested_by: String,
    created_at: OffsetDateTime,
    started_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
}

struct DeploymentMachineRow {
    machine_id: Uuid,
    machine_name: String,
    configuration: Option<String>,
    status: String,
    phase: Option<String>,
    store_path: Option<String>,
    signatures: Vec<String>,
    message: Option<String>,
    report: Option<Json<ActivationReport>>,
    released_at: Option<OffsetDateTime>,
    accepted_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
}

impl TryFrom<DeploymentMachineRow> for DeploymentMachine {
    type Error = Error;

    fn try_from(row: DeploymentMachineRow) -> Result<Self> {
        Ok(Self {
            machine_id: row.machine_id,
            machine_name: row.machine_name,
            configuration: row.configuration,
            status: parse_enum(&row.status, MachineDeploymentStatus::parse)?,
            phase: row
                .phase
                .as_deref()
                .map(|p| parse_enum(p, ActivationPhase::parse))
                .transpose()?,
            store_path: row.store_path,
            signatures: row.signatures,
            message: row.message,
            report: row.report.map(|Json(report)| report),
            released_at: row.released_at,
            accepted_at: row.accepted_at,
            finished_at: row.finished_at,
        })
    }
}

impl PostgresStore {
    async fn load_deployment(&self, row: DeploymentRow) -> Result<Deployment> {
        let machines = sqlx::query_as!(
            DeploymentMachineRow,
            r#"SELECT dm.machine_id, m.name AS machine_name, dm.configuration, dm.status,
                      dm.phase, dm.store_path, dm.signatures,
                      dm.message, dm.report AS "report: Json<ActivationReport>",
                      dm.released_at, dm.accepted_at, dm.finished_at
               FROM deployment_machines AS dm JOIN machines AS m ON m.id = dm.machine_id
               WHERE dm.deployment_id = $1
               ORDER BY m.name COLLATE "C""#,
            row.id
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(DeploymentMachine::try_from)
        .collect::<Result<_>>()?;
        Ok(Deployment {
            id: row.id,
            organization_id: row.organization_id,
            kind: parse_enum(&row.kind, DeploymentKind::parse)?,
            status: parse_enum(&row.status, DeploymentStatus::parse)?,
            message: row.message,
            repository_id: row.repository_id,
            rev: row.rev,
            commit_id: row.commit_id,
            mode: parse_enum(&row.mode, ActivationMode::parse)?,
            requested_by: row.requested_by,
            created_at: row.created_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
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
                      facts AS "facts: Json<Facts>", facts_updated_at, last_seen_at,
                      EXISTS (
                          SELECT 1 FROM agent_sessions AS s
                          WHERE s.machine_id = machines.id AND s.disconnected_at IS NULL
                      ) AS "connected!",
                      repository_id, configuration,
                      ARRAY(
                          SELECT tag FROM machine_tags AS t
                          WHERE t.machine_id = machines.id ORDER BY tag COLLATE "C"
                      ) AS "tags!"
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
            repository_id: None,
            configuration: None,
            tags: Vec::new(),
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
    async fn create_repository(
        &self,
        organization_id: Uuid,
        name: &str,
        url: &str,
        now: OffsetDateTime,
    ) -> Result<Repository> {
        let repository = Repository {
            id: Uuid::new_v4(),
            organization_id,
            name: name.to_owned(),
            url: url.to_owned(),
            created_at: truncate_to_micros(now),
        };
        sqlx::query!(
            "INSERT INTO repositories (id, organization_id, name, url, created_at)
             VALUES ($1, $2, $3, $4, $5)",
            repository.id,
            organization_id,
            name,
            url,
            repository.created_at
        )
        .execute(&self.pool)
        .await
        .map_err(|e| match Error::from(e) {
            Error::Conflict(_) => Error::Conflict(format!("repository {name:?}")),
            e => e,
        })?;
        Ok(repository)
    }

    async fn set_repository_url(&self, repository_id: Uuid, url: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE repositories SET url = $1 WHERE id = $2",
            url,
            repository_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn repositories(&self, organization_id: Uuid) -> Result<Vec<Repository>> {
        Ok(sqlx::query_as!(
            Repository,
            r#"SELECT id, organization_id, name, url, created_at FROM repositories
               WHERE organization_id = $1 ORDER BY name COLLATE "C""#,
            organization_id
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn repository(&self, repository_id: Uuid) -> Result<Option<Repository>> {
        Ok(sqlx::query_as!(
            Repository,
            "SELECT id, organization_id, name, url, created_at FROM repositories WHERE id = $1",
            repository_id
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn set_machine_configuration(
        &self,
        machine_id: Uuid,
        repository_id: Uuid,
        configuration: &str,
    ) -> Result<()> {
        sqlx::query!(
            "UPDATE machines SET repository_id = $1, configuration = $2 WHERE id = $3",
            repository_id,
            configuration,
            machine_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_machine_tags(&self, machine_id: Uuid, tags: &[String]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query!("DELETE FROM machine_tags WHERE machine_id = $1", machine_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query!(
            "INSERT INTO machine_tags (machine_id, tag)
             SELECT $1, tag FROM UNNEST($2::text[]) AS tag
             ON CONFLICT DO NOTHING",
            machine_id,
            tags
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn create_deployment(&self, d: &NewDeployment) -> Result<()> {
        let now = truncate_to_micros(d.created_at);
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "INSERT INTO deployments (id, organization_id, kind, status, repository_id, rev,
                                      mode, requested_by, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            d.id,
            d.organization_id,
            d.kind.as_str(),
            d.status.as_str(),
            d.repository_id,
            d.rev,
            d.mode.as_str(),
            d.requested_by,
            now
        )
        .execute(&mut *tx)
        .await?;
        for m in &d.machines {
            let released_at = (m.status == MachineDeploymentStatus::Activating).then_some(now);
            sqlx::query!(
                "INSERT INTO deployment_machines
                     (deployment_id, machine_id, configuration, status, signatures, released_at)
                 VALUES ($1, $2, $3, $4, '{}', $5)",
                d.id,
                m.machine_id,
                m.configuration,
                m.status.as_str(),
                released_at
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn deployment(&self, deployment_id: Uuid) -> Result<Option<Deployment>> {
        let row = sqlx::query_as!(
            DeploymentRow,
            "SELECT id, organization_id, kind, status, message, repository_id, rev, commit_id,
                    mode, requested_by, created_at, started_at, finished_at
             FROM deployments WHERE id = $1",
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
        let rows = sqlx::query_as!(
            DeploymentRow,
            "SELECT id, organization_id, kind, status, message, repository_id, rev, commit_id,
                    mode, requested_by, created_at, started_at, finished_at
             FROM deployments WHERE organization_id = $1
             ORDER BY created_at DESC, id DESC LIMIT $2",
            organization_id,
            i64::from(limit)
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
        // SKIP LOCKED: concurrent servers each claim a different deployment.
        Ok(sqlx::query_scalar!(
            "UPDATE deployments SET status = 'building', started_at = $1
             WHERE id = (
                 SELECT id FROM deployments WHERE status = 'queued'
                 ORDER BY created_at LIMIT 1
                 FOR UPDATE SKIP LOCKED
             )
             RETURNING id",
            truncate_to_micros(now)
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn set_deployment_commit(&self, deployment_id: Uuid, commit_id: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE deployments SET commit_id = $1 WHERE id = $2",
            commit_id,
            deployment_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn start_activating(&self, deployment_id: Uuid) -> Result<()> {
        sqlx::query!(
            "UPDATE deployments SET status = 'activating'
             WHERE id = $1 AND finished_at IS NULL",
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
        let now = truncate_to_micros(now);
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "SELECT id FROM deployments WHERE id = $1 FOR UPDATE",
            deployment_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE deployment_machines SET status = 'failed', message = $1, finished_at = $2
             WHERE deployment_id = $3 AND finished_at IS NULL",
            message,
            now,
            deployment_id
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE deployments SET status = 'failed', message = $1, finished_at = $2
             WHERE id = $3 AND finished_at IS NULL",
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
        sqlx::query!(
            "UPDATE deployment_machines SET status = $1
             WHERE deployment_id = $2 AND machine_id = $3 AND finished_at IS NULL",
            status.as_str(),
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
        release: &Release,
        now: OffsetDateTime,
    ) -> Result<()> {
        let (store_path, signatures, attribute) = match release {
            Release::System {
                store_path,
                signatures,
            } => (Some(store_path.as_str()), signatures.as_slice(), None),
            Release::Build { attribute } => (None, &[][..], Some(attribute.as_str())),
        };
        sqlx::query!(
            "UPDATE deployment_machines
             SET status = 'activating', store_path = $1, signatures = $2, attribute = $3,
                 released_at = $4
             WHERE deployment_id = $5 AND machine_id = $6 AND finished_at IS NULL",
            store_path,
            signatures,
            attribute,
            truncate_to_micros(now),
            deployment_id,
            machine_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn pending_activations(&self, machine_id: Uuid) -> Result<Vec<PendingActivation>> {
        sqlx::query!(
            "SELECT dm.deployment_id, d.kind, d.mode, dm.store_path, dm.signatures, dm.attribute,
                    d.commit_id
             FROM deployment_machines AS dm JOIN deployments AS d ON d.id = dm.deployment_id
             WHERE dm.machine_id = $1 AND dm.status = 'activating' AND dm.accepted_at IS NULL
             ORDER BY dm.released_at",
            machine_id
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(PendingActivation {
                deployment_id: row.deployment_id,
                kind: parse_enum(&row.kind, DeploymentKind::parse)?,
                mode: parse_enum(&row.mode, ActivationMode::parse)?,
                store_path: row.store_path,
                signatures: row.signatures,
                attribute: row.attribute,
                commit_id: row.commit_id,
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
        let result = sqlx::query!(
            "UPDATE deployment_machines SET phase = $1, accepted_at = COALESCE(accepted_at, $2)
             WHERE deployment_id = $3 AND machine_id = $4 AND status = 'activating'",
            phase.as_str(),
            truncate_to_micros(now),
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
        let now = truncate_to_micros(now);
        let store_path = outcome
            .report
            .as_ref()
            .map(|r| r.store_path.as_str())
            .filter(|p| !p.is_empty());
        let mut tx = self.pool.begin().await?;
        // Machines finishing at the same time (on different replicas) take
        // turns, so exactly one of them sees the deployment complete.
        sqlx::query!(
            "SELECT id FROM deployments WHERE id = $1 FOR UPDATE",
            deployment_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        let changed = sqlx::query!(
            "UPDATE deployment_machines
             SET status = $1, message = $2, report = $3, store_path = COALESCE(store_path, $4),
                 finished_at = $5
             WHERE deployment_id = $6 AND machine_id = $7 AND finished_at IS NULL",
            outcome.status.as_str(),
            outcome.message,
            outcome.report.as_ref().map(Json) as _,
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
            r#"SELECT COUNT(*) FILTER (WHERE finished_at IS NULL) AS "open!",
                      COUNT(*) FILTER (WHERE status <> 'succeeded') AS "bad!"
               FROM deployment_machines WHERE deployment_id = $1"#,
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
            let result = sqlx::query!(
                "UPDATE deployments SET status = $1, finished_at = $2
                 WHERE id = $3 AND finished_at IS NULL",
                final_status.as_str(),
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
        Ok(sqlx::query!(
            "SELECT deployment_id, machine_id FROM deployment_machines
             WHERE status = 'activating' AND released_at < $1",
            truncate_to_micros(cutoff)
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| (row.deployment_id, row.machine_id))
        .collect())
    }

    async fn stale_builds(&self, cutoff: OffsetDateTime) -> Result<Vec<Uuid>> {
        Ok(sqlx::query_scalar!(
            "SELECT id FROM deployments WHERE status = 'building' AND started_at < $1",
            truncate_to_micros(cutoff)
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn append_log(&self, entry: &NewLogEntry) -> Result<()> {
        sqlx::query!(
            "INSERT INTO deployment_logs (deployment_id, machine_id, source, text, created_at)
             VALUES ($1, $2, $3, $4, $5)",
            entry.deployment_id,
            entry.machine_id,
            entry.source,
            entry.text,
            truncate_to_micros(entry.created_at)
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
        let mut tx = self.pool.begin().await?;
        let received = sqlx::query_scalar!(
            "SELECT log_bytes FROM deployment_machines
             WHERE deployment_id = $1 AND machine_id = $2
             FOR UPDATE",
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
        sqlx::query!(
            "UPDATE deployment_machines SET log_bytes = $1
             WHERE deployment_id = $2 AND machine_id = $3",
            end as i64,
            deployment_id,
            machine_id
        )
        .execute(&mut *tx)
        .await?;
        if !new.is_empty() {
            sqlx::query!(
                "INSERT INTO deployment_logs (deployment_id, machine_id, source, text, created_at)
                 VALUES ($1, $2, $3, $4, $5)",
                deployment_id,
                machine_id,
                log_source::AGENT,
                new,
                truncate_to_micros(now)
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
        Ok(sqlx::query_as!(
            LogEntry,
            r#"SELECT l.id, l.machine_id, m.name AS "machine_name?", l.source, l.text,
                      l.created_at
               FROM deployment_logs AS l LEFT JOIN machines AS m ON m.id = l.machine_id
               WHERE l.deployment_id = $1 AND l.id > $2
               ORDER BY l.id LIMIT $3"#,
            deployment_id,
            after,
            i64::from(limit)
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn machine_deployments(
        &self,
        machine_id: Uuid,
        limit: u32,
    ) -> Result<Vec<MachineDeploymentRecord>> {
        sqlx::query_as!(
            MachineDeploymentRow,
            "SELECT dm.deployment_id, dm.machine_id, d.kind, d.mode, d.rev, d.commit_id,
                    d.requested_by, dm.status, dm.store_path, dm.message, d.created_at,
                    dm.finished_at
             FROM deployment_machines AS dm JOIN deployments AS d ON d.id = dm.deployment_id
             WHERE dm.machine_id = $1
             ORDER BY d.created_at DESC, d.id DESC LIMIT $2",
            machine_id,
            i64::from(limit)
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
        sqlx::query_as!(
            MachineDeploymentRow,
            "SELECT dm.deployment_id, dm.machine_id, d.kind, d.mode, d.rev, d.commit_id,
                    d.requested_by, dm.status, dm.store_path, dm.message, d.created_at,
                    dm.finished_at
             FROM deployment_machines AS dm JOIN deployments AS d ON d.id = dm.deployment_id
             WHERE d.organization_id = $1 AND d.id = (
                 SELECT d2.id
                 FROM deployment_machines AS dm2
                 JOIN deployments AS d2 ON d2.id = dm2.deployment_id
                 WHERE dm2.machine_id = dm.machine_id
                 ORDER BY d2.created_at DESC, d2.id DESC LIMIT 1
             )",
            organization_id
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
        Ok(sqlx::query_as!(
            SystemRevision,
            r#"SELECT DISTINCT ON (dm.store_path)
                      dm.store_path AS "store_path!", d.id AS deployment_id, d.repository_id,
                      d.rev, d.commit_id AS "commit_id!", d.created_at
               FROM deployment_machines AS dm JOIN deployments AS d ON d.id = dm.deployment_id
               WHERE d.organization_id = $1 AND d.kind = 'deploy' AND d.commit_id IS NOT NULL
                 AND dm.store_path = ANY($2)
               ORDER BY dm.store_path, d.created_at DESC, d.id DESC"#,
            organization_id,
            store_paths
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn create_user(
        &self,
        organization_id: Uuid,
        username: &str,
        password_hash: &str,
        now: OffsetDateTime,
    ) -> Result<User> {
        let now = truncate_to_micros(now);
        let user = User {
            id: Uuid::new_v4(),
            organization_id,
            username: username.to_owned(),
            password_hash: password_hash.to_owned(),
            created_at: now,
            password_changed_at: now,
        };
        sqlx::query!(
            "INSERT INTO users (id, organization_id, username, password_hash, created_at,
                                password_changed_at)
             VALUES ($1, $2, $3, $4, $5, $5)",
            user.id,
            organization_id,
            username,
            password_hash,
            now
        )
        .execute(&self.pool)
        .await
        .map_err(|e| match Error::from(e) {
            Error::Conflict(_) => Error::Conflict(format!("user {username:?}")),
            e => e,
        })?;
        Ok(user)
    }

    async fn user_count(&self, organization_id: Uuid) -> Result<u64> {
        let n = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!" FROM users WHERE organization_id = $1"#,
            organization_id
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    async fn user_by_name(&self, organization_id: Uuid, username: &str) -> Result<Option<User>> {
        Ok(sqlx::query_as!(
            User,
            "SELECT id, organization_id, username, password_hash, created_at, password_changed_at
             FROM users WHERE organization_id = $1 AND username = $2",
            organization_id,
            username
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn set_user_password(
        &self,
        user_id: Uuid,
        password_hash: &str,
        keep_session: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "UPDATE users SET password_hash = $1, password_changed_at = $2 WHERE id = $3",
            password_hash,
            truncate_to_micros(now),
            user_id
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "DELETE FROM user_sessions WHERE user_id = $1 AND token_hash IS DISTINCT FROM $2",
            user_id,
            keep_session
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn create_user_session(&self, session: &NewUserSession) -> Result<()> {
        sqlx::query!(
            "INSERT INTO user_sessions (id, user_id, token_hash, created_at, expires_at)
             VALUES ($1, $2, $3, $4, $5)",
            Uuid::new_v4(),
            session.user_id,
            session.token_hash,
            truncate_to_micros(session.created_at),
            truncate_to_micros(session.expires_at)
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn session_user(&self, token_hash: &str, now: OffsetDateTime) -> Result<Option<User>> {
        Ok(sqlx::query_as!(
            User,
            "SELECT u.id, u.organization_id, u.username, u.password_hash, u.created_at,
                    u.password_changed_at
             FROM user_sessions AS s JOIN users AS u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.expires_at > $2",
            token_hash,
            now
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn delete_user_session(&self, token_hash: &str) -> Result<()> {
        sqlx::query!(
            "DELETE FROM user_sessions WHERE token_hash = $1",
            token_hash
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_expired_user_sessions(&self, now: OffsetDateTime) -> Result<u64> {
        let result = sqlx::query!("DELETE FROM user_sessions WHERE expires_at <= $1", now)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

struct MachineDeploymentRow {
    deployment_id: Uuid,
    machine_id: Uuid,
    kind: String,
    mode: String,
    rev: Option<String>,
    commit_id: Option<String>,
    requested_by: String,
    status: String,
    store_path: Option<String>,
    message: Option<String>,
    created_at: OffsetDateTime,
    finished_at: Option<OffsetDateTime>,
}

impl TryFrom<MachineDeploymentRow> for MachineDeploymentRecord {
    type Error = Error;

    fn try_from(row: MachineDeploymentRow) -> Result<Self> {
        Ok(Self {
            deployment_id: row.deployment_id,
            machine_id: row.machine_id,
            kind: parse_enum(&row.kind, DeploymentKind::parse)?,
            mode: parse_enum(&row.mode, ActivationMode::parse)?,
            rev: row.rev,
            commit_id: row.commit_id,
            requested_by: row.requested_by,
            status: parse_enum(&row.status, MachineDeploymentStatus::parse)?,
            store_path: row.store_path,
            message: row.message,
            created_at: row.created_at,
            finished_at: row.finished_at,
        })
    }
}
