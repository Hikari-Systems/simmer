//! D-116 — a ramp group's daily row, over `&PgPool` / a transaction, per the
//! house pattern. The `quota_usage` functions in [`super::quota`] with the
//! route and domain group taken out of the key; read those for the reasoning
//! behind each statement, which is the same.
//!
//! **Lock order:** every transaction that touches both rows takes the group
//! row first. The callers in [`crate::quota::postgres`] keep to it.

use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::quota::store::{QuotaError, Usage};

fn usage(r: &sqlx::postgres::PgRow) -> Result<Usage, QuotaError> {
    Ok(Usage {
        allowance: Some(r.try_get("allowance")?),
        allowance_override: None,
        committed: r.try_get("committed")?,
        reserved: r.try_get("reserved")?,
    })
}

/// Create the group's row if absent and **lock it**, as
/// [`super::quota::lock_usage`] does the route's. `allowance` and `step` are
/// written only on insert (D-026). The second value is "this call created it".
pub async fn lock(
    tx: &mut Transaction<'_, Postgres>,
    ramp_group: &str,
    day_index: i64,
    allowance: i64,
    step: i64,
) -> Result<(Usage, bool), QuotaError> {
    let row = sqlx::query(
        r#"
        INSERT INTO ramp_group_usage (ramp_group, day_index, allowance, step)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (ramp_group, day_index)
        DO UPDATE SET updated_at = now()
        RETURNING allowance, committed, reserved, (xmax = 0) AS created
        "#,
    )
    .bind(ramp_group)
    .bind(day_index)
    .bind(allowance)
    .bind(step)
    .fetch_one(&mut **tx)
    .await?;
    Ok((usage(&row)?, row.try_get("created")?))
}

/// `reserved += count`. Caller holds the lock.
pub async fn reserve(
    tx: &mut Transaction<'_, Postgres>,
    ramp_group: &str,
    day_index: i64,
    count: i64,
) -> Result<(), QuotaError> {
    sqlx::query(
        r#"
        UPDATE ramp_group_usage
        SET reserved = reserved + $3, updated_at = now()
        WHERE ramp_group = $1 AND day_index = $2
        "#,
    )
    .bind(ramp_group)
    .bind(day_index)
    .bind(count)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// §7.4 phase 3's success path on the group row; `still_reserved` as in
/// [`super::quota::commit_usage`].
pub async fn commit(
    tx: &mut Transaction<'_, Postgres>,
    ramp_group: &str,
    day_index: i64,
    count: i64,
    still_reserved: bool,
) -> Result<(), QuotaError> {
    let sql = if still_reserved {
        r#"
        UPDATE ramp_group_usage
        SET reserved = GREATEST(reserved - $3, 0),
            committed = committed + $3,
            updated_at = now()
        WHERE ramp_group = $1 AND day_index = $2
        "#
    } else {
        r#"
        UPDATE ramp_group_usage
        SET committed = committed + $3, updated_at = now()
        WHERE ramp_group = $1 AND day_index = $2
        "#
    };
    sqlx::query(sql)
        .bind(ramp_group)
        .bind(day_index)
        .bind(count)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// §7.4 phase 3's failure path on the group row.
pub async fn release(
    tx: &mut Transaction<'_, Postgres>,
    ramp_group: &str,
    day_index: i64,
    count: i64,
) -> Result<(), QuotaError> {
    sqlx::query(
        r#"
        UPDATE ramp_group_usage
        SET reserved = GREATEST(reserved - $3, 0), updated_at = now()
        WHERE ramp_group = $1 AND day_index = $2
        "#,
    )
    .bind(ramp_group)
    .bind(day_index)
    .bind(count)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Read the row without locking it. Absent is `None`.
pub async fn read(
    pool: &PgPool,
    ramp_group: &str,
    day_index: i64,
) -> Result<Option<Usage>, QuotaError> {
    sqlx::query(
        r#"
        SELECT allowance, committed, reserved
        FROM ramp_group_usage
        WHERE ramp_group = $1 AND day_index = $2
        "#,
    )
    .bind(ramp_group)
    .bind(day_index)
    .fetch_optional(pool)
    .await?
    .as_ref()
    .map(usage)
    .transpose()
}

/// D-115's two rows, for the group: `day_index`'s and the latest before it.
pub async fn advance_rows(
    pool: &PgPool,
    ramp_group: &str,
    day_index: i64,
) -> Result<crate::quota::AdvanceRows, QuotaError> {
    let rows = sqlx::query(
        r#"
        SELECT day_index, step, allowance, committed
        FROM ramp_group_usage
        WHERE ramp_group = $1 AND day_index <= $2
        ORDER BY day_index DESC
        LIMIT 2
        "#,
    )
    .bind(ramp_group)
    .bind(day_index)
    .fetch_all(pool)
    .await?;
    let rows = rows
        .into_iter()
        .map(|r| {
            Ok(crate::quota::advance::Row {
                day_index: r.try_get("day_index")?,
                step: Some(r.try_get("step")?),
                allowance: Some(r.try_get("allowance")?),
                committed: r.try_get("committed")?,
            })
        })
        .collect::<Result<Vec<_>, QuotaError>>()?;
    Ok(crate::quota::AdvanceRows::from_newest_first(
        day_index, &rows,
    ))
}
