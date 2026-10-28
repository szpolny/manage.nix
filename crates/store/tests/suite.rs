//! The store test suite. Every test runs against both backends.
//!
//! SQLite tests always run. Postgres tests run when
//! `MNIX_TEST_POSTGRES_URL` points at a server where the user may create
//! databases; each test gets a fresh database. Set
//! `MNIX_TEST_REQUIRE_POSTGRES=1` to fail instead of skipping when it is unset
//! (the `store-postgres` flake check does).

use std::str::FromStr;
use std::sync::Arc;

use mnix_protocol::api::{DeploymentKind, DeploymentStatus, MachineDeploymentStatus};
use mnix_protocol::{ActivationMode, ActivationPhase, ActivationReport, ActivationStatus, Facts};
use mnix_store::{
    disconnect_reason, log_source, Backend, Error, Event, MachineOutcome, NewDeployment,
    NewDeploymentMachine, NewEnrollmentToken, NewLogEntry, NewSession, NewUserSession,
    PostgresStore, Redemption, SqliteStore, Store,
};
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection};
use tempfile::TempDir;
use time::macros::datetime;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// Keeps a test database alive for the duration of a test.
type Fixture = Option<TempDir>;

async fn sqlite_store() -> (Arc<dyn Store>, Fixture) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("db.sqlite").display());
    let store = mnix_store::connect(&url).await.unwrap();
    store.migrate().await.unwrap();
    (store, Some(dir))
}

async fn postgres_store() -> Option<(Arc<dyn Store>, Fixture)> {
    let Ok(url) = std::env::var("MNIX_TEST_POSTGRES_URL") else {
        if std::env::var_os("MNIX_TEST_REQUIRE_POSTGRES").is_some() {
            panic!("MNIX_TEST_REQUIRE_POSTGRES is set but MNIX_TEST_POSTGRES_URL is not");
        }
        eprintln!("skipping Postgres: MNIX_TEST_POSTGRES_URL is not set");
        return None;
    };
    let admin = PgConnectOptions::from_str(&url).unwrap();
    let name = format!("mnix_test_{}", uuid::Uuid::new_v4().simple());
    let mut conn = PgConnection::connect_with(&admin).await.unwrap();
    sqlx::query(&format!("CREATE DATABASE \"{name}\""))
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    let store = PostgresStore::connect_with(admin.database(&name))
        .await
        .unwrap();
    store.migrate().await.unwrap();
    Some((Arc::new(store), None))
}

/// An empty database, for the migration tests.
async fn unmigrated_sqlite_store() -> (Arc<dyn Store>, Fixture) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("db.sqlite").display());
    (mnix_store::connect(&url).await.unwrap(), Some(dir))
}

/// Defines `mod <name> { sqlite, postgres }` running `$body(store)` on each.
macro_rules! store_test {
    ($name:ident, $body:expr) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn sqlite() {
                let (store, _fixture) = sqlite_store().await;
                ($body)(store).await;
            }

            #[tokio::test]
            async fn postgres() {
                if let Some((store, _fixture)) = postgres_store().await {
                    ($body)(store).await;
                }
            }
        }
    };
}

store_test!(ping, |store: Arc<dyn Store>| async move {
    store.ping().await.unwrap();
});

store_test!(migrate_is_idempotent, |store: Arc<dyn Store>| async move {
    store.migrate().await.unwrap();
    let first = store.schema().await.unwrap();
    store.migrate().await.unwrap();
    assert_eq!(store.schema().await.unwrap(), first);
});

store_test!(migrate_creates_tables, |store: Arc<dyn Store>| async move {
    let names: Vec<_> = store
        .schema()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(
        names,
        [
            "agent_sessions",
            "deployment_logs",
            "deployment_machines",
            "deployments",
            "enrollment_tokens",
            "events",
            "machine_tags",
            "machines",
            "organizations",
            "repositories",
            "server_meta",
            "user_sessions",
            "users"
        ]
    );
});

#[tokio::test]
async fn migrations_start_from_an_empty_database() {
    let (store, _fixture) = unmigrated_sqlite_store().await;
    assert!(store.schema().await.unwrap().is_empty());
    store.migrate().await.unwrap();
    assert!(!store.schema().await.unwrap().is_empty());
}

const T0: OffsetDateTime = datetime!(2026-10-22 08:00:00.123456 UTC);

fn facts(hostname: &str) -> Facts {
    Facts {
        hostname: hostname.to_owned(),
        current_system: "/nix/store/00000000000000000000000000000000-nixos-system".to_owned(),
        booted_system: None,
        nixos_version: Some("26.11".to_owned()),
        kernel: "6.12.0".to_owned(),
        reboot_required: false,
        generations: Vec::new(),
    }
}

