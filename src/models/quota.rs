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

use std::collections::HashMap;

use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::quota::store::{Expired, QuotaError, ReserveRequest, Reset, Usage, UsageKey};

/// Create the `(ramp, route, domain_group, day_index)` row if absent and **lock it**,
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
    ramp: &str,
    route: &str,
    domain_group: &str,
    day_index: i64,
    allowance: Option<i64>,
    step: Option<i64>,
) -> Result<(Usage, bool), QuotaError> {
    // `xmax = 0` is true exactly for a row this statement inserted: the
    // conflict arm updates it, which sets `xmax`. Contenders on a fresh key
    // queue on the unique index, so one of them inserts and the rest update —
    // D-115's "decided once" rests on that.
    let row = sqlx::query(
        r#"
        INSERT INTO quota_usage (ramp, route, domain_group, day_index, allowance, step)
        VALUES ($5, $1, $2, $3, $4, $6)
        ON CONFLICT (ramp, route, domain_group, day_index)
        DO UPDATE SET updated_at = now()
        RETURNING allowance, allowance_override, committed, reserved, (xmax = 0) AS created
        "#,
    )
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .bind(allowance)
    .bind(ramp)
    .bind(step)
    .fetch_one(&mut **tx)
    .await?;

    Ok((
        Usage {
            allowance: row.try_get("allowance")?,
            allowance_override: row.try_get("allowance_override")?,
            committed: row.try_get("committed")?,
            reserved: row.try_get("reserved")?,
        },
        row.try_get("created")?,
    ))
}

/// D-115 — the element's row for `day_index` and the one before it, newest
/// first. The primary key's order serves the `ORDER BY … LIMIT` directly.
pub async fn read_advance_rows(
    pool: &PgPool,
    ramp: &str,
    route: &str,
    domain_group: &str,
    day_index: i64,
) -> Result<crate::quota::AdvanceRows, QuotaError> {
    let rows = sqlx::query(
        r#"
        SELECT day_index, step, allowance, committed
        FROM quota_usage
        WHERE ramp = $1 AND route = $2 AND domain_group = $3 AND day_index <= $4
        ORDER BY day_index DESC
        LIMIT 2
        "#,
    )
    .bind(ramp)
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .fetch_all(pool)
    .await?;

    let rows = rows
        .into_iter()
        .map(|r| {
            Ok(crate::quota::advance::Row {
                day_index: r.try_get("day_index")?,
                step: r.try_get("step")?,
                allowance: r.try_get("allowance")?,
                committed: r.try_get("committed")?,
            })
        })
        .collect::<Result<Vec<_>, QuotaError>>()?;
    Ok(crate::quota::AdvanceRows::from_newest_first(
        day_index, &rows,
    ))
}

/// Read a row without locking it. Absent means untouched today.
pub async fn read_usage(
    pool: &PgPool,
    ramp: &str,
    route: &str,
    domain_group: &str,
    day_index: i64,
) -> Result<Option<Usage>, QuotaError> {
    let row = sqlx::query(
        r#"
        SELECT allowance, allowance_override, committed, reserved
        FROM quota_usage
        WHERE ramp = $4 AND route = $1 AND domain_group = $2 AND day_index = $3
        "#,
    )
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .bind(ramp)
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

/// §9.2 — read many rows in one query.
///
/// Unnest-and-join rather than a `WHERE ... IN` over tuples so that the number
/// of rows requested does not change the statement text: the read API asks about
/// every route times every domain group, and a per-request statement shape would
/// defeat the prepared-statement cache for no benefit.
pub async fn read_usage_many(
    pool: &PgPool,
    ramp: &str,
    keys: &[UsageKey],
) -> Result<HashMap<(String, String), Usage>, QuotaError> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }

    let routes: Vec<&str> = keys.iter().map(|k| k.route.as_str()).collect();
    let groups: Vec<&str> = keys.iter().map(|k| k.domain_group.as_str()).collect();
    let days: Vec<i64> = keys.iter().map(|k| k.day_index).collect();

    let rows = sqlx::query(
        r#"
        SELECT q.route, q.domain_group, q.allowance, q.allowance_override,
               q.committed, q.reserved
        FROM quota_usage q
        JOIN UNNEST($1::text[], $2::text[], $3::bigint[]) AS k(route, domain_group, day_index)
          ON q.route = k.route
         AND q.domain_group = k.domain_group
         AND q.day_index = k.day_index
        WHERE q.ramp = $4
        "#,
    )
    .bind(&routes)
    .bind(&groups)
    .bind(&days)
    .bind(ramp)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok((
                (r.try_get("route")?, r.try_get("domain_group")?),
                Usage {
                    allowance: r.try_get("allowance")?,
                    allowance_override: r.try_get("allowance_override")?,
                    committed: r.try_get("committed")?,
                    reserved: r.try_get("reserved")?,
                },
            ))
        })
        .collect()
}

