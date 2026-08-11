//! Postgres connection management and migrations.
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

/// Build the pool. Does not connect eagerly — §7.5 says an unreachable database
/// means `451` on every message, not a refusal to start, so the listener must
/// come up regardless and report the condition through `/health`.
pub fn build_pool(cfg: &Database) -> anyhow::Result<PgPool> {
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
