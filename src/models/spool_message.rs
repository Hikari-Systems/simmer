//! §7.7's `spool_message` and `spool_ramp_state` rows (D-116), over Postgres.
//!
//! Free functions, as everywhere in `models/`, so
//! [`crate::quota::postgres`]'s `commit_and_complete` can put the row's
//! deletion inside §7.4's commit transaction.
//!
//! Every write a lease holder makes carries `lease_token = $token` in its
//! `WHERE`: a holder whose lease ran out and was re-claimed matches nothing.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::quota::store::QuotaError;
use crate::spool::store::{
    BookedSlot, ClaimRequest, Claimed, DeadEntry, DeadLetterRequest, DeadReason, LaneStats,
    NewSpooled, Reschedule, RetryDead, SpoolRampState, SpoolTotals,
};

const LIVE: &str = "state IN ('queued', 'leased')";

pub async fn enqueue(pool: &PgPool, m: &NewSpooled) -> Result<(), QuotaError> {
    sqlx::query(
        r#"
        INSERT INTO spool_message
            (id, ramp, domain_group, group_basis, state, next_attempt_at, received_at,
             expires_at, envelope, body_ref, body_bytes, body_sha256, uuid_seed)
        VALUES ($1, $2, $3, $4, 'queued', $5, $6, $7, $8, $9, $10, $11, $12)
        "#,
    )
    .bind(m.id)
    .bind(&m.ramp)
    .bind(&m.domain_group)
    .bind(&m.group_basis)
    .bind(m.next_attempt_at)
    .bind(m.received_at)
    .bind(m.expires_at)
    .bind(&m.envelope)
    .bind(&m.body_ref)
    .bind(m.body_bytes)
    .bind(&m.body_sha256)
    .bind(m.uuid_seed)
    .execute(pool)
    .await?;
    Ok(())
}

fn claimed(r: &PgRow) -> Result<Claimed, QuotaError> {
    let booked = match (
        r.try_get::<Option<String>, _>("booked_route")?,
        r.try_get::<Option<String>, _>("booked_group")?,
        r.try_get::<Option<DateTime<Utc>>, _>("booked_tat")?,
    ) {
        (Some(route), Some(domain_group), Some(tat)) => Some(BookedSlot {
            route,
            domain_group,
            tat,
        }),
        _ => None,
    };
    Ok(Claimed {
        id: r.try_get("id")?,
        ramp: r.try_get("ramp")?,
        domain_group: r.try_get("domain_group")?,
        group_basis: r.try_get("group_basis")?,
        received_at: r.try_get("received_at")?,
        expires_at: r.try_get("expires_at")?,
        next_attempt_at: r.try_get("next_attempt_at")?,
        envelope: r.try_get("envelope")?,
        body_ref: r.try_get("body_ref")?,
        body_bytes: r.try_get("body_bytes")?,
        body_sha256: r.try_get("body_sha256")?,
        uuid_seed: r.try_get("uuid_seed")?,
        attempts: r.try_get("attempts")?,
        pinned_route: r.try_get("pinned_route")?,
        booked,
        lease_token: r
            .try_get::<Option<Uuid>, _>("lease_token")?
            .ok_or_else(|| QuotaError::Storage("a claimed row has no lease_token".into()))?,
        lease_until: r
            .try_get::<Option<DateTime<Utc>>, _>("lease_until")?
            .ok_or_else(|| QuotaError::Storage("a claimed row has no lease_until".into()))?,
    })
}

/// One statement: the CTE's `FOR UPDATE SKIP LOCKED` takes the row locks and
/// passes over rows another claimant is taking, and the `UPDATE` leases what it
/// locked. `gen_random_uuid()` is volatile, so every row gets its own token.
pub async fn claim_due(pool: &PgPool, req: &ClaimRequest) -> Result<Vec<Claimed>, QuotaError> {
    let rows = sqlx::query(
        r#"
        WITH due AS (
            SELECT m.id
              FROM spool_message m
             WHERE m.next_attempt_at <= $1
               AND (m.state = 'queued' OR (m.state = 'leased' AND m.lease_until < $1))
               AND NOT EXISTS (SELECT 1 FROM spool_ramp_state s
                                WHERE s.ramp = m.ramp AND s.paused)
             ORDER BY m.next_attempt_at, m.id
             LIMIT $2
             FOR UPDATE SKIP LOCKED
        )
        UPDATE spool_message m
           SET state = 'leased', lease_owner = $3, lease_until = $4,
               lease_token = gen_random_uuid()
          FROM due
         WHERE m.id = due.id
        RETURNING m.*
        "#,
    )
    .bind(req.now)
    .bind(i64::from(req.batch))
    .bind(&req.owner)
    .bind(req.now + req.lease)
    .fetch_all(pool)
    .await?;
    let mut out = rows.iter().map(claimed).collect::<Result<Vec<_>, _>>()?;
    // `RETURNING` keeps no order.
    out.sort_by_key(|c| (c.next_attempt_at, c.id));
    Ok(out)
}

