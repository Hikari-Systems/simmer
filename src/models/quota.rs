//! Runtime `sqlx` over `&PgPool`, per the hikari-systems data-service pattern.
//!
//! No `query_as!` anywhere: it needs a live `DATABASE_URL` at *compile* time,
//! which breaks the Docker build (`CLAUDE.md`). Every query here is checked at
//! runtime and its columns read by name.
//!
//! These are free functions rather than methods so that the §7.4 transaction in
//! [`crate::quota::postgres`] can compose them inside one `BEGIN` — the
//! reservation protocol is only correct if the read, the check and the write
//! share a transaction and a row lock.

use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::quota::store::{Expired, QuotaError, ReserveRequest, Usage};

/// Create the `(route, domain_group, day_index)` row if absent and **lock it**,
/// returning what it says.
///
/// `ON CONFLICT DO UPDATE` rather than `DO NOTHING`: the former takes a row lock
/// even when the row already existed, which is what serialises concurrent
/// reservations. `DO NOTHING` would return no row on conflict and leave the
/// contenders unserialised — the exact race §7.4 exists to close.
///
/// `allowance` is written only on insert. §7.4's row is authoritative once
/// created (D-026), so `EXCLUDED.allowance` is deliberately not used in the
/// update: a config edit plus a restart must not raise today's ceiling.
pub async fn lock_usage(
    tx: &mut Transaction<'_, Postgres>,
    route: &str,
    domain_group: &str,
    day_index: i64,
    allowance: Option<i64>,
) -> Result<Usage, QuotaError> {
    let row = sqlx::query(
        r#"
        INSERT INTO quota_usage (route, domain_group, day_index, allowance)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (route, domain_group, day_index)
        DO UPDATE SET updated_at = now()
        RETURNING allowance, allowance_override, committed, reserved
        "#,
    )
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .bind(allowance)
    .fetch_one(&mut **tx)
    .await?;

    Ok(Usage {
        allowance: row.try_get("allowance")?,
        allowance_override: row.try_get("allowance_override")?,
        committed: row.try_get("committed")?,
        reserved: row.try_get("reserved")?,
    })
}

/// Read a row without locking it. Absent means untouched today.
pub async fn read_usage(
    pool: &PgPool,
    route: &str,
    domain_group: &str,
    day_index: i64,
) -> Result<Option<Usage>, QuotaError> {
    let row = sqlx::query(
        r#"
        SELECT allowance, allowance_override, committed, reserved
        FROM quota_usage
        WHERE route = $1 AND domain_group = $2 AND day_index = $3
        "#,
    )
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .fetch_optional(pool)
    .await?;

    row.map(|r| {
        Ok(Usage {
            allowance: r.try_get("allowance")?,
            allowance_override: r.try_get("allowance_override")?,
            committed: r.try_get("committed")?,
            reserved: r.try_get("reserved")?,
        })
    })
    .transpose()
}

