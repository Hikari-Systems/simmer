//! §7.4's reserve/send/commit protocol over SQL Server — the `mssql` build's
//! [`QuotaStore`] (D-084).
//!
//! Operation for operation the same as [`crate::quota::postgres`], which carries
//! the reasoning; this file carries only what the translation changed.
//!
//! ## The row lock
//!
//! Postgres serialises contenders for one `(route, domain_group, day_index)` row
//! with `INSERT … ON CONFLICT DO UPDATE`, which locks the row whether it inserted
//! or not. T-SQL has no `ON CONFLICT`, and the obvious replacement, `MERGE`, is
//! not safe here: without `HOLDLOCK` two sessions can both see "no row" and both
//! insert. The form used instead is the documented safe upsert:
//!
//! ```sql
//! UPDATE quota_usage WITH (UPDLOCK, SERIALIZABLE) SET … WHERE <key>;
//! IF @@ROWCOUNT = 0 INSERT INTO quota_usage …;
//! ```
//!
//! inside one transaction. `UPDLOCK` makes a second contender for an existing
//! row wait; `SERIALIZABLE` takes a key-range lock when the row is *absent*, so a
//! second contender cannot insert it either and instead waits, then updates the
//! row the first one created. After that statement the session holds an update
//! lock on the row until commit — what `ON CONFLICT DO UPDATE` gives Postgres.
//!
//! ## Everything else
//!
//! - `RETURNING` → `OUTPUT … INTO @table`, then one `SELECT`: under `SET NOCOUNT
//!   ON` that is the only result set a batch returns.
//! - `GREATEST(x, 0)` → `CASE`, so SQL Server 2017 works, not only 2022.
//! - The sweeper's single `DELETE … RETURNING` CTE becomes a `DELETE … OUTPUT`
//!   and an `UPDATE … FROM` in one transaction. Two sweepers still cannot
//!   double-release: only one `DELETE` can win a row.
//! - `UNNEST` over three arrays → `OPENJSON` over one JSON parameter, which keeps
//!   the statement text independent of how many keys are asked about.
//! - Instants are UTC `DATETIME2`, bound as `NaiveDateTime`.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tiberius::Row;
use uuid::Uuid;

use super::store::{
    Expired, QuotaError, QuotaStore, Reservation, ReserveRequest, Reserved, Reset, RouteState,
    Usage, UsageKey,
};
use crate::db::mssql::{self, Pool};

pub struct MssqlQuotaStore {
    pool: Pool,
}

impl MssqlQuotaStore {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }
}

impl From<tiberius::error::Error> for QuotaError {
    fn from(e: tiberius::error::Error) -> Self {
        QuotaError::Storage(e.to_string())
    }
}

/// A NOT NULL `BIGINT` column.
fn int(row: &Row, col: &str) -> Result<i64, QuotaError> {
    row.try_get::<i64, _>(col)?
        .ok_or_else(|| QuotaError::Storage(format!("column {col} was unexpectedly NULL")))
}

/// A nullable `BIGINT` column.
fn opt_int(row: &Row, col: &str) -> Result<Option<i64>, QuotaError> {
    Ok(row.try_get::<i64, _>(col)?)
}

fn text(row: &Row, col: &str) -> Result<String, QuotaError> {
    row.try_get::<&str, _>(col)?
        .map(str::to_string)
        .ok_or_else(|| QuotaError::Storage(format!("column {col} was unexpectedly NULL")))
}

fn usage(row: &Row) -> Result<Usage, QuotaError> {
    Ok(Usage {
        allowance: opt_int(row, "allowance")?,
        allowance_override: opt_int(row, "allowance_override")?,
        committed: int(row, "committed")?,
        reserved: int(row, "reserved")?,
    })
}

fn missing(what: &str) -> QuotaError {
    QuotaError::Storage(format!("{what} returned no row"))
}

/// The upsert-and-lock of the module docs, then the row it locked.
const LOCK_USAGE: &str = "\
    UPDATE dbo.quota_usage WITH (UPDLOCK, SERIALIZABLE) \
       SET updated_at = SYSUTCDATETIME() \
     WHERE route = @P1 AND domain_group = @P2 AND day_index = @P3; \
    IF @@ROWCOUNT = 0 \
        INSERT INTO dbo.quota_usage (route, domain_group, day_index, allowance) \
        VALUES (@P1, @P2, @P3, @P4); \
    SELECT allowance, allowance_override, committed, reserved \
      FROM dbo.quota_usage \
     WHERE route = @P1 AND domain_group = @P2 AND day_index = @P3;";

