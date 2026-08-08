//! §9.3 route-scoped admin state, persisted so mutations survive a restart.
//!
//! The per-domain-group allowance override is **not** here — §11 puts it on this
//! table, but this table is keyed on route alone and cannot represent it. It is a
//! column on `quota_usage` instead. See `DECISIONS.md` D-025.

use std::collections::HashMap;

use sqlx::{PgPool, Row};

use crate::quota::store::{QuotaError, RouteState};

/// Every route's state, in one query.
///
/// An absent row reads as the default — not paused, not graduated — so nothing
/// has to write a row for a route that has never been touched by an operator.
pub async fn all(pool: &PgPool) -> Result<HashMap<String, RouteState>, QuotaError> {
    let rows = sqlx::query("SELECT route, paused, graduated FROM route_state")
        .fetch_all(pool)
        .await?;

    rows.into_iter()
        .map(|r| {
            Ok((
                r.try_get::<String, _>("route")?,
                RouteState {
                    paused: r.try_get("paused")?,
                    graduated: r.try_get("graduated")?,
                },
            ))
        })
        .collect()
}

/// §9.3 `POST /routes/{name}/pause` and `/resume`. The endpoint is phase 7; the
/// write lives here so the chain walk can be tested against a paused route now.
pub async fn set_paused(pool: &PgPool, route: &str, paused: bool) -> Result<(), QuotaError> {
    sqlx::query(
        r#"
        INSERT INTO route_state (route, paused) VALUES ($1, $2)
        ON CONFLICT (route) DO UPDATE SET paused = $2, updated_at = now()
        "#,
    )
    .bind(route)
    .bind(paused)
    .execute(pool)
    .await?;
    Ok(())
}

/// §9.3 `POST /routes/{name}/graduate` — pin the route to its final schedule
/// value. §7.2 is explicit that routes never do this on their own.
pub async fn set_graduated(pool: &PgPool, route: &str, graduated: bool) -> Result<(), QuotaError> {
    sqlx::query(
        r#"
        INSERT INTO route_state (route, graduated) VALUES ($1, $2)
        ON CONFLICT (route) DO UPDATE SET graduated = $2, updated_at = now()
        "#,
    )
    .bind(route)
    .bind(graduated)
    .execute(pool)
    .await?;
    Ok(())
}

/// §9.3 `POST /routes/{name}/allowance` — override today's ceiling for one
/// domain group. Expires at the next day boundary by construction: it is a
/// column on the row for one `day_index`, and tomorrow is a different row.
pub async fn set_allowance_override(
    pool: &PgPool,
    route: &str,
    domain_group: &str,
    day_index: i64,
    allowance: Option<i64>,
    scheduled: Option<i64>,
) -> Result<(), QuotaError> {
    sqlx::query(
        r#"
        INSERT INTO quota_usage (route, domain_group, day_index, allowance, allowance_override)
        VALUES ($1, $2, $3, $5, $4)
        ON CONFLICT (route, domain_group, day_index)
        DO UPDATE SET allowance_override = $4, updated_at = now()
        "#,
    )
    .bind(route)
    .bind(domain_group)
    .bind(day_index)
    .bind(allowance)
    .bind(scheduled)
    .execute(pool)
    .await?;
    Ok(())
}