/// §9.3 `POST /quota/reset` — "reset counters for a route/group. Destructive."
///
/// Two things this deliberately does not do. It does not delete the row, because
/// `allowance` is authoritative once written (D-026) and re-creating it would
/// silently adopt whatever the schedule says now. And it does not zero
/// `reserved`: it recomputes it from the reservations that are actually
/// outstanding. Zeroing would give away headroom that an in-flight send already
/// holds, and the resulting overshoot of the day's ceiling is the exact failure
/// the §7.4 protocol exists to make impossible.
///
/// Under the row lock, so a reservation cannot be taken between the read and the
/// write.
pub async fn reset_counters(
    pool: &PgPool,
    ramp: &str,
    route: &str,
    domain_group: &str,
    day_index: i64,
) -> Result<Option<Reset>, QuotaError> {
    let mut tx = pool.begin().await?;

    let Some(before) = sqlx::query(
        r#"
        SELECT committed, reserved FROM quota_usage
        WHERE ramp = $4 AND route = $1 AND domain_group = $2 AND day_index = $3
        FOR UPDATE
        "#,
    )
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .bind(ramp)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };

    let row = sqlx::query(
        r#"
        UPDATE quota_usage q
        SET committed = 0,
            reserved = COALESCE((
                SELECT SUM(r.count)::BIGINT FROM quota_reservation r
                WHERE r.ramp = q.ramp
                  AND r.route = q.route
                  AND r.domain_group = q.domain_group
                  AND r.day_index = q.day_index
            ), 0),
            updated_at = now()
        WHERE q.ramp = $4 AND q.route = $1 AND q.domain_group = $2 AND q.day_index = $3
        RETURNING q.reserved
        "#,
    )
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .bind(ramp)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(Some(Reset {
        committed_before: before.try_get("committed")?,
        reserved_before: before.try_get("reserved")?,
        reserved_after: row.try_get("reserved")?,
    }))
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
        WHERE ramp = $5 AND route = $1 AND domain_group = $2 AND day_index = $3
        "#,
    )
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .bind(count)
    .bind(&req.ramp)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO quota_reservation
            (id, route, domain_group, day_index, count, correlation_id, expires_at, ramp,
             ramp_group, group_day_index)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        "#,
    )
    .bind(id)
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .bind(count)
    .bind(&req.correlation_id)
    .bind(req.expires_at)
    .bind(&req.ramp)
    .bind(req.group.as_ref().map(|g| g.name.as_str()))
    .bind(req.group.as_ref().map(|g| g.day_index))
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
    ramp: &str,
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
        WHERE ramp = $5 AND route = $1 AND domain_group = $2 AND day_index = $3
        "#
    } else {
        // The message *was* delivered, so the ramp must reflect it even though
        // the reservation is gone. Committing without the matching decrement is
        // the lesser error: it under-reports headroom for the rest of the day
        // rather than over-granting it.
        r#"
        UPDATE quota_usage
        SET committed = committed + $4, updated_at = now()
        WHERE ramp = $5 AND route = $1 AND domain_group = $2 AND day_index = $3
        "#
    };

    sqlx::query(sql)
        .bind(route)
        .bind(domain_group)
        .bind(day_index)
        .bind(count)
        .bind(ramp)
        .execute(&mut **tx)
        .await?;

    Ok(())
}

