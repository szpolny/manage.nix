use std::str::FromStr;

use async_trait::async_trait;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

use crate::{group_columns, Backend, Result, Store, Table, MIGRATIONS_TABLE};

static MIGRATOR: Migrator = sqlx::migrate!("../../migrations/postgres");

/// The backend for teams and replicated deployments.
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
}