/// Creates a machine with an enrollment token `token-<name>` valid for an hour.
async fn machine_with_token(store: &dyn Store, name: &str) -> (Uuid, Uuid) {
    let org = store.ensure_organization("default", T0).await.unwrap();
    let machine = store.create_machine(org.id, name, T0).await.unwrap();
    store
        .create_enrollment_token(&NewEnrollmentToken {
            machine_id: machine.id,
            token_hash: format!("token-{name}"),
            created_at: T0,
            expires_at: T0 + Duration::hours(1),
        })
        .await
        .unwrap();
    (org.id, machine.id)
}

async fn enrolled_machine(store: &dyn Store, name: &str) -> (Uuid, Uuid) {
    let (org, machine) = machine_with_token(store, name).await;
    let redemption = store
        .redeem_enrollment_token(&format!("token-{name}"), &format!("cred-{name}"), T0)
        .await
        .unwrap();
    assert_eq!(
        redemption,
        Redemption::Enrolled {
            machine_id: machine
        }
    );
    (org, machine)
}

fn session(machine_id: Uuid, at: OffsetDateTime) -> NewSession {
    NewSession {
        machine_id,
        agent_version: "0.1.0".to_owned(),
        facts: facts("web-1"),
        connected_at: at,
    }
}

store_test!(
    organizations_are_created_once,
    |store: Arc<dyn Store>| async move {
        let first = store.ensure_organization("default", T0).await.unwrap();
        let again = store
            .ensure_organization("default", T0 + Duration::minutes(1))
            .await
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(first.created_at, T0);
        let other = store.ensure_organization("other", T0).await.unwrap();
        assert_ne!(first.id, other.id);
    }
);

store_test!(
    machines_are_created_and_listed,
    |store: Arc<dyn Store>| async move {
        let org = store.ensure_organization("default", T0).await.unwrap();
        let web = store.create_machine(org.id, "web-1", T0).await.unwrap();
        store.create_machine(org.id, "db-1", T0).await.unwrap();
        store.create_machine(org.id, "Zeta", T0).await.unwrap();

        assert_eq!(store.machine(web.id).await.unwrap(), Some(web.clone()));
        assert!(store.machine(Uuid::new_v4()).await.unwrap().is_none());
        assert!(web.enrolled_at.is_none() && !web.connected && web.facts.is_none());

        let names: Vec<_> = store
            .machines(org.id)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, ["Zeta", "db-1", "web-1"]);

        let other = store.ensure_organization("other", T0).await.unwrap();
        assert!(store.machines(other.id).await.unwrap().is_empty());
        store.create_machine(other.id, "web-1", T0).await.unwrap();
    }
);

store_test!(machine_names_are_unique_per_organization, |store: Arc<
    dyn Store,
>| async move {
    let org = store.ensure_organization("default", T0).await.unwrap();
    store.create_machine(org.id, "web-1", T0).await.unwrap();
    let err = store.create_machine(org.id, "web-1", T0).await.unwrap_err();
    assert!(matches!(err, Error::Conflict(_)), "{err:?}");
    assert!(err.to_string().contains("web-1"), "{err}");
});