/// §7.4 phase 3, the failure path: give the headroom back.
pub async fn release_usage(
    tx: &mut Transaction<'_, Postgres>,
    ramp: &str,
    route: &str,
    domain_group: &str,
    day_index: i64,
    count: i64,
) -> Result<(), QuotaError> {
    sqlx::query(
        r#"
        UPDATE quota_usage
        SET reserved = GREATEST(reserved - $4, 0), updated_at = now()
        WHERE ramp = $5 AND route = $1 AND domain_group = $2 AND day_index = $3
        "#,
    )
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .bind(count)
    .bind(ramp)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// §7.4's sweeper: delete every expired reservation and give back what it held.
///
/// Safe under concurrency — §11 asks that the storage layer not preclude
/// horizontal scaling later, and D-007 keeps the shapes that cost nothing now.
/// Two sweepers running at once cannot double-release: only one `DELETE` can win
/// a row, and every decrement is derived from the rows that `DELETE` actually
/// removed.
///
/// D-116 made it a transaction of several statements rather than one CTE: a
/// reservation may also hold a ramp group's row, and every transaction touching
/// both locks the **group row first**. One statement updating both would take
/// them in whatever order the plan chose, which is how two transactions end up
/// each holding the lock the other wants.
pub async fn sweep_expired(pool: &PgPool) -> Result<Vec<Expired>, QuotaError> {
    let mut tx = pool.begin().await?;
    let taken = sqlx::query(
        r#"
        DELETE FROM quota_reservation
        WHERE expires_at < now()
        RETURNING ramp, route, domain_group, day_index, count, ramp_group, group_day_index
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;
    let out = give_back(&mut tx, &taken).await?;
    tx.commit().await?;
    Ok(out)
}

/// Decrement `reserved` for reservation rows already deleted: the group rows,
/// in key order, then the route rows, in key order. Returns one entry per route
/// row that was there to decrement.
async fn give_back(
    tx: &mut Transaction<'_, Postgres>,
    taken: &[sqlx::postgres::PgRow],
) -> Result<Vec<Expired>, QuotaError> {
    use std::collections::BTreeMap;

    let mut groups: BTreeMap<(String, i64), i64> = BTreeMap::new();
    let mut routes: BTreeMap<(String, String, String, i64), i64> = BTreeMap::new();
    for r in taken {
        let count: i64 = r.try_get("count")?;
        let group: Option<String> = r.try_get("ramp_group")?;
        let group_day: Option<i64> = r.try_get("group_day_index")?;
        if let (Some(g), Some(d)) = (group, group_day) {
            *groups.entry((g, d)).or_default() += count;
        }
        *routes
            .entry((
                r.try_get("ramp")?,
                r.try_get("route")?,
                r.try_get("domain_group")?,
                r.try_get("day_index")?,
            ))
            .or_default() += count;
    }

    for ((group, day), n) in &groups {
        crate::models::ramp_group_usage::release(tx, group, *day, *n).await?;
    }

    let mut out = Vec::new();
    for ((ramp, route, domain_group, day), n) in routes {
        let updated = sqlx::query(
            r#"
            UPDATE quota_usage
            SET reserved = GREATEST(reserved - $5, 0), updated_at = now()
            WHERE ramp = $1 AND route = $2 AND domain_group = $3 AND day_index = $4
            "#,
        )
        .bind(&ramp)
        .bind(&route)
        .bind(&domain_group)
        .bind(day)
        .bind(n)
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() > 0 {
            out.push(Expired {
                ramp,
                route,
                count: n,
            });
        }
    }
    Ok(out)
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

    // As the sweeper, and for its reason: group rows before route rows (D-116).
    let mut tx = pool.begin().await?;
    let taken = sqlx::query(
        r#"
        DELETE FROM quota_reservation
        WHERE id = ANY($1)
        RETURNING ramp, route, domain_group, day_index, count, ramp_group, group_day_index
        "#,
    )
    .bind(ids)
    .fetch_all(&mut *tx)
    .await?;
    let released = give_back(&mut tx, &taken).await?;
    tx.commit().await?;

    Ok(released.len() as u64)
}
