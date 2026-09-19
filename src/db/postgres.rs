//! Postgres connection management and migrations — the default `postgres`
//! feature's half of [`crate::db`].
//!
//! Follows the hikari-systems data-service pattern for *organisation* — a
//! concrete `AppState`-style pool handle, plain-SQL migrations in `migrations/`
//! applied by `sqlx::migrate!` at startup, and runtime queries in `models/` — but
//! builds the pool directly rather than through the house `build_pool`.
//!
//! The reason is that `SPEC.md` §4.1 specifies `database.url`, a single
//! connection URL, whereas `build_pool` takes a `DbConfig` of discrete
//! host/port/user/password fields. Reconstructing one from the other would mean
//! parsing the URL only to re-serialise it. See `DECISIONS.md` D-005.

use std::str::FromStr;

use anyhow::Context;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

use crate::config::Database;
use crate::db::{PoolGauge, PoolStats};

/// Build the pool. Does not connect eagerly — §7.5 says an unreachable database
/// means `451` on every message, not a refusal to start, so the listener must
/// come up regardless and report the condition through `/health`.
pub fn build_pool(cfg: &Database) -> anyhow::Result<PgPool> {
    // D-084: a SQL Server connection string means the wrong image. Say so
    // rather than "not a valid Postgres connection URL".
    let lower = cfg.url.trim_start().to_ascii_lowercase();
    if lower.starts_with("jdbc:sqlserver:") || lower.starts_with("server=") {
        anyhow::bail!(
            "database.url is a SQL Server connection string, but this is the Postgres build \
             of simmer. Use the `-mssql` image (`hikarisystems/simmer:<version>-mssql`) for \
             SQL Server"
        );
    }
    let options = PgConnectOptions::from_str(&cfg.url)
        // Deliberately does not include the URL in the error: it carries the
        // password, and this error is going straight to a log.
        .context("database.url is not a valid Postgres connection URL")?;

    Ok(PgPoolOptions::new()
        .max_connections(cfg.max_connections)
        // §4.1's `connect_timeout` bounds how long a caller waits to get a usable
        // connection, which for a pool is the acquire path. Mapping it here keeps
        // the §8.4 timeout budget honest: a message must not sit waiting on the
        // pool for longer than the operator configured.
        .acquire_timeout(cfg.connect_timeout)
        .connect_lazy_with(options))
}

/// Apply migrations. Runs at startup, per §11 and the house pattern.
pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("applying migrations")?;
    Ok(())
}

/// Is the database reachable? Used by `GET /health` (§9.2).
pub async fn is_reachable(pool: &PgPool) -> bool {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(pool)
        .await
        .is_ok()
}

/// `simmer_db_pool_*` (D-075) read straight off the pool.
impl PoolGauge for PgPool {
    fn stats(&self) -> PoolStats {
        let size = u64::from(self.size());
        let idle = self.num_idle() as u64;
        PoolStats {
            in_use: size.saturating_sub(idle),
            idle,
            max: u64::from(self.options().get_max_connections()),
        }
    }
}