/// `GREATEST(reserved - n, 0)`, for SQL Server before 2022.
const RELEASE_USAGE: &str = "\
    UPDATE dbo.quota_usage \
       SET reserved = CASE WHEN reserved - @P4 < 0 THEN 0 ELSE reserved - @P4 END, \
           updated_at = SYSUTCDATETIME() \
     WHERE route = @P1 AND domain_group = @P2 AND day_index = @P3;";

/// Delete one reservation, reporting whether it was still there.
const TAKE_RESERVATION: &str = "\
    DELETE FROM dbo.quota_reservation WHERE id = @P1; \
    SELECT CAST(@@ROWCOUNT AS BIGINT) AS n;";

#[async_trait]
impl QuotaStore for MssqlQuotaStore {
    async fn reserve(&self, req: &ReserveRequest) -> Result<Reserved, QuotaError> {
        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        let client = &mut conn.client;

        // Opened as its own batch rather than inside the parameterised one: the
        // transaction spans two round trips, with the headroom decision made
        // here in Rust in between, exactly as the Postgres store makes it.
        client
            .simple_query("BEGIN TRANSACTION")
            .await?
            .into_results()
            .await?;

        let row = client
            .query(
                LOCK_USAGE,
                &[
                    &req.route,
                    &req.domain_group,
                    &req.day_index,
                    &req.allowance,
                ],
            )
            .await?
            .into_row()
            .await?
            .ok_or_else(|| missing("locking the quota row"))?;
        let current = usage(&row)?;

        if !req.over_cap && !current.has_headroom_for(req.count) {
            client
                .simple_query("ROLLBACK TRANSACTION")
                .await?
                .into_results()
                .await?;
            conn.broken = false;
            return Ok(Reserved::NoHeadroom { usage: current });
        }

        let id = Uuid::new_v4();
        client
            .execute(
                "UPDATE dbo.quota_usage \
                    SET reserved = reserved + @P4, updated_at = SYSUTCDATETIME() \
                  WHERE route = @P1 AND domain_group = @P2 AND day_index = @P3; \
                 INSERT INTO dbo.quota_reservation \
                     (id, route, domain_group, day_index, [count], correlation_id, expires_at) \
                 VALUES (@P5, @P1, @P2, @P3, @P4, @P6, @P7);",
                &[
                    &req.route,
                    &req.domain_group,
                    &req.day_index,
                    &req.count,
                    &id,
                    &req.correlation_id,
                    &req.expires_at.naive_utc(),
                ],
            )
            .await?;
        client
            .simple_query("COMMIT TRANSACTION")
            .await?
            .into_results()
            .await?;

        conn.broken = false;
        Ok(Reserved::Taken(Reservation {
            id,
            route: req.route.clone(),
            domain_group: req.domain_group.clone(),
            day_index: req.day_index,
            count: req.count,
        }))
    }

    async fn commit(
        &self,
        reservation: &Reservation,
        recipient_keys: &[crate::frequency::Key],
    ) -> Result<(), QuotaError> {
        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        let client = &mut conn.client;

        client
            .simple_query("BEGIN TRANSACTION")
            .await?
            .into_results()
            .await?;

        let taken = client
            .query(TAKE_RESERVATION, &[&reservation.id])
            .await?
            .into_row()
            .await?
            .ok_or_else(|| missing("deleting the reservation"))?;
        let still_reserved = int(&taken, "n")? > 0;
        if !still_reserved {
            // See `PgQuotaStore::commit`: delivered, so it counts.
            tracing::warn!(
                route = %reservation.route,
                reservation = %reservation.id,
                "reservation expired before the downstream replied; committing anyway. \
                 A nonzero rate here means the reservation expiry is tuned shorter than \
                 real downstream latency (§7.4)"
            );
        }

        let sql = if still_reserved {
            "UPDATE dbo.quota_usage \
                SET reserved = CASE WHEN reserved - @P4 < 0 THEN 0 ELSE reserved - @P4 END, \
                    committed = committed + @P4, \
                    updated_at = SYSUTCDATETIME() \
              WHERE route = @P1 AND domain_group = @P2 AND day_index = @P3;"
        } else {
            "UPDATE dbo.quota_usage \
                SET committed = committed + @P4, updated_at = SYSUTCDATETIME() \
              WHERE route = @P1 AND domain_group = @P2 AND day_index = @P3;"
        };
        client
            .execute(
                sql,
                &[
                    &reservation.route,
                    &reservation.domain_group,
                    &reservation.day_index,
                    &reservation.count,
                ],
            )
            .await?;

        // §7.4 phase 3's second clause, in the same transaction as the first.
        let sent_at = Utc::now().naive_utc();
        for key in recipient_keys {
            client
                .execute(
                    "INSERT INTO dbo.recipient_event (recipient_hash, route, sent_at) \
                     VALUES (@P1, @P2, @P3);",
                    &[&key.as_bytes(), &reservation.route, &sent_at],
                )
                .await?;
        }

        client
            .simple_query("COMMIT TRANSACTION")
            .await?
            .into_results()
            .await?;
        conn.broken = false;
        Ok(())
    }

