//! The store test suite. Every test runs against both backends.
//!
//! SQLite tests always run. Postgres tests run when
//! `MNIX_TEST_POSTGRES_URL` points at a server where the user may create
//! databases; each test gets a fresh database. Set
//! `MNIX_TEST_REQUIRE_POSTGRES=1` to fail instead of skipping when it is unset
//! (the `store-postgres` flake check does).

use std::str::FromStr;
use std::sync::Arc;

use mnix_store::{Backend, PostgresStore, SqliteStore, Store};
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection};
use tempfile::TempDir;

/// Keeps a test database alive for the duration of a test.
type Fixture = Option<TempDir>;

async fn sqlite_store() -> (Arc<dyn Store>, Fixture) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("db.sqlite").display());
    let store = mnix_store::connect(&url).await.unwrap();
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
    Some((Arc::new(store), None))
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
    assert!(store.schema().await.unwrap().is_empty());
    store.migrate().await.unwrap();
    let names: Vec<_> = store
        .schema()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(names.contains(&"server_meta".to_owned()), "{names:?}");
});

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
    sqlite.migrate().await.unwrap();
    postgres.migrate().await.unwrap();
    assert_eq!(
        sqlite.schema().await.unwrap(),
        postgres.schema().await.unwrap()
    );
}
