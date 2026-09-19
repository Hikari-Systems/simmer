//! Storage backends — one per build (D-084).
//!
//! The default `postgres` feature is §11's backend, unchanged. The `mssql`
//! feature swaps in SQL Server behind the same [`QuotaStore`] trait, for the
//! `-mssql` image. Exactly one is compiled in: a binary that carried both would
//! have to choose at runtime, and the choice would then be a configuration
//! mistake waiting to happen rather than a property of the image someone pulled.

#[cfg(all(feature = "postgres", feature = "mssql"))]
compile_error!(
    "features `postgres` and `mssql` are mutually exclusive: build the SQL Server \
     image with `--no-default-features --features mssql` (D-084)"
);
#[cfg(not(any(feature = "postgres", feature = "mssql")))]
compile_error!("enable exactly one storage feature: `postgres` (the default) or `mssql` (D-084)");

use std::sync::Arc;

use crate::config::Database;
use crate::quota::QuotaStore;

#[cfg(feature = "mssql")]
pub mod mssql;
#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "postgres")]
pub use postgres::{build_pool, is_reachable, migrate};

/// Which backend this binary was built with. Logged at startup and reported by
/// `GET /health`, so "which image is this?" has an answer from outside.
#[cfg(feature = "postgres")]
pub const BACKEND: &str = "postgres";
#[cfg(feature = "mssql")]
pub const BACKEND: &str = "mssql";

/// A connection pool's occupancy, for `simmer_db_pool_*` (D-075).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStats {
    pub in_use: u64,
    pub idle: u64,
    pub max: u64,
}

/// Whatever pool the backend uses, reduced to what the gauges need. Kept off
/// [`QuotaStore`], which deliberately says nothing about connections.
pub trait PoolGauge: Send + Sync + 'static {
    fn stats(&self) -> PoolStats;
}

/// An opened backend: the store the relay uses and the pool the gauges read.
pub struct Backend {
    pub store: Arc<dyn QuotaStore>,
    pub gauge: Arc<dyn PoolGauge>,
}

/// Build the pool and apply migrations (§11). Does not require the database to
/// be reachable afterwards — §7.5 makes that a `451` per message, not a
/// refusal to start — but migrations do need it, exactly as before D-084.
pub async fn open(cfg: &Database) -> anyhow::Result<Backend> {
    #[cfg(feature = "postgres")]
    {
        let pool = postgres::build_pool(cfg)?;
        postgres::migrate(&pool).await?;
        Ok(Backend {
            store: Arc::new(crate::quota::PgQuotaStore::new(pool.clone())),
            gauge: Arc::new(pool),
        })
    }
    #[cfg(feature = "mssql")]
    {
        let pool = mssql::build_pool(cfg)?;
        mssql::migrate(&pool).await?;
        Ok(Backend {
            store: Arc::new(crate::quota::MssqlQuotaStore::new(pool.clone())),
            gauge: Arc::new(mssql::Gauge::new(pool, cfg.max_connections)),
        })
    }
}