    async fn release(&self, reservation: &Reservation) -> Result<(), QuotaError> {
        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        let client = &mut conn.client;

        client
            .simple_query("BEGIN TRANSACTION")
            .await?
            .into_results()
            .await?;

        let taken = client
            .query(TAKE_RESERVATION, &[&reservation.id])
            .await?
            .into_row()
            .await?
            .ok_or_else(|| missing("deleting the reservation"))?;
        // Already swept means the headroom is already back; see
        // `PgQuotaStore::release`.
        if int(&taken, "n")? > 0 {
            client
                .execute(
                    RELEASE_USAGE,
                    &[
                        &reservation.route,
                        &reservation.domain_group,
                        &reservation.day_index,
                        &reservation.count,
                    ],
                )
                .await?;
        }

        client
            .simple_query("COMMIT TRANSACTION")
            .await?
            .into_results()
            .await?;
        conn.broken = false;
        Ok(())
    }

    async fn usage(
        &self,
        route: &str,
        domain_group: &str,
        day_index: i64,
    ) -> Result<Usage, QuotaError> {
        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        let row = conn
            .client
            .query(
                "SELECT allowance, allowance_override, committed, reserved \
                   FROM dbo.quota_usage \
                  WHERE route = @P1 AND domain_group = @P2 AND day_index = @P3;",
                &[&route, &domain_group, &day_index],
            )
            .await?
            .into_row()
            .await?;
        let out = row.as_ref().map(usage).transpose()?.unwrap_or_default();
        conn.broken = false;
        Ok(out)
    }

    async fn usage_many(
        &self,
        keys: &[UsageKey],
    ) -> Result<HashMap<(String, String), Usage>, QuotaError> {
        if keys.is_empty() {
            return Ok(HashMap::new());
        }
        let json = serde_json::Value::Array(
            keys.iter()
                .map(|k| serde_json::json!({ "r": k.route, "g": k.domain_group, "d": k.day_index }))
                .collect(),
        )
        .to_string();

        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        // OPENJSON's columns take the database's default collation; the table's
        // keys are binary (see the migrations), so the comparison names it.
        let rows = conn
            .client
            .query(
                "SELECT q.route, q.domain_group, q.allowance, q.allowance_override, \
                        q.committed, q.reserved \
                   FROM dbo.quota_usage q \
                   JOIN OPENJSON(@P1) WITH ( \
                            route        NVARCHAR(200) '$.r', \
                            domain_group NVARCHAR(200) '$.g', \
                            day_index    BIGINT        '$.d') k \
                     ON q.route = k.route COLLATE Latin1_General_100_BIN2 \
                    AND q.domain_group = k.domain_group COLLATE Latin1_General_100_BIN2 \
                    AND q.day_index = k.day_index;",
                &[&json],
            )
            .await?
            .into_first_result()
            .await?;

        let out = rows
            .iter()
            .map(|r| Ok(((text(r, "route")?, text(r, "domain_group")?), usage(r)?)))
            .collect::<Result<_, QuotaError>>()?;
        conn.broken = false;
        Ok(out)
    }

    async fn set_paused(&self, route: &str, paused: bool) -> Result<(), QuotaError> {
        self.upsert_route_state("paused", route, paused).await
    }

    async fn set_graduated(&self, route: &str, graduated: bool) -> Result<(), QuotaError> {
        self.upsert_route_state("graduated", route, graduated).await
    }

    async fn set_allowance_override(
        &self,
        route: &str,
        domain_group: &str,
        day_index: i64,
        allowance: Option<i64>,
        scheduled: Option<i64>,
    ) -> Result<(), QuotaError> {
        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        // Balanced BEGIN/COMMIT inside one batch: the upsert's locks must span
        // both statements, or two writers could both find no row.
        conn.client
            .execute(
                "BEGIN TRANSACTION; \
                 UPDATE dbo.quota_usage WITH (UPDLOCK, SERIALIZABLE) \
                    SET allowance_override = @P4, updated_at = SYSUTCDATETIME() \
                  WHERE route = @P1 AND domain_group = @P2 AND day_index = @P3; \
                 IF @@ROWCOUNT = 0 \
                     INSERT INTO dbo.quota_usage \
                         (route, domain_group, day_index, allowance, allowance_override) \
                     VALUES (@P1, @P2, @P3, @P5, @P4); \
                 COMMIT TRANSACTION;",
                &[&route, &domain_group, &day_index, &allowance, &scheduled],
            )
            .await?;
        conn.broken = false;
        Ok(())
    }