store_test!(
    enrollment_tokens_work_once,
    |store: Arc<dyn Store>| async move {
        let (_, machine) = machine_with_token(&*store, "web-1").await;
        let enrolled_at = T0 + Duration::minutes(5);
        assert_eq!(
            store
                .redeem_enrollment_token("token-web-1", "cred-1", enrolled_at)
                .await
                .unwrap(),
            Redemption::Enrolled {
                machine_id: machine
            }
        );
        let record = store
            .machine_by_credential("cred-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.id, machine);
        assert_eq!(record.enrolled_at, Some(enrolled_at));

        assert_eq!(
            store
                .redeem_enrollment_token("token-web-1", "cred-2", enrolled_at)
                .await
                .unwrap(),
            Redemption::AlreadyUsed {
                machine_id: machine
            }
        );
        assert!(store
            .machine_by_credential("cred-2")
            .await
            .unwrap()
            .is_none());
        assert!(store
            .machine_by_credential("cred-1")
            .await
            .unwrap()
            .is_some());
    }
);

store_test!(expired_and_unknown_tokens_are_rejected, |store: Arc<
    dyn Store,
>| async move {
    let (_, machine) = machine_with_token(&*store, "web-1").await;
    let expiry = T0 + Duration::hours(1);
    assert_eq!(
        store
            .redeem_enrollment_token("token-web-1", "cred-1", expiry)
            .await
            .unwrap(),
        Redemption::Expired {
            machine_id: machine
        }
    );
    assert_eq!(
        store
            .redeem_enrollment_token("nonsense", "cred-1", T0)
            .await
            .unwrap(),
        Redemption::Unknown
    );
    assert!(store
        .machine_by_credential("cred-1")
        .await
        .unwrap()
        .is_none());
    assert!(store
        .machine(machine)
        .await
        .unwrap()
        .unwrap()
        .enrolled_at
        .is_none());
});

store_test!(a_new_token_replaces_the_credential, |store: Arc<
    dyn Store,
>| async move {
    let (_, machine) = enrolled_machine(&*store, "web-1").await;
    store
        .create_enrollment_token(&NewEnrollmentToken {
            machine_id: machine,
            token_hash: "second".to_owned(),
            created_at: T0,
            expires_at: T0 + Duration::hours(1),
        })
        .await
        .unwrap();
    store
        .redeem_enrollment_token("second", "cred-new", T0)
        .await
        .unwrap();
    assert!(store
        .machine_by_credential("cred-web-1")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .machine_by_credential("cred-new")
            .await
            .unwrap()
            .unwrap()
            .id,
        machine
    );
});

store_test!(sessions_record_facts_and_heartbeats, |store: Arc<
    dyn Store,
>| async move {
    let (_, machine) = enrolled_machine(&*store, "web-1").await;
    let connected_at = T0 + Duration::seconds(1);
    let session_id = store
        .open_session(&session(machine, connected_at))
        .await
        .unwrap();

    let record = store.machine(machine).await.unwrap().unwrap();
    assert!(record.connected);
    assert_eq!(record.facts, Some(facts("web-1")));
    assert_eq!(record.agent_version.as_deref(), Some("0.1.0"));
    assert_eq!(record.last_seen_at, Some(connected_at));

    let beat = T0 + Duration::seconds(10);
    assert!(store.heartbeat(session_id, beat).await.unwrap());
    assert_eq!(
        store.machine(machine).await.unwrap().unwrap().last_seen_at,
        Some(beat)
    );

    let mut updated = facts("web-1");
    updated.reboot_required = true;
    store.update_facts(machine, &updated, beat).await.unwrap();
    let record = store.machine(machine).await.unwrap().unwrap();
    assert_eq!(record.facts, Some(updated));
    assert_eq!(record.facts_updated_at, Some(beat));

    assert!(store
        .close_session(session_id, disconnect_reason::CLOSED, beat)
        .await
        .unwrap());
    assert!(!store
        .close_session(session_id, disconnect_reason::CLOSED, beat)
        .await
        .unwrap());
    assert!(!store.heartbeat(session_id, beat).await.unwrap());
    let record = store.machine(machine).await.unwrap().unwrap();
    assert!(!record.connected);
    assert_eq!(record.last_seen_at, Some(beat));
});

store_test!(a_new_session_replaces_the_old_one, |store: Arc<
    dyn Store,
>| async move {
    let (_, machine) = enrolled_machine(&*store, "web-1").await;
    let old = store.open_session(&session(machine, T0)).await.unwrap();
    let new = store
        .open_session(&session(machine, T0 + Duration::seconds(5)))
        .await
        .unwrap();
    assert!(!store
        .heartbeat(old, T0 + Duration::seconds(6))
        .await
        .unwrap());
    assert!(store
        .heartbeat(new, T0 + Duration::seconds(6))
        .await
        .unwrap());
    assert!(store.machine(machine).await.unwrap().unwrap().connected);
});

store_test!(
    stale_sessions_are_closed,
    |store: Arc<dyn Store>| async move {
        let (_, quiet) = enrolled_machine(&*store, "quiet").await;
        let (_, chatty) = enrolled_machine(&*store, "chatty").await;
        let quiet_session = store.open_session(&session(quiet, T0)).await.unwrap();
        let chatty_session = store.open_session(&session(chatty, T0)).await.unwrap();
        store
            .heartbeat(chatty_session, T0 + Duration::seconds(30))
            .await
            .unwrap();

        let closed = store
            .close_stale_sessions(
                T0 + Duration::seconds(20),
                disconnect_reason::HEARTBEAT_TIMEOUT,
                T0 + Duration::seconds(50),
            )
            .await
            .unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].session_id, quiet_session);
        assert_eq!(closed[0].machine_id, quiet);
        assert!(!store.machine(quiet).await.unwrap().unwrap().connected);
        assert!(store.machine(chatty).await.unwrap().unwrap().connected);

        // Already closed sessions are not reported twice.
        assert!(store
            .close_stale_sessions(
                T0 + Duration::seconds(20),
                disconnect_reason::HEARTBEAT_TIMEOUT,
                T0 + Duration::seconds(60),
            )
            .await
            .unwrap()
            .is_empty());
    }
);

