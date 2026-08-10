//! `recipient_event` — §7.3's rolling-window events, and §11's high-cardinality
//! table.
//!
//! Runtime `sqlx` over `&PgPool` / `&mut Transaction`, per the hikari-systems
//! data-service pattern. The recording function takes a transaction because
//! §7.4 phase 3 records events *in the same breath* as the commit: "on downstream
//! `2xx`, move the count from `reserved` to `committed` **and record
//! recipient-frequency events**". One transaction is what makes those two facts
//! impossible to disagree.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::frequency::Key;
use crate::quota::store::QuotaError;

/// How many events this route has recorded for this recipient since `since`.
///
/// Read without a lock, outside the reservation transaction (D-049). The window
/// is a steering input, not a ceiling: a concurrent pair that both read "2" under
/// a threshold of 3 both send, and the answer is one extra message on a warming
/// route rather than a corrupted ramp.
pub async fn count_since(
    pool: &PgPool,
    route: &str,
    key: &Key,
    since: DateTime<Utc>,
) -> Result<i64, QuotaError> {
    let row = sqlx::query(
        r#"
        SELECT count(*) AS n
        FROM recipient_event
        WHERE recipient_hash = $1 AND route = $2 AND sent_at >= $3
        "#,
    )
    .bind(key.as_bytes())
    .bind(route)
    .bind(since)
    .fetch_one(pool)
    .await?;

    Ok(row.try_get("n")?)
}

/// Record one event per key, inside the caller's transaction.
///
/// Called from §7.4 phase 3 on a downstream `2xx` only. A message that was not
/// delivered has not been seen by the recipient and must not count against a
/// window whose entire purpose is "how often has this person heard from us".
pub async fn record(
    tx: &mut Transaction<'_, Postgres>,
    route: &str,
    keys: &[Key],
    sent_at: DateTime<Utc>,
) -> Result<(), QuotaError> {
    for key in keys {
        sqlx::query(
            r#"
            INSERT INTO recipient_event (recipient_hash, route, sent_at)
            VALUES ($1, $2, $3)
            "#,
        )
        .bind(key.as_bytes())
        .bind(route)
        .bind(sent_at)
        .execute(&mut **tx)
        .await?;
    }

    Ok(())
}

/// §7.3's sweeper: "evicts rows older than the longest configured window plus a
/// margin". Returns how many went.
pub async fn evict_before(pool: &PgPool, cutoff: DateTime<Utc>) -> Result<u64, QuotaError> {
    let done = sqlx::query("DELETE FROM recipient_event WHERE sent_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?;

    Ok(done.rows_affected())
}