    async fn reset_counters(
        &self,
        route: &str,
        domain_group: &str,
        day_index: i64,
    ) -> Result<Option<Reset>, QuotaError> {
        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        // See `models::quota::reset_counters` for why `reserved` is recomputed
        // from live reservations rather than zeroed. Under the row lock, so no
        // reservation can land between the read and the write.
        let row = conn
            .client
            .query(
                "BEGIN TRANSACTION; \
                 DECLARE @found BIT = 0, @c BIGINT, @r BIGINT, @after BIGINT; \
                 SELECT @found = 1, @c = committed, @r = reserved \
                   FROM dbo.quota_usage WITH (UPDLOCK, ROWLOCK) \
                  WHERE route = @P1 AND domain_group = @P2 AND day_index = @P3; \
                 IF @found = 1 \
                 BEGIN \
                     UPDATE q \
                        SET committed = 0, \
                            reserved = COALESCE(( \
                                SELECT SUM(r.[count]) FROM dbo.quota_reservation r \
                                 WHERE r.route = q.route \
                                   AND r.domain_group = q.domain_group \
                                   AND r.day_index = q.day_index), 0), \
                            updated_at = SYSUTCDATETIME() \
                       FROM dbo.quota_usage q \
                      WHERE q.route = @P1 AND q.domain_group = @P2 AND q.day_index = @P3; \
                     SELECT @after = reserved FROM dbo.quota_usage \
                      WHERE route = @P1 AND domain_group = @P2 AND day_index = @P3; \
                 END; \
                 COMMIT TRANSACTION; \
                 SELECT @found AS found, @c AS committed_before, @r AS reserved_before, \
                        @after AS reserved_after;",
                &[&route, &domain_group, &day_index],
            )
            .await?
            .into_row()
            .await?
            .ok_or_else(|| missing("resetting counters"))?;

        let found = row.try_get::<bool, _>("found")?.unwrap_or(false);
        let out = if found {
            Some(Reset {
                committed_before: int(&row, "committed_before")?,
                reserved_before: int(&row, "reserved_before")?,
                reserved_after: int(&row, "reserved_after")?,
            })
        } else {
            None
        };
        conn.broken = false;
        Ok(out)
    }

    async fn route_states(&self) -> Result<HashMap<String, RouteState>, QuotaError> {
        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        let rows = conn
            .client
            .simple_query("SELECT route, paused, graduated FROM dbo.route_state")
            .await?
            .into_first_result()
            .await?;
        let out = rows
            .iter()
            .map(|r| {
                Ok((
                    text(r, "route")?,
                    RouteState {
                        paused: r.try_get::<bool, _>("paused")?.unwrap_or(false),
                        graduated: r.try_get::<bool, _>("graduated")?.unwrap_or(false),
                    },
                ))
            })
            .collect::<Result<_, QuotaError>>()?;
        conn.broken = false;
        Ok(out)
    }

    async fn sweep_expired(&self) -> Result<Vec<Expired>, QuotaError> {
        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        // The table variables name the binary collation: a table variable's
        // strings otherwise take tempdb's default, and joining them to the
        // binary-collated keys is a collation conflict.
        let rows = conn
            .client
            .simple_query(
                "BEGIN TRANSACTION; \
                 DECLARE @expired TABLE ( \
                     route        NVARCHAR(200) COLLATE Latin1_General_100_BIN2, \
                     domain_group NVARCHAR(200) COLLATE Latin1_General_100_BIN2, \
                     day_index    BIGINT, \
                     n            BIGINT); \
                 DECLARE @done TABLE ( \
                     route NVARCHAR(200) COLLATE Latin1_General_100_BIN2, \
                     n     BIGINT); \
                 DELETE FROM dbo.quota_reservation \
                 OUTPUT deleted.route, deleted.domain_group, deleted.day_index, deleted.[count] \
                   INTO @expired \
                  WHERE expires_at < SYSUTCDATETIME(); \
                 UPDATE q \
                    SET reserved = CASE WHEN q.reserved - t.n < 0 THEN 0 ELSE q.reserved - t.n END, \
                        updated_at = SYSUTCDATETIME() \
                 OUTPUT inserted.route, t.n INTO @done \
                   FROM dbo.quota_usage q \
                   JOIN (SELECT route, domain_group, day_index, SUM(n) AS n \
                           FROM @expired GROUP BY route, domain_group, day_index) t \
                     ON q.route = t.route \
                    AND q.domain_group = t.domain_group \
                    AND q.day_index = t.day_index; \
                 COMMIT TRANSACTION; \
                 SELECT route, n FROM @done;",
            )
            .await?
            .into_first_result()
            .await?;
        let out = rows
            .iter()
            .map(|r| {
                Ok(Expired {
                    route: text(r, "route")?,
                    count: int(r, "n")?,
                })
            })
            .collect::<Result<_, QuotaError>>()?;
        conn.broken = false;
        Ok(out)
    }