store_test!(
    events_are_listed_newest_first,
    |store: Arc<dyn Store>| async move {
        let (org, machine) = machine_with_token(&*store, "web-1").await;
        for (i, kind) in ["machine_created", "machine_enrolled", "agent_connected"]
            .into_iter()
            .enumerate()
        {
            store
                .record_event(&Event {
                    id: Uuid::new_v4(),
                    organization_id: org,
                    machine_id: Some(machine),
                    kind: kind.to_owned(),
                    data: serde_json::json!({ "n": i }),
                    created_at: T0 + Duration::seconds(i as i64),
                })
                .await
                .unwrap();
        }
        let events = store.machine_events(machine, 2).await.unwrap();
        let kinds: Vec<_> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["agent_connected", "machine_enrolled"]);
        assert_eq!(events[0].data, serde_json::json!({ "n": 2 }));
        assert_eq!(events[0].created_at, T0 + Duration::seconds(2));
        assert_eq!(events[0].organization_id, org);
    }
);

#[tokio::test]
async fn connect_picks_backend_from_url() {
    let (store, _fixture) = sqlite_store().await;
    assert_eq!(store.backend(), Backend::Sqlite);
    if let Some((store, _fixture)) = postgres_store().await {
        assert_eq!(store.backend(), Backend::Postgres);
    }
}

#[tokio::test]
async fn connect_rejects_unknown_scheme() {
    assert!(matches!(
        mnix_store::connect("mysql://localhost/mnix").await,
        Err(mnix_store::Error::UnsupportedUrl(_))
    ));
}

#[tokio::test]
async fn sqlite_uses_required_pragmas() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("db.sqlite").display());
    let store = SqliteStore::connect(&url).await.unwrap();
    let pool = store.pool();

    let (journal_mode,): (String,) = sqlx::query_as("PRAGMA journal_mode")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(journal_mode, "wal");
    let (foreign_keys,): (i64,) = sqlx::query_as("PRAGMA foreign_keys")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(foreign_keys, 1);
    let (busy_timeout,): (i64,) = sqlx::query_as("PRAGMA busy_timeout")
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(busy_timeout > 0);
}

#[tokio::test]
async fn backends_produce_the_same_logical_schema() {
    let Some((postgres, _pg)) = postgres_store().await else {
        return;
    };
    let (sqlite, _sqlite) = sqlite_store().await;
    assert_eq!(
        sqlite.schema().await.unwrap(),
        postgres.schema().await.unwrap()
    );
}

store_test!(
    repositories_and_configurations,
    |store: Arc<dyn Store>| async move {
        let org = store.ensure_organization("default", T0).await.unwrap();
        let infra = store
            .create_repository(org.id, "infra", "https://git.example/infra", T0)
            .await
            .unwrap();
        store
            .create_repository(org.id, "homelab", "/srv/git/homelab", T0)
            .await
            .unwrap();
        let err = store
            .create_repository(org.id, "infra", "x", T0)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        let names: Vec<_> = store
            .repositories(org.id)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["homelab", "infra"]);
        assert_eq!(
            store.repository(infra.id).await.unwrap(),
            Some(infra.clone())
        );

        let machine = store.create_machine(org.id, "web-1", T0).await.unwrap();
        assert_eq!(machine.repository_id, None);
        store
            .set_machine_configuration(machine.id, infra.id, "web")
            .await
            .unwrap();
        store
            .set_machine_tags(machine.id, &["web".into(), "eu".into(), "web".into()])
            .await
            .unwrap();
        let record = store.machine(machine.id).await.unwrap().unwrap();
        assert_eq!(record.repository_id, Some(infra.id));
        assert_eq!(record.configuration.as_deref(), Some("web"));
        assert_eq!(record.tags, ["eu", "web"]);
        store.set_machine_tags(machine.id, &[]).await.unwrap();
        assert!(store.machines(org.id).await.unwrap()[0].tags.is_empty());
    }
);