/// Increment `reserved` and record the reservation. Caller holds the row lock.
pub async fn insert_reservation(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    req: &ReserveRequest,
) -> Result<(), QuotaError> {
    let (route, domain_group, day_index, count) =
        (&req.route, &req.domain_group, req.day_index, req.count);

    sqlx::query(
        r#"
        UPDATE quota_usage
        SET reserved = reserved + $4, updated_at = now()
        WHERE route = $1 AND domain_group = $2 AND day_index = $3
        "#,
    )
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .bind(count)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO quota_reservation
            (id, route, domain_group, day_index, count, correlation_id, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(id)
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .bind(count)
    .bind(&req.correlation_id)
    .bind(req.expires_at)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// Delete a reservation row, reporting whether it was still there.
///
/// `false` means the sweeper got there first — the process was slower than
/// `expires_at`. The caller has to know, because commit and release want
/// opposite things in that case.
pub async fn take_reservation(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<bool, QuotaError> {
    let deleted = sqlx::query("DELETE FROM quota_reservation WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// §7.4 phase 3, the success path: `reserved` down, `committed` up.
///
/// `still_reserved` is [`take_reservation`]'s answer. When it is `false` the
/// sweeper has already given the headroom back, so decrementing `reserved` again
/// would take it from some *other* message's live reservation.
pub async fn commit_usage(
    tx: &mut Transaction<'_, Postgres>,
    route: &str,
    domain_group: &str,
    day_index: i64,
    count: i64,
    still_reserved: bool,
) -> Result<(), QuotaError> {
    let sql = if still_reserved {
        r#"
        UPDATE quota_usage
        SET reserved = GREATEST(reserved - $4, 0),
            committed = committed + $4,
            updated_at = now()
        WHERE route = $1 AND domain_group = $2 AND day_index = $3
        "#
    } else {
        // The message *was* delivered, so the ramp must reflect it even though
        // the reservation is gone. Committing without the matching decrement is
        // the lesser error: it under-reports headroom for the rest of the day
        // rather than over-granting it.
        r#"
        UPDATE quota_usage
        SET committed = committed + $4, updated_at = now()
        WHERE route = $1 AND domain_group = $2 AND day_index = $3
        "#
    };

    sqlx::query(sql)
        .bind(route)
        .bind(domain_group)
        .bind(day_index)
        .bind(count)
        .execute(&mut **tx)
        .await?;

    Ok(())
}

/// §7.4 phase 3, the failure path: give the headroom back.
pub async fn release_usage(
    tx: &mut Transaction<'_, Postgres>,
    route: &str,
    domain_group: &str,
    day_index: i64,
    count: i64,
) -> Result<(), QuotaError> {
    sqlx::query(
        r#"
        UPDATE quota_usage
        SET reserved = GREATEST(reserved - $4, 0), updated_at = now()
        WHERE route = $1 AND domain_group = $2 AND day_index = $3
        "#,
    )
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .bind(count)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// §7.4's sweeper: delete every expired reservation and give back what it held,
/// in **one statement**.
///
/// Written as a CTE rather than a select-then-update loop so it is safe under
/// concurrency — §11 asks that the storage layer not preclude horizontal scaling
/// later, and D-007 keeps the shapes that cost nothing now. Two sweepers running
/// this simultaneously cannot double-release: only one `DELETE` can win a row,
/// and the decrement is derived from the rows that `DELETE` actually removed.
pub async fn sweep_expired(pool: &PgPool) -> Result<Vec<Expired>, QuotaError> {
    let rows = sqlx::query(
        r#"
        WITH expired AS (
            DELETE FROM quota_reservation
            WHERE expires_at < now()
            RETURNING route, domain_group, day_index, count
        ),
        totals AS (
            SELECT route, domain_group, day_index, SUM(count)::BIGINT AS n
            FROM expired
            GROUP BY route, domain_group, day_index
        )
        UPDATE quota_usage q
        SET reserved = GREATEST(q.reserved - t.n, 0), updated_at = now()
        FROM totals t
        WHERE q.route = t.route
          AND q.domain_group = t.domain_group
          AND q.day_index = t.day_index
        RETURNING q.route, t.n
        "#,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(Expired {
                route: r.try_get("route")?,
                count: r.try_get::<i64, _>("n")?,
            })
        })
        .collect()
}

/// §10.4 — release the reservations this process still holds, at shutdown.
///
/// Scoped to explicit ids rather than truncating the table (D-007): another
/// instance's live reservations are none of this one's business, and the
/// difference costs nothing to keep.
pub async fn release_by_ids(pool: &PgPool, ids: &[Uuid]) -> Result<u64, QuotaError> {
    if ids.is_empty() {
        return Ok(0);
    }

    let rows = sqlx::query(
        r#"
        WITH taken AS (
            DELETE FROM quota_reservation
            WHERE id = ANY($1)
            RETURNING route, domain_group, day_index, count
        ),
        totals AS (
            SELECT route, domain_group, day_index, SUM(count)::BIGINT AS n
            FROM taken
            GROUP BY route, domain_group, day_index
        )
        UPDATE quota_usage q
        SET reserved = GREATEST(q.reserved - t.n, 0), updated_at = now()
        FROM totals t
        WHERE q.route = t.route
          AND q.domain_group = t.domain_group
          AND q.day_index = t.day_index
        RETURNING q.route
        "#,
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;

    Ok(rows.len() as u64)
}
