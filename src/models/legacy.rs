//! D-099 — adopting the rows a pre-ramps instance left behind.
//!
//! The migration filled `ramp` with `''` on every existing row. This moves them
//! into the configured `default_ramp`, once, at startup, before any listener
//! binds.

use sqlx::{PgPool, Row};

use crate::quota::store::{Adoption, QuotaError};

/// Every `''` row into `ramp`, in one transaction.
///
/// The transaction-scoped advisory lock serialises replicas starting together:
/// the second waits, then finds nothing left to move. A legacy key that already
/// exists under `ramp` stops startup instead of merging two histories.
/// `quota_reservation` and `recipient_event` have no unique key, so they cannot
/// clash.
pub async fn adopt(pool: &PgPool, ramp: &str) -> Result<Adoption, QuotaError> {
    let mut tx = pool.begin().await?;

    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('simmer_adopt_legacy_rows'))")
        .execute(&mut *tx)
        .await?;

    let clash = sqlx::query(
        r#"
        SELECT l.route, l.domain_group, l.day_index
        FROM quota_usage l
        JOIN quota_usage t
          ON t.ramp = $1
         AND t.route = l.route
         AND t.domain_group = l.domain_group
         AND t.day_index = l.day_index
        WHERE l.ramp = ''
        LIMIT 1
        "#,
    )
    .bind(ramp)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(r) = clash {
        return Err(QuotaError::LegacyConflict(format!(
            "quota_usage ({}, {}, day {}) exists both before ramps and under ramp '{ramp}'",
            r.try_get::<String, _>("route")?,
            r.try_get::<String, _>("domain_group")?,
            r.try_get::<i64, _>("day_index")?,
        )));
    }

    let clash = sqlx::query(
        r#"
        SELECT l.route
        FROM route_state l
        JOIN route_state t ON t.ramp = $1 AND t.route = l.route
        WHERE l.ramp = ''
        LIMIT 1
        "#,
    )
    .bind(ramp)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(r) = clash {
        return Err(QuotaError::LegacyConflict(format!(
            "route_state '{}' exists both before ramps and under ramp '{ramp}'",
            r.try_get::<String, _>("route")?,
        )));
    }

    let routes = sqlx::query(
        r#"
        SELECT route FROM quota_usage WHERE ramp = ''
        UNION SELECT route FROM quota_reservation WHERE ramp = ''
        UNION SELECT route FROM route_state WHERE ramp = ''
        UNION SELECT route FROM recipient_event WHERE ramp = ''
        ORDER BY route
        "#,
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|r| r.try_get::<String, _>("route"))
    .collect::<Result<Vec<_>, _>>()?;

    let mut moved = [0u64; 4];
    for (i, table) in [
        "quota_usage",
        "quota_reservation",
        "route_state",
        "recipient_event",
    ]
    .iter()
    .enumerate()
    {
        // The table name is one of four literals above, never input.
        moved[i] = sqlx::query(&format!("UPDATE {table} SET ramp = $1 WHERE ramp = ''"))
            .bind(ramp)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }

    tx.commit().await?;

    Ok(Adoption {
        quota_usage: moved[0],
        quota_reservation: moved[1],
        route_state: moved[2],
        recipient_event: moved[3],
        routes,
    })
}