/// A queued deployment of `machines` (already created) from `infra`.
async fn queued_deployment(
    store: &dyn Store,
    org: Uuid,
    machines: &[Uuid],
    at: OffsetDateTime,
) -> Uuid {
    let repository = match store.repositories(org).await.unwrap().pop() {
        Some(r) => r,
        None => store
            .create_repository(org, "infra", "/srv/git/infra", T0)
            .await
            .unwrap(),
    };
    let id = Uuid::new_v4();
    store
        .create_deployment(&NewDeployment {
            id,
            organization_id: org,
            kind: DeploymentKind::Deploy,
            status: DeploymentStatus::Queued,
            repository_id: Some(repository.id),
            rev: Some("main".to_owned()),
            mode: ActivationMode::Switch,
            requested_by: "alice".to_owned(),
            created_at: at,
            machines: machines
                .iter()
                .map(|&machine_id| NewDeploymentMachine {
                    machine_id,
                    configuration: Some("conf".to_owned()),
                    status: MachineDeploymentStatus::Queued,
                })
                .collect(),
        })
        .await
        .unwrap();
    id
}

fn report(status: ActivationStatus, store_path: &str) -> ActivationReport {
    ActivationReport {
        store_path: store_path.to_owned(),
        mode: ActivationMode::Switch,
        status,
        system_changed: status == ActivationStatus::Succeeded,
        previous_system: Some("/nix/store/old".to_owned()),
        message: None,
        failed_checks: vec![],
    }
}

store_test!(deployments_go_from_queue_to_result, |store: Arc<
    dyn Store,
>| async move {
    let (org, web) = enrolled_machine(&*store, "web").await;
    let (_, db) = enrolled_machine(&*store, "db").await;
    let first = queued_deployment(&*store, org, &[web, db], T0).await;
    let second = queued_deployment(&*store, org, &[web], T0 + Duration::seconds(1)).await;

    let listed = store.deployments(org, 10).await.unwrap();
    assert_eq!(
        listed.iter().map(|d| d.id).collect::<Vec<_>>(),
        [second, first]
    );
    let d = &listed[1];
    assert_eq!(d.status, DeploymentStatus::Queued);
    assert_eq!(
        (d.kind, d.mode),
        (DeploymentKind::Deploy, ActivationMode::Switch)
    );
    assert_eq!(d.rev.as_deref(), Some("main"));
    assert_eq!(d.requested_by, "alice");
    let names: Vec<_> = d.machines.iter().map(|m| m.machine_name.as_str()).collect();
    assert_eq!(names, ["db", "web"]);
    assert!(d
        .machines
        .iter()
        .all(|m| m.status == MachineDeploymentStatus::Queued));

    // Oldest first, each once.
    let started = T0 + Duration::seconds(5);
    assert_eq!(store.claim_deployment(started).await.unwrap(), Some(first));
    assert_eq!(store.claim_deployment(started).await.unwrap(), Some(second));
    assert_eq!(store.claim_deployment(started).await.unwrap(), None);

    store.set_deployment_commit(first, "abc123").await.unwrap();
    store
        .set_deployment_machine_status(first, web, MachineDeploymentStatus::Building)
        .await
        .unwrap();
    let sigs = vec!["mnix-1:sig".to_owned()];
    store
        .release_activation(first, web, "/nix/store/new-web", &sigs, started)
        .await
        .unwrap();
    store.start_activating(first).await.unwrap();
    let d = store.deployment(first).await.unwrap().unwrap();
    assert_eq!(d.status, DeploymentStatus::Activating);
    assert_eq!(d.commit_id.as_deref(), Some("abc123"));
    assert_eq!(d.started_at, Some(started));
    let m = d.machines.iter().find(|m| m.machine_id == web).unwrap();
    assert_eq!(m.status, MachineDeploymentStatus::Activating);
    assert_eq!(m.store_path.as_deref(), Some("/nix/store/new-web"));
    assert_eq!(m.signatures, sigs);
    assert_eq!(m.released_at, Some(started));

    let pending = store.pending_activations(web).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].deployment_id, first);
    assert_eq!(pending[0].kind, DeploymentKind::Deploy);
    assert_eq!(pending[0].store_path.as_deref(), Some("/nix/store/new-web"));
    assert_eq!(pending[0].signatures, sigs);
    assert!(store.pending_activations(db).await.unwrap().is_empty());

    // A phase acknowledges the request.
    assert!(store
        .record_activation_phase(first, web, ActivationPhase::Accepted, started)
        .await
        .unwrap());
    assert!(store.pending_activations(web).await.unwrap().is_empty());
    assert!(!store
        .record_activation_phase(first, db, ActivationPhase::Accepted, started)
        .await
        .unwrap());

    let done = T0 + Duration::seconds(30);
    let succeeded = MachineOutcome {
        status: MachineDeploymentStatus::Succeeded,
        message: None,
        report: Some(report(ActivationStatus::Succeeded, "/nix/store/new-web")),
    };
    let finished = store
        .finish_deployment_machine(first, web, &succeeded, done)
        .await
        .unwrap();
    assert!(finished.machine && finished.deployment.is_none());
    // Finishing twice changes nothing.
    let again = store
        .finish_deployment_machine(first, web, &succeeded, done)
        .await
        .unwrap();
    assert!(!again.machine);
    assert!(!store
        .record_activation_phase(first, web, ActivationPhase::AwaitingConfirmation, done)
        .await
        .unwrap());

    let failed = MachineOutcome {
        status: MachineDeploymentStatus::Failed,
        message: Some("build failed".to_owned()),
        report: None,
    };
    let finished = store
        .finish_deployment_machine(first, db, &failed, done)
        .await
        .unwrap();
    assert_eq!(finished.deployment, Some(DeploymentStatus::Failed));
    let d = store.deployment(first).await.unwrap().unwrap();
    assert_eq!(d.status, DeploymentStatus::Failed);
    assert_eq!(d.finished_at, Some(done));
    let m = d.machines.iter().find(|m| m.machine_id == web).unwrap();
    assert_eq!(m.report, succeeded.report);
    assert_eq!(m.phase, Some(ActivationPhase::Accepted));
    assert_eq!(m.finished_at, Some(done));
    let m = d.machines.iter().find(|m| m.machine_id == db).unwrap();
    assert_eq!(m.message.as_deref(), Some("build failed"));
    // Finished deployments stay finished.
    store.start_activating(first).await.unwrap();
    assert_eq!(
        store.deployment(first).await.unwrap().unwrap().status,
        DeploymentStatus::Failed
    );

    let finished = store
        .finish_deployment_machine(second, web, &succeeded, done)
        .await
        .unwrap();
    assert_eq!(finished.deployment, Some(DeploymentStatus::Succeeded));
});

