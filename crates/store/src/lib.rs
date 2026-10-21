//! Storage for `mnix-server`.
//!
//! The server talks to its database only through the [`Store`] trait. There
//! are two implementations, [`SqliteStore`] (the default, single instance) and
//! [`PostgresStore`] (teams, replicas). No SQL lives outside this crate.
//!
//! Each backend has its own migration directory (`migrations/sqlite`,
//! `migrations/postgres`) with the same numbering; the test suite checks that
//! both produce the same logical schema.

mod postgres;
mod sqlite;

use std::sync::Arc;

use async_trait::async_trait;

pub use postgres::PostgresStore;
pub use sqlite::SqliteStore;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported database URL {0:?}: expected sqlite:// or postgres://")]
    UnsupportedUrl(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
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
}

/// Opens the store for `url` (`sqlite://...` or `postgres://...`).
pub async fn connect(url: &str) -> Result<Arc<dyn Store>> {
    Ok(match Backend::from_url(url)? {
        Backend::Sqlite => Arc::new(SqliteStore::connect(url).await?),
        Backend::Postgres => Arc::new(PostgresStore::connect(url).await?),
    })
}

/// Name of the table sqlx uses to track applied migrations.
const MIGRATIONS_TABLE: &str = "_sqlx_migrations";

fn group_columns(rows: Vec<(String, String)>) -> Vec<Table> {
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
}
