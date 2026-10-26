//! The store test suite. Every test runs against both backends.
//!
//! SQLite tests always run. Postgres tests run when
//! `MNIX_TEST_POSTGRES_URL` points at a server where the user may create
//! databases; each test gets a fresh database. Set
//! `MNIX_TEST_REQUIRE_POSTGRES=1` to fail instead of skipping when it is unset
//! (the `store-postgres` flake check does).

use std::str::FromStr;
use std::sync::Arc;

use mnix_protocol::Facts;
use mnix_store::{
    disconnect_reason, Backend, Error, Event, NewEnrollmentToken, NewSession, PostgresStore,
    Redemption, SqliteStore, Store,
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
            "enrollment_tokens",
            "events",
            "machines",
            "organizations",
            "server_meta"
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
