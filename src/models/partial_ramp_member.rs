//! `partial_ramp_member` — D-118's sticky membership of an auto partial ramp.
//!
//! A recipient a message was committed to on a route whose `schedule.share` is
//! `mode: auto` stays offered that route whatever the share later says (O-21).
//! The row is written inside §7.4 phase 3's transaction, like §7.3's events, and
//! carries the same kind of 16-byte keyed hash — never plaintext.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::frequency::Key;
use crate::quota::store::{MemberRoute, QuotaError};

/// Insert-if-absent, then move `last_committed_at` forward. Never backward: two
/// commits finishing out of order still leave the newer instant.
pub async fn record(
    tx: &mut Transaction<'_, Postgres>,
    ramp: &str,
    route: &str,
    member: &Key,
    at: DateTime<Utc>,
) -> Result<(), QuotaError> {
    sqlx::query(
        r#"
        INSERT INTO partial_ramp_member
            (ramp, route, recipient_hash, first_committed_at, last_committed_at)
        VALUES ($1, $2, $3, $4, $4)
        ON CONFLICT (ramp, route, recipient_hash) DO UPDATE
            SET last_committed_at = GREATEST(partial_ramp_member.last_committed_at,
                                             EXCLUDED.last_committed_at)
        "#,
    )
    .bind(ramp)
    .bind(route)
    .bind(member.as_bytes())
    .bind(at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn is_member(
    pool: &PgPool,
    ramp: &str,
    route: &str,
    member: &Key,
) -> Result<bool, QuotaError> {
    let row = sqlx::query(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM partial_ramp_member
            WHERE ramp = $1 AND route = $2 AND recipient_hash = $3
        ) AS member
        "#,
    )
    .bind(ramp)
    .bind(route)
    .bind(member.as_bytes())
    .fetch_one(pool)
    .await?;
    Ok(row.try_get("member")?)
}

pub async fn routes(pool: &PgPool) -> Result<Vec<MemberRoute>, QuotaError> {
    let rows = sqlx::query(
        r#"
        SELECT ramp, route, count(*) AS members, max(last_committed_at) AS last
        FROM partial_ramp_member
        GROUP BY ramp, route
        ORDER BY ramp, route
        "#,
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(MemberRoute {
                ramp: r.try_get("ramp")?,
                route: r.try_get("route")?,
                members: r.try_get("members")?,
                last_committed_at: r.try_get("last")?,
            })
        })
        .collect()
}

/// Drop a route's rows together, but only while none was committed at or after
/// `idle_since` — the guard lives in the statement, so a commit between the
/// sweeper's read and this cannot be taken with the route.
pub async fn drop_idle(
    pool: &PgPool,
    ramp: &str,
    route: &str,
    idle_since: DateTime<Utc>,
) -> Result<u64, QuotaError> {
    let done = sqlx::query(
        r#"
        DELETE FROM partial_ramp_member
        WHERE ramp = $1 AND route = $2
          AND NOT EXISTS (
              SELECT 1 FROM partial_ramp_member recent
              WHERE recent.ramp = $1 AND recent.route = $2
                AND recent.last_committed_at >= $3
          )
        "#,
    )
    .bind(ramp)
    .bind(route)
    .bind(idle_since)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}