pub async fn renew_lease(
    pool: &PgPool,
    id: Uuid,
    token: Uuid,
    until: DateTime<Utc>,
) -> Result<bool, QuotaError> {
    let n = sqlx::query(
        "UPDATE spool_message SET lease_until = $3 \
         WHERE id = $1 AND lease_token = $2 AND state = 'leased'",
    )
    .bind(id)
    .bind(token)
    .bind(until)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

pub async fn reschedule(pool: &PgPool, r: &Reschedule) -> Result<bool, QuotaError> {
    let n = sqlx::query(
        r#"
        UPDATE spool_message
           SET state = 'queued', next_attempt_at = $3,
               attempts = attempts + CASE WHEN $4 THEN 1 ELSE 0 END,
               pinned_route = COALESCE($5, pinned_route),
               booked_route = $6, booked_group = $7, booked_tat = $8,
               last_code = COALESCE($9, last_code), last_error = COALESCE($10, last_error),
               lease_owner = NULL, lease_until = NULL, lease_token = NULL
         WHERE id = $1 AND lease_token = $2 AND state = 'leased'
        "#,
    )
    .bind(r.id)
    .bind(r.token)
    .bind(r.next_attempt_at)
    .bind(r.attempted)
    .bind(&r.pinned_route)
    .bind(r.booked.as_ref().map(|b| &b.route))
    .bind(r.booked.as_ref().map(|b| &b.domain_group))
    .bind(r.booked.as_ref().map(|b| b.tat))
    .bind(r.last_code)
    .bind(&r.last_error)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

pub async fn dead_letter(pool: &PgPool, d: &DeadLetterRequest) -> Result<bool, QuotaError> {
    let n = sqlx::query(
        r#"
        UPDATE spool_message
           SET state = 'dead', dead_reason = $3, dead_at = $4,
               attempts = attempts + CASE WHEN $5 THEN 1 ELSE 0 END,
               pinned_route = COALESCE($6, pinned_route),
               last_code = COALESCE($7, last_code), last_error = COALESCE($8, last_error),
               body_ref = CASE WHEN $9 THEN body_ref ELSE NULL END,
               booked_route = NULL, booked_group = NULL, booked_tat = NULL,
               lease_owner = NULL, lease_until = NULL, lease_token = NULL
         WHERE id = $1 AND lease_token = $2 AND state = 'leased'
        "#,
    )
    .bind(d.id)
    .bind(d.token)
    .bind(d.reason.as_str())
    .bind(d.at)
    .bind(d.attempted)
    .bind(&d.pinned_route)
    .bind(d.last_code)
    .bind(&d.last_error)
    .bind(d.keep_body)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Inside `commit_and_complete`'s transaction: delete the row whoever holds
/// it, and say whether `token` did.
pub async fn complete(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    token: Uuid,
) -> Result<bool, QuotaError> {
    let row = sqlx::query("DELETE FROM spool_message WHERE id = $1 RETURNING lease_token")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(match row {
        Some(r) => r.try_get::<Option<Uuid>, _>("lease_token")? == Some(token),
        None => false,
    })
}

pub async fn take_dead_bodies(
    pool: &PgPool,
    cutoff: DateTime<Utc>,
) -> Result<Vec<String>, QuotaError> {
    // The old value comes back through a self-join: `RETURNING` sees the new
    // row, whose body_ref is already NULL.
    let rows = sqlx::query(
        r#"
        UPDATE spool_message m SET body_ref = NULL
          FROM spool_message old
         WHERE m.id = old.id AND m.state = 'dead' AND m.dead_at < $1
           AND m.body_ref IS NOT NULL
        RETURNING old.body_ref
        "#,
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    rows.iter().map(|r| Ok(r.try_get("body_ref")?)).collect()
}

pub async fn purge_dead(pool: &PgPool, cutoff: DateTime<Utc>) -> Result<Vec<String>, QuotaError> {
    let rows = sqlx::query(
        "DELETE FROM spool_message WHERE state = 'dead' AND dead_at < $1 RETURNING body_ref",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        if let Some(body) = r.try_get::<Option<String>, _>("body_ref")? {
            out.push(body);
        }
    }
    Ok(out)
}

pub async fn known_body_refs(
    pool: &PgPool,
    refs: &[String],
) -> Result<HashSet<String>, QuotaError> {
    if refs.is_empty() {
        return Ok(HashSet::new());
    }
    let rows = sqlx::query("SELECT body_ref FROM spool_message WHERE body_ref = ANY($1)")
        .bind(refs)
        .fetch_all(pool)
        .await?;
    rows.iter().map(|r| Ok(r.try_get("body_ref")?)).collect()
}

pub async fn totals(pool: &PgPool) -> Result<SpoolTotals, QuotaError> {
    let row = sqlx::query(&format!(
        "SELECT COUNT(*) FILTER (WHERE {LIVE}) AS messages, \
                COALESCE(SUM(body_bytes) FILTER (WHERE body_ref IS NOT NULL), 0)::BIGINT AS bytes \
           FROM spool_message"
    ))
    .fetch_one(pool)
    .await?;
    Ok(SpoolTotals {
        messages: row.try_get("messages")?,
        bytes: row.try_get("bytes")?,
    })
}

pub async fn lane_depth(pool: &PgPool, ramp: &str, domain_group: &str) -> Result<i64, QuotaError> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM spool_message WHERE ramp = $1 AND domain_group = $2 AND {LIVE}"
    ))
    .bind(ramp)
    .bind(domain_group)
    .fetch_one(pool)
    .await?)
}