store_test!(
    rollbacks_are_released_at_once,
    |store: Arc<dyn Store>| async move {
        let (org, web) = enrolled_machine(&*store, "web").await;
        let id = Uuid::new_v4();
        store
            .create_deployment(&NewDeployment {
                id,
                organization_id: org,
                kind: DeploymentKind::Rollback,
                status: DeploymentStatus::Activating,
                repository_id: None,
                rev: None,
                mode: ActivationMode::Test,
                requested_by: "bob".to_owned(),
                created_at: T0,
                machines: vec![NewDeploymentMachine {
                    machine_id: web,
                    configuration: None,
                    status: MachineDeploymentStatus::Activating,
                }],
            })
            .await
            .unwrap();
        let pending = store.pending_activations(web).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            (
                pending[0].kind,
                pending[0].mode,
                pending[0].store_path.as_deref()
            ),
            (DeploymentKind::Rollback, ActivationMode::Test, None)
        );
        assert!(pending[0].signatures.is_empty());
        // The store path the agent rolled back to is recorded.
        store
            .finish_deployment_machine(
                id,
                web,
                &MachineOutcome {
                    status: MachineDeploymentStatus::Succeeded,
                    message: None,
                    report: Some(report(ActivationStatus::Succeeded, "/nix/store/previous")),
                },
                T0,
            )
            .await
            .unwrap();
        let d = store.deployment(id).await.unwrap().unwrap();
        assert_eq!(d.status, DeploymentStatus::Succeeded);
        assert_eq!(
            d.machines[0].store_path.as_deref(),
            Some("/nix/store/previous")
        );
    }
);

store_test!(stuck_deployments_are_found_and_failed, |store: Arc<
    dyn Store,
>| async move {
    let (org, web) = enrolled_machine(&*store, "web").await;
    let (_, db) = enrolled_machine(&*store, "db").await;
    let building = queued_deployment(&*store, org, &[db], T0).await;
    let activating = queued_deployment(&*store, org, &[web], T0).await;
    store.claim_deployment(T0).await.unwrap();
    store.claim_deployment(T0).await.unwrap();
    store
        .release_activation(activating, web, "/nix/store/x", &[], T0)
        .await
        .unwrap();
    store.start_activating(activating).await.unwrap();

    let later = T0 + Duration::minutes(10);
    assert_eq!(store.stale_builds(later).await.unwrap(), [building]);
    assert!(store.stale_builds(T0).await.unwrap().is_empty());
    assert_eq!(
        store.expired_activations(later).await.unwrap(),
        [(activating, web)]
    );
    assert!(store.expired_activations(T0).await.unwrap().is_empty());

    store
        .fail_deployment(building, "interrupted", later)
        .await
        .unwrap();
    let d = store.deployment(building).await.unwrap().unwrap();
    assert_eq!(d.status, DeploymentStatus::Failed);
    assert_eq!(d.message.as_deref(), Some("interrupted"));
    assert_eq!(d.machines[0].status, MachineDeploymentStatus::Failed);
    assert_eq!(d.machines[0].message.as_deref(), Some("interrupted"));
    assert!(store.stale_builds(later).await.unwrap().is_empty());
});

