//! Storage for `mnix-server`.
//!
//! The server talks to its database only through the [`Store`] trait. There
//! are two implementations, [`SqliteStore`] (the default, single instance) and
//! [`PostgresStore`] (teams, replicas). No SQL lives outside the backend
//! crates.
//!
//! Each backend has its own migration directory (`migrations/sqlite`,
//! `migrations/postgres`) with the same numbering; the test suite checks that
//! both produce the same logical schema.

use std::sync::Arc;

pub use mnix_store_core::*;
pub use mnix_store_postgres::PostgresStore;
pub use mnix_store_sqlite::SqliteStore;

/// Opens the store for `url` (`sqlite://...` or `postgres://...`).
pub async fn connect(url: &str) -> Result<Arc<dyn Store>> {
    Ok(match Backend::from_url(url)? {
        Backend::Sqlite => Arc::new(SqliteStore::connect(url).await?),
        Backend::Postgres => Arc::new(PostgresStore::connect(url).await?),
    })
}