pub async fn lanes(pool: &PgPool) -> Result<Vec<LaneStats>, QuotaError> {
    let rows = sqlx::query(&format!(
        "SELECT ramp, domain_group, COUNT(*) AS depth, \
                COALESCE(SUM(body_bytes), 0)::BIGINT AS bytes, \
                MIN(received_at) AS oldest, MIN(next_attempt_at) AS next_at \
           FROM spool_message WHERE {LIVE} \
          GROUP BY ramp, domain_group ORDER BY ramp, domain_group"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(LaneStats {
                ramp: r.try_get("ramp")?,
                domain_group: r.try_get("domain_group")?,
                depth: r.try_get("depth")?,
                bytes: r.try_get("bytes")?,
                oldest_received_at: r.try_get("oldest")?,
                next_attempt_at: r.try_get("next_at")?,
            })
        })
        .collect()
}

pub async fn dead_entries(pool: &PgPool, limit: u32) -> Result<Vec<DeadEntry>, QuotaError> {
    let rows = sqlx::query(
        "SELECT id, ramp, domain_group, pinned_route, dead_reason, last_code, last_error, \
                attempts, received_at, dead_at, body_ref IS NOT NULL AS body_retained \
           FROM spool_message WHERE state = 'dead' \
          ORDER BY dead_at DESC, id LIMIT $1",
    )
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(DeadEntry {
                id: r.try_get("id")?,
                ramp: r.try_get("ramp")?,
                domain_group: r.try_get("domain_group")?,
                route: r.try_get("pinned_route")?,
                reason: r
                    .try_get::<Option<String>, _>("dead_reason")?
                    .as_deref()
                    .and_then(DeadReason::parse),
                last_code: r.try_get("last_code")?,
                last_error: r.try_get("last_error")?,
                attempts: r.try_get("attempts")?,
                received_at: r.try_get("received_at")?,
                dead_at: r.try_get("dead_at")?,
                body_retained: r.try_get("body_retained")?,
            })
        })
        .collect()
}

pub async fn retry_dead(
    pool: &PgPool,
    id: Uuid,
    now: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> Result<RetryDead, QuotaError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT body_ref IS NOT NULL AS has_body FROM spool_message \
         WHERE id = $1 AND state = 'dead' FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let outcome = match row {
        None => RetryDead::NotFound,
        Some(r) if !r.try_get::<bool, _>("has_body")? => RetryDead::NoBody,
        Some(_) => {
            sqlx::query(
                "UPDATE spool_message \
                    SET state = 'queued', next_attempt_at = $2, expires_at = $3, \
                        dead_reason = NULL, dead_at = NULL \
                  WHERE id = $1",
            )
            .bind(id)
            .bind(now)
            .bind(expires_at)
            .execute(&mut *tx)
            .await?;
            RetryDead::Requeued
        }
    };
    tx.commit().await?;
    Ok(outcome)
}

pub async fn delete_message(pool: &PgPool, id: Uuid) -> Result<Option<Option<String>>, QuotaError> {
    let row = sqlx::query("DELETE FROM spool_message WHERE id = $1 RETURNING body_ref")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => Ok(Some(r.try_get("body_ref")?)),
        None => Ok(None),
    }
}

pub async fn set_paused(pool: &PgPool, ramp: &str, paused: bool) -> Result<(), QuotaError> {
    sqlx::query(
        "INSERT INTO spool_ramp_state (ramp, paused) VALUES ($1, $2) \
         ON CONFLICT (ramp) DO UPDATE SET paused = EXCLUDED.paused, updated_at = now()",
    )
    .bind(ramp)
    .bind(paused)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_draining(pool: &PgPool, ramp: &str, draining: bool) -> Result<(), QuotaError> {
    sqlx::query(
        "INSERT INTO spool_ramp_state (ramp, draining) VALUES ($1, $2) \
         ON CONFLICT (ramp) DO UPDATE SET draining = EXCLUDED.draining, updated_at = now()",
    )
    .bind(ramp)
    .bind(draining)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn states(pool: &PgPool) -> Result<HashMap<String, SpoolRampState>, QuotaError> {
    let rows = sqlx::query("SELECT ramp, paused, draining FROM spool_ramp_state")
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get("ramp")?,
                SpoolRampState {
                    paused: r.try_get("paused")?,
                    draining: r.try_get("draining")?,
                },
            ))
        })
        .collect()
}