store_test!(
    logs_keep_order_and_drop_repeated_agent_output,
    |store: Arc<dyn Store>| async move {
        let (org, web) = enrolled_machine(&*store, "web").await;
        let deployment = queued_deployment(&*store, org, &[web], T0).await;
        store
            .append_log(&NewLogEntry {
                deployment_id: deployment,
                machine_id: None,
                source: log_source::SERVER.to_owned(),
                text: "fetching\n".to_owned(),
                created_at: T0,
            })
            .await
            .unwrap();
        for (offset, text) in [
            (0, "one\n"),
            (0, "one\n"),
            (0, "one\ntwo\n"),
            (8, "three\n"),
        ] {
            store
                .append_agent_log(deployment, web, offset, text, T0)
                .await
                .unwrap();
        }
        // Unknown deployments are ignored.
        store
            .append_agent_log(Uuid::new_v4(), web, 0, "x", T0)
            .await
            .unwrap();

        let logs = store.deployment_logs(deployment, 0, 100).await.unwrap();
        let texts: Vec<_> = logs.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["fetching\n", "one\n", "two\n", "three\n"]);
        assert_eq!(logs[0].source, "server");
        assert_eq!(logs[0].machine_name, None);
        assert_eq!(logs[1].source, "agent");
        assert_eq!(logs[1].machine_name.as_deref(), Some("web"));
        assert_eq!(logs[1].machine_id, Some(web));
        assert!(logs.windows(2).all(|w| w[0].id < w[1].id));

        let rest = store
            .deployment_logs(deployment, logs[1].id, 1)
            .await
            .unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].text, "two\n");
    }
);

store_test!(machine_history_and_system_revisions, |store: Arc<
    dyn Store,
>| async move {
    let (org, web) = enrolled_machine(&*store, "web").await;
    let (_, db) = enrolled_machine(&*store, "db").await;
    let (_, idle) = enrolled_machine(&*store, "idle").await;
    assert!(store
        .latest_machine_deployments(org)
        .await
        .unwrap()
        .is_empty());

    let first = queued_deployment(&*store, org, &[web, db], T0).await;
    let second = queued_deployment(&*store, org, &[web], T0 + Duration::seconds(1)).await;
    for (deployment, at) in [(first, T0), (second, T0 + Duration::seconds(1))] {
        assert_eq!(store.claim_deployment(at).await.unwrap(), Some(deployment));
    }
    store
        .set_deployment_commit(first, "commit-1")
        .await
        .unwrap();
    store
        .set_deployment_commit(second, "commit-2")
        .await
        .unwrap();
    // Both deployments built the same system for web (the commits differ
    // only for db); the newer one counts.
    for (deployment, machine, path) in [
        (first, web, "/nix/store/web-1"),
        (first, db, "/nix/store/db-1"),
        (second, web, "/nix/store/web-1"),
    ] {
        store
            .release_activation(deployment, machine, path, &[], T0)
            .await
            .unwrap();
    }
    store
        .finish_deployment_machine(
            first,
            db,
            &MachineOutcome {
                status: MachineDeploymentStatus::Failed,
                message: Some("broken".to_owned()),
                report: None,
            },
            T0 + Duration::seconds(2),
        )
        .await
        .unwrap();

    let history = store.machine_deployments(web, 10).await.unwrap();
    assert_eq!(
        history.iter().map(|h| h.deployment_id).collect::<Vec<_>>(),
        [second, first]
    );
    assert_eq!(history[0].machine_id, web);
    assert_eq!(history[0].kind, DeploymentKind::Deploy);
    assert_eq!(history[0].commit_id.as_deref(), Some("commit-2"));
    assert_eq!(history[0].status, MachineDeploymentStatus::Activating);
    assert_eq!(history[0].requested_by, "alice");
    assert_eq!(store.machine_deployments(web, 1).await.unwrap().len(), 1);

    let db_history = store.machine_deployments(db, 10).await.unwrap();
    assert_eq!(db_history.len(), 1);
    assert_eq!(db_history[0].status, MachineDeploymentStatus::Failed);
    assert_eq!(db_history[0].message.as_deref(), Some("broken"));
    assert_eq!(db_history[0].finished_at, Some(T0 + Duration::seconds(2)));
    assert!(store
        .machine_deployments(idle, 10)
        .await
        .unwrap()
        .is_empty());

    let mut latest = store.latest_machine_deployments(org).await.unwrap();
    latest.sort_by_key(|r| r.deployment_id == second);
    assert_eq!(
        latest
            .iter()
            .map(|r| (r.machine_id, r.deployment_id))
            .collect::<Vec<_>>(),
        [(db, first), (web, second)]
    );

    let mut revisions = store
        .system_revisions(
            org,
            &[
                "/nix/store/web-1".to_owned(),
                "/nix/store/db-1".to_owned(),
                "/nix/store/unknown".to_owned(),
            ],
        )
        .await
        .unwrap();
    revisions.sort_by(|a, b| a.store_path.cmp(&b.store_path));
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0].store_path, "/nix/store/db-1");
    assert_eq!(revisions[0].commit_id, "commit-1");
    assert_eq!(revisions[1].store_path, "/nix/store/web-1");
    assert_eq!(revisions[1].deployment_id, second);
    assert_eq!(revisions[1].commit_id, "commit-2");
    assert_eq!(revisions[1].rev.as_deref(), Some("main"));
    assert!(revisions[1].repository_id.is_some());
    assert!(store.system_revisions(org, &[]).await.unwrap().is_empty());
});

