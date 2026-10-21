use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

use crate::{group_columns, Backend, Result, Store, Table, MIGRATIONS_TABLE};

static MIGRATOR: Migrator = sqlx::migrate!("../../migrations/sqlite");

/// How long a connection waits for the write lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The default backend: one file, one server instance.
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
}
