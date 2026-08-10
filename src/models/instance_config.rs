//! `instance_config` — §11's "the recipient hash salt and similar singletons".
//!
//! Runtime `sqlx` over `&PgPool`, per the hikari-systems data-service pattern.

use sqlx::{PgPool, Row};

use crate::quota::store::QuotaError;

/// The key §7.3's salt is stored under.
pub const RECIPIENT_HASH_SALT: &str = "recipient_hash_salt";

/// Read a singleton, or `None` if it has never been written.
pub async fn get(pool: &PgPool, key: &str) -> Result<Option<String>, QuotaError> {
    let row = sqlx::query("SELECT value FROM instance_config WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await?;

    row.map(|r| r.try_get::<String, _>("value"))
        .transpose()
        .map_err(Into::into)
}

/// Write a singleton **only if it is absent**, and return whatever is there
/// afterwards — which may be a value another writer inserted first.
///
/// This is the shape §7.3's salt needs. "The salt is generated once and
/// persisted" has to hold across restarts *and* across replicas: a second
/// instance that minted its own would give the two of them different views of
/// the same recipient, and a restart that minted a new one would silently reset
/// every window rather than failing visibly.
///
/// `ON CONFLICT DO NOTHING` plus a read is what makes that safe without a lock.
/// The insert either wins or does nothing, and the `SELECT` afterwards is
/// authoritative either way.
pub async fn get_or_insert(pool: &PgPool, key: &str, value: &str) -> Result<String, QuotaError> {
    sqlx::query(
        r#"
        INSERT INTO instance_config (key, value)
        VALUES ($1, $2)
        ON CONFLICT (key) DO NOTHING
        "#,
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;

    get(pool, key).await?.ok_or_else(|| {
        // Only reachable if something deleted the row between the insert and the
        // read. Failing is right: §7.5's posture is that a quota decision without
        // its state is not a decision, and the same holds for a window count
        // whose salt is unknown.
        QuotaError::Storage(format!(
            "instance_config '{key}' vanished between write and read"
        ))
    })
}