store_test!(users_are_unique_per_organization, |store: Arc<
    dyn Store,
>| async move {
    let org = store.ensure_organization("default", T0).await.unwrap();
    let other = store.ensure_organization("other", T0).await.unwrap();
    assert_eq!(store.user_count(org.id).await.unwrap(), 0);
    let alice = store
        .create_user(org.id, "alice", "hash-1", T0)
        .await
        .unwrap();
    assert_eq!(alice.created_at, T0);
    assert_eq!(alice.password_changed_at, T0);
    assert!(matches!(
        store.create_user(org.id, "alice", "hash-2", T0).await,
        Err(Error::Conflict(_))
    ));
    store
        .create_user(other.id, "alice", "hash-3", T0)
        .await
        .unwrap();
    assert_eq!(store.user_count(org.id).await.unwrap(), 1);
    assert_eq!(
        store.user_by_name(org.id, "alice").await.unwrap(),
        Some(alice)
    );
    assert_eq!(store.user_by_name(org.id, "bob").await.unwrap(), None);
});

store_test!(
    user_sessions_expire_and_end_with_a_new_password,
    |store: Arc<dyn Store>| async move {
        let org = store.ensure_organization("default", T0).await.unwrap();
        let alice = store
            .create_user(org.id, "alice", "hash-1", T0)
            .await
            .unwrap();
        for (token, hours) in [("s1", 1), ("s2", 1), ("s3", 2)] {
            store
                .create_user_session(&NewUserSession {
                    user_id: alice.id,
                    token_hash: token.to_owned(),
                    created_at: T0,
                    expires_at: T0 + Duration::hours(hours),
                })
                .await
                .unwrap();
        }
        let user = store.session_user("s1", T0).await.unwrap().unwrap();
        assert_eq!(user.username, "alice");
        assert_eq!(store.session_user("nope", T0).await.unwrap(), None);
        // Expired at exactly `expires_at`.
        assert_eq!(
            store
                .session_user("s1", T0 + Duration::hours(1))
                .await
                .unwrap(),
            None
        );

        store.delete_user_session("s2").await.unwrap();
        assert_eq!(store.session_user("s2", T0).await.unwrap(), None);

        // Changing the password keeps only the session that changed it.
        let later = T0 + Duration::minutes(5);
        store
            .set_user_password(alice.id, "hash-2", Some("s3"), later)
            .await
            .unwrap();
        assert_eq!(store.session_user("s1", T0).await.unwrap(), None);
        let user = store.session_user("s3", T0).await.unwrap().unwrap();
        assert_eq!(user.password_hash, "hash-2");
        assert_eq!(user.password_changed_at, later);
        store
            .set_user_password(alice.id, "hash-3", None, later)
            .await
            .unwrap();
        assert_eq!(store.session_user("s3", T0).await.unwrap(), None);

        store
            .create_user_session(&NewUserSession {
                user_id: alice.id,
                token_hash: "s4".to_owned(),
                created_at: T0,
                expires_at: T0 + Duration::hours(1),
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .delete_expired_user_sessions(T0 + Duration::minutes(59))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .delete_expired_user_sessions(T0 + Duration::hours(1))
                .await
                .unwrap(),
            1
        );
    }
);