    async fn recipient_event_count(
        &self,
        route: &str,
        key: &crate::frequency::Key,
        since: DateTime<Utc>,
    ) -> Result<i64, QuotaError> {
        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        let row = conn
            .client
            .query(
                "SELECT COUNT_BIG(*) AS n FROM dbo.recipient_event \
                  WHERE recipient_hash = @P1 AND route = @P2 AND sent_at >= @P3;",
                &[&key.as_bytes(), &route, &since.naive_utc()],
            )
            .await?
            .into_row()
            .await?
            .ok_or_else(|| missing("counting recipient events"))?;
        let n = int(&row, "n")?;
        conn.broken = false;
        Ok(n)
    }

    async fn recipient_hash_salt(&self) -> Result<Vec<u8>, QuotaError> {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;

        const KEY: &str = "recipient_hash_salt";
        let mine = B64.encode(crate::frequency::generate_salt());

        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        // Insert-if-absent, then read: whoever got there first wins and every
        // replica uses that one (D-050). The range lock is what stops two
        // first-starters from both inserting.
        let row = conn
            .client
            .query(
                "BEGIN TRANSACTION; \
                 IF NOT EXISTS (SELECT 1 FROM dbo.instance_config WITH (UPDLOCK, SERIALIZABLE) \
                                 WHERE [key] = @P1) \
                     INSERT INTO dbo.instance_config ([key], [value]) VALUES (@P1, @P2); \
                 COMMIT TRANSACTION; \
                 SELECT [value] FROM dbo.instance_config WHERE [key] = @P1;",
                &[&KEY, &mine],
            )
            .await?
            .into_row()
            .await?
            .ok_or_else(|| {
                QuotaError::Storage(format!(
                    "instance_config '{KEY}' vanished between write and read"
                ))
            })?;
        let stored = text(&row, "value")?;
        conn.broken = false;

        B64.decode(stored.as_bytes()).map_err(|e| {
            QuotaError::Storage(format!("instance_config '{KEY}' is not valid base64: {e}"))
        })
    }

    async fn sweep_recipient_events(&self, cutoff: DateTime<Utc>) -> Result<u64, QuotaError> {
        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        let row = conn
            .client
            .query(
                "DELETE FROM dbo.recipient_event WHERE sent_at < @P1; \
                 SELECT CAST(@@ROWCOUNT AS BIGINT) AS n;",
                &[&cutoff.naive_utc()],
            )
            .await?
            .into_row()
            .await?
            .ok_or_else(|| missing("sweeping recipient events"))?;
        let n = int(&row, "n")?;
        conn.broken = false;
        Ok(n.max(0) as u64)
    }

    async fn is_available(&self) -> bool {
        mssql::is_reachable(&self.pool).await
    }
}

impl MssqlQuotaStore {
    /// §9.3's pause and graduate: one boolean column of `route_state`, upserted.
    /// `column` is one of two literals from this file, never input.
    async fn upsert_route_state(
        &self,
        column: &'static str,
        route: &str,
        value: bool,
    ) -> Result<(), QuotaError> {
        debug_assert!(column == "paused" || column == "graduated");
        let sql = format!(
            "BEGIN TRANSACTION; \
             UPDATE dbo.route_state WITH (UPDLOCK, SERIALIZABLE) \
                SET {column} = @P2, updated_at = SYSUTCDATETIME() \
              WHERE route = @P1; \
             IF @@ROWCOUNT = 0 \
                 INSERT INTO dbo.route_state (route, {column}) VALUES (@P1, @P2); \
             COMMIT TRANSACTION;"
        );
        let mut conn = mssql::get(&self.pool).await?;
        conn.broken = true;
        conn.client.execute(sql, &[&route, &value]).await?;
        conn.broken = false;
        Ok(())
    }
}
