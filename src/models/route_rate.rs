//! D-111's `route_rate` rows: one GCRA `tat` per `(ramp, route, domain_group)`.
//!
//! Free functions, composed into one transaction by
//! [`crate::quota::postgres`]'s `book_rate_slot` and `unbook_rate_slot`.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::quota::store::{QuotaError, RateKey};

/// Create the bucket's row if absent and **lock it**, returning its `tat`.
///
/// `ON CONFLICT DO UPDATE` for `lock_usage`'s reason: it takes the row lock
/// whether or not the row already existed, which is what serialises contenders.
pub async fn lock(
    tx: &mut Transaction<'_, Postgres>,
    key: &RateKey,
) -> Result<Option<DateTime<Utc>>, QuotaError> {
    let row = sqlx::query(
        r#"
        INSERT INTO route_rate (ramp, route, domain_group)
        VALUES ($1, $2, $3)
        ON CONFLICT (ramp, route, domain_group)
        DO UPDATE SET updated_at = now()
        RETURNING tat
        "#,
    )
    .bind(&key.ramp)
    .bind(&key.route)
    .bind(&key.domain_group)
    .fetch_one(&mut **tx)
    .await?;
    Ok(row.try_get("tat")?)
}

/// Write a locked row's new `tat`.
pub async fn set_tat(
    tx: &mut Transaction<'_, Postgres>,
    key: &RateKey,
    tat: DateTime<Utc>,
) -> Result<(), QuotaError> {
    sqlx::query(
        "UPDATE route_rate SET tat = $4, updated_at = now() \
         WHERE ramp = $1 AND route = $2 AND domain_group = $3",
    )
    .bind(&key.ramp)
    .bind(&key.route)
    .bind(&key.domain_group)
    .bind(tat)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Lock an existing row for `unbook`, without creating one.
pub async fn lock_existing(
    tx: &mut Transaction<'_, Postgres>,
    key: &RateKey,
) -> Result<Option<DateTime<Utc>>, QuotaError> {
    let row = sqlx::query(
        "SELECT tat FROM route_rate \
         WHERE ramp = $1 AND route = $2 AND domain_group = $3 FOR UPDATE",
    )
    .bind(&key.ramp)
    .bind(&key.route)
    .bind(&key.domain_group)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(match row {
        Some(r) => r.try_get("tat")?,
        None => None,
    })
}

/// Every booked bucket in one ramp, unlocked.
pub async fn all(
    pool: &PgPool,
    ramp: &str,
) -> Result<HashMap<(String, String), DateTime<Utc>>, QuotaError> {
    let rows = sqlx::query(
        "SELECT route, domain_group, tat FROM route_rate WHERE ramp = $1 AND tat IS NOT NULL",
    )
    .bind(ramp)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok((
                (r.try_get("route")?, r.try_get("domain_group")?),
                r.try_get("tat")?,
            ))
        })
        .collect()
}
