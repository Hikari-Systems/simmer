//! [`SpoolStore`] over SQL Server (D-116, D-084). Statement for statement the
//! Postgres store's (`models/spool_message.rs`), which carries the reasoning;
//! this file carries only what the translation changed.
//!
//! - **The claim.** `FOR UPDATE SKIP LOCKED` becomes a `TOP (n) … ORDER BY` CTE
//!   read `WITH (UPDLOCK, READPAST, ROWLOCK)`, updated through the CTE.
//!   `READPAST` passes over rows another claimant has locked; `UPDLOCK` holds
//!   the ones this statement read until it has leased them. `NEWID()` is
//!   evaluated per row, so every claim gets its own token. The scan is forced
//!   onto `spool_message_next`, in claim order: any plan that sorts first
//!   locks every candidate, and the other claimants then find nothing (see
//!   the migration). A claim chosen as a deadlock victim (1205) — seen on
//!   CI's SQL Server, never locally — is retried: the statement was rolled
//!   back whole, so nothing was claimed.
//! - **Fenced writes** report `@@ROWCOUNT` as a row, the house pattern here.
//! - `RETURNING` becomes `OUTPUT … INTO @table` and a `SELECT`, with the table
//!   variable's strings in the binary collation (see `sweep_expired`).

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tiberius::Row;
use uuid::Uuid;

use super::{commit_in, int, missing, opt_instant, opt_int, text, MssqlQuotaStore};
use crate::db::mssql;
use crate::quota::store::{QuotaError, Reservation};
use crate::spool::store::{
    BookedSlot, ClaimRequest, Claimed, DeadEntry, DeadLetterRequest, DeadReason, LaneStats,
    NewSpooled, Reschedule, RetryDead, SpoolRampState, SpoolStore, SpoolTotals,
};

const COLUMNS: &str = "id, ramp, domain_group, group_basis, received_at, expires_at, \
    next_attempt_at, envelope, body_ref, body_bytes, body_sha256, uuid_seed, attempts, \
    pinned_route, booked_route, booked_group, booked_tat, lease_token, lease_until";

fn opt_text(row: &Row, col: &str) -> Result<Option<String>, QuotaError> {
    Ok(row.try_get::<&str, _>(col)?.map(str::to_string))
}

fn uuid(row: &Row, col: &str) -> Result<Uuid, QuotaError> {
    row.try_get::<Uuid, _>(col)?
        .ok_or_else(|| QuotaError::Storage(format!("column {col} was unexpectedly NULL")))
}

fn instant(row: &Row, col: &str) -> Result<DateTime<Utc>, QuotaError> {
    opt_instant(row, col)?
        .ok_or_else(|| QuotaError::Storage(format!("column {col} was unexpectedly NULL")))
}

fn claimed(r: &Row) -> Result<Claimed, QuotaError> {
    let booked = match (
        opt_text(r, "booked_route")?,
        opt_text(r, "booked_group")?,
        opt_instant(r, "booked_tat")?,
    ) {
        (Some(route), Some(domain_group), Some(tat)) => Some(BookedSlot {
            route,
            domain_group,
            tat,
        }),
        _ => None,
    };
    Ok(Claimed {
        id: uuid(r, "id")?,
        ramp: text(r, "ramp")?,
        domain_group: text(r, "domain_group")?,
        group_basis: text(r, "group_basis")?,
        received_at: instant(r, "received_at")?,
        expires_at: instant(r, "expires_at")?,
        next_attempt_at: instant(r, "next_attempt_at")?,
        envelope: text(r, "envelope")?,
        body_ref: opt_text(r, "body_ref")?,
        body_bytes: int(r, "body_bytes")?,
        body_sha256: r
            .try_get::<&[u8], _>("body_sha256")?
            .map(<[u8]>::to_vec)
            .unwrap_or_default(),
        uuid_seed: uuid(r, "uuid_seed")?,
        attempts: int(r, "attempts")?,
        pinned_route: opt_text(r, "pinned_route")?,
        booked,
        lease_token: uuid(r, "lease_token")?,
        lease_until: instant(r, "lease_until")?,
    })
}

/// How many times a claim is tried when it is chosen as a deadlock victim.
const DEADLOCK_ATTEMPTS: u32 = 5;

/// SQL Server's 1205: "Transaction … was deadlocked … and has been chosen as
/// the deadlock victim. Rerun the transaction."
fn is_deadlock(e: &tiberius::error::Error) -> bool {
    matches!(e, tiberius::error::Error::Server(t) if t.code() == 1205)
}

/// `SELECT CAST(@@ROWCOUNT AS BIGINT) AS n` after a fenced write, as a bool.
async fn affected_one(
    client: &mut tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>,
    sql: &str,
    params: &[&dyn tiberius::ToSql],
) -> Result<bool, QuotaError> {
    let row = client
        .query(sql, params)
        .await?
        .into_row()
        .await?
        .ok_or_else(|| missing("a fenced write"))?;
    Ok(int(&row, "n")? == 1)
}

impl MssqlQuotaStore {
    async fn upsert_spool_state(
        &self,
        column: &'static str,
        ramp: &str,
        value: bool,
    ) -> Result<(), QuotaError> {
        debug_assert!(column == "paused" || column == "draining");
        let sql = format!(
            "BEGIN TRANSACTION; \
             UPDATE dbo.spool_ramp_state WITH (UPDLOCK, SERIALIZABLE) \
                SET {column} = @P2, updated_at = SYSUTCDATETIME() \
              WHERE ramp = @P1; \
             IF @@ROWCOUNT = 0 \
                 INSERT INTO dbo.spool_ramp_state (ramp, {column}) VALUES (@P1, @P2); \
             COMMIT TRANSACTION;"
        );
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        conn.client.execute(sql, &[&ramp, &value]).await?;
        conn.broken = false;
        Ok(())
    }
}

#[async_trait]
impl SpoolStore for MssqlQuotaStore {
    async fn enqueue(&self, m: &NewSpooled) -> Result<(), QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        conn.client
            .execute(
                "INSERT INTO dbo.spool_message \
                     (id, ramp, domain_group, group_basis, state, next_attempt_at, received_at, \
                      expires_at, envelope, body_ref, body_bytes, body_sha256, uuid_seed) \
                 VALUES (@P1, @P2, @P3, @P4, N'queued', @P5, @P6, @P7, @P8, @P9, @P10, @P11, \
                         @P12);",
                &[
                    &m.id,
                    &m.ramp,
                    &m.domain_group,
                    &m.group_basis,
                    &m.next_attempt_at.naive_utc(),
                    &m.received_at.naive_utc(),
                    &m.expires_at.naive_utc(),
                    &m.envelope,
                    &m.body_ref,
                    &m.body_bytes,
                    &m.body_sha256.as_slice(),
                    &m.uuid_seed,
                ],
            )
            .await?;
        conn.broken = false;
        Ok(())
    }

    async fn claim_due(&self, req: &ClaimRequest) -> Result<Vec<Claimed>, QuotaError> {
        let sql = format!(
            "DECLARE @claimed TABLE (id UNIQUEIDENTIFIER); \
             WITH due AS ( \
                 SELECT TOP (@P2) m.state, m.lease_owner, m.lease_until, m.lease_token, m.id \
                   FROM dbo.spool_message m \
                        WITH (UPDLOCK, READPAST, ROWLOCK, INDEX(spool_message_next)) \
                  WHERE m.next_attempt_at <= @P1 \
                    AND (m.state = N'queued' OR (m.state = N'leased' AND m.lease_until < @P1)) \
                    AND NOT EXISTS (SELECT 1 FROM dbo.spool_ramp_state s \
                                     WHERE s.ramp = m.ramp AND s.paused = 1) \
                  ORDER BY m.next_attempt_at, m.id) \
             UPDATE due SET state = N'leased', lease_owner = @P3, lease_until = @P4, \
                            lease_token = NEWID() \
             OUTPUT inserted.id INTO @claimed; \
             SELECT {COLUMNS} FROM dbo.spool_message \
              WHERE id IN (SELECT id FROM @claimed) \
              ORDER BY next_attempt_at, id;"
        );
        let until = (req.now + req.lease).naive_utc();
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut conn = mssql::get(self.pool()).await?;
            conn.broken = true;
            let result = async {
                conn.client
                    .query(
                        sql.as_str(),
                        &[
                            &req.now.naive_utc(),
                            &i64::from(req.batch),
                            &req.owner,
                            &until,
                        ],
                    )
                    .await?
                    .into_first_result()
                    .await
            }
            .await;
            match result {
                Ok(rows) => {
                    let out = rows.iter().map(claimed).collect::<Result<Vec<_>, _>>()?;
                    conn.broken = false;
                    return Ok(out);
                }
                // Claimants updating the same few rows' index entries can
                // deadlock under `UPDLOCK`; SQL Server rolls the victim's whole
                // statement back, so nothing was claimed and trying again is
                // safe. A claim that keeps losing is reported, and the
                // dispatcher simply polls again (D-122).
                Err(e) if is_deadlock(&e) && attempt < DEADLOCK_ATTEMPTS => {
                    drop(conn);
                    let jitter = (uuid::Uuid::new_v4().as_u128() % 40) as u64;
                    tokio::time::sleep(std::time::Duration::from_millis(10 + jitter)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    async fn renew_lease(
        &self,
        id: Uuid,
        token: Uuid,
        until: DateTime<Utc>,
    ) -> Result<bool, QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let ok = affected_one(
            &mut conn.client,
            "UPDATE dbo.spool_message SET lease_until = @P3 \
              WHERE id = @P1 AND lease_token = @P2 AND state = N'leased'; \
             SELECT CAST(@@ROWCOUNT AS BIGINT) AS n;",
            &[&id, &token, &until.naive_utc()],
        )
        .await?;
        conn.broken = false;
        Ok(ok)
    }

    async fn reschedule(&self, r: &Reschedule) -> Result<bool, QuotaError> {
        let booked_route = r.booked.as_ref().map(|b| b.route.as_str());
        let booked_group = r.booked.as_ref().map(|b| b.domain_group.as_str());
        let booked_tat = r.booked.as_ref().map(|b| b.tat.naive_utc());
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let ok = affected_one(
            &mut conn.client,
            "UPDATE dbo.spool_message \
                SET state = N'queued', next_attempt_at = @P3, \
                    attempts = attempts + CASE WHEN @P4 = 1 THEN 1 ELSE 0 END, \
                    pinned_route = COALESCE(@P5, pinned_route), \
                    booked_route = @P6, booked_group = @P7, booked_tat = @P8, \
                    last_code = COALESCE(@P9, last_code), \
                    last_error = COALESCE(@P10, last_error), \
                    lease_owner = NULL, lease_until = NULL, lease_token = NULL \
              WHERE id = @P1 AND lease_token = @P2 AND state = N'leased'; \
             SELECT CAST(@@ROWCOUNT AS BIGINT) AS n;",
            &[
                &r.id,
                &r.token,
                &r.next_attempt_at.naive_utc(),
                &r.attempted,
                &r.pinned_route.as_deref(),
                &booked_route,
                &booked_group,
                &booked_tat,
                &r.last_code,
                &r.last_error.as_deref(),
            ],
        )
        .await?;
        conn.broken = false;
        Ok(ok)
    }

    async fn dead_letter(&self, d: &DeadLetterRequest) -> Result<bool, QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let ok = affected_one(
            &mut conn.client,
            "UPDATE dbo.spool_message \
                SET state = N'dead', dead_reason = @P3, dead_at = @P4, \
                    attempts = attempts + CASE WHEN @P5 = 1 THEN 1 ELSE 0 END, \
                    pinned_route = COALESCE(@P6, pinned_route), \
                    last_code = COALESCE(@P7, last_code), \
                    last_error = COALESCE(@P8, last_error), \
                    body_ref = CASE WHEN @P9 = 1 THEN body_ref ELSE NULL END, \
                    booked_route = NULL, booked_group = NULL, booked_tat = NULL, \
                    lease_owner = NULL, lease_until = NULL, lease_token = NULL \
              WHERE id = @P1 AND lease_token = @P2 AND state = N'leased'; \
             SELECT CAST(@@ROWCOUNT AS BIGINT) AS n;",
            &[
                &d.id,
                &d.token,
                &d.reason.as_str(),
                &d.at.naive_utc(),
                &d.attempted,
                &d.pinned_route.as_deref(),
                &d.last_code,
                &d.last_error.as_deref(),
                &d.keep_body,
            ],
        )
        .await?;
        conn.broken = false;
        Ok(ok)
    }

    async fn commit_and_complete(
        &self,
        reservation: &Reservation,
        recipient_keys: &[crate::frequency::Key],
        id: Uuid,
        token: Uuid,
    ) -> Result<bool, QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let client = &mut conn.client;
        client
            .simple_query("BEGIN TRANSACTION")
            .await?
            .into_results()
            .await?;
        commit_in(client, reservation, recipient_keys).await?;
        let row = client
            .query(
                "DECLARE @gone TABLE (lease_token UNIQUEIDENTIFIER); \
                 DELETE FROM dbo.spool_message OUTPUT deleted.lease_token INTO @gone \
                  WHERE id = @P1; \
                 SELECT lease_token FROM @gone;",
                &[&id],
            )
            .await?
            .into_row()
            .await?;
        let held = match row {
            Some(r) => r.try_get::<Uuid, _>("lease_token")? == Some(token),
            None => false,
        };
        client
            .simple_query("COMMIT TRANSACTION")
            .await?
            .into_results()
            .await?;
        conn.broken = false;
        Ok(held)
    }

    async fn take_dead_bodies(&self, cutoff: DateTime<Utc>) -> Result<Vec<String>, QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let rows = conn
            .client
            .query(
                "DECLARE @taken TABLE (body_ref NVARCHAR(512) COLLATE Latin1_General_100_BIN2); \
                 UPDATE dbo.spool_message SET body_ref = NULL \
                 OUTPUT deleted.body_ref INTO @taken \
                  WHERE state = N'dead' AND dead_at < @P1 AND body_ref IS NOT NULL; \
                 SELECT body_ref FROM @taken;",
                &[&cutoff.naive_utc()],
            )
            .await?
            .into_first_result()
            .await?;
        let out = rows
            .iter()
            .map(|r| text(r, "body_ref"))
            .collect::<Result<_, _>>()?;
        conn.broken = false;
        Ok(out)
    }

    async fn purge_dead(&self, cutoff: DateTime<Utc>) -> Result<Vec<String>, QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let rows = conn
            .client
            .query(
                "DECLARE @gone TABLE (body_ref NVARCHAR(512) COLLATE Latin1_General_100_BIN2); \
                 DELETE FROM dbo.spool_message OUTPUT deleted.body_ref INTO @gone \
                  WHERE state = N'dead' AND dead_at < @P1; \
                 SELECT body_ref FROM @gone WHERE body_ref IS NOT NULL;",
                &[&cutoff.naive_utc()],
            )
            .await?
            .into_first_result()
            .await?;
        let out = rows
            .iter()
            .map(|r| text(r, "body_ref"))
            .collect::<Result<_, _>>()?;
        conn.broken = false;
        Ok(out)
    }

    async fn known_body_refs(&self, refs: &[String]) -> Result<HashSet<String>, QuotaError> {
        if refs.is_empty() {
            return Ok(HashSet::new());
        }
        let json = serde_json::to_string(refs)
            .map_err(|e| QuotaError::Storage(format!("encoding body refs: {e}")))?;
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let rows = conn
            .client
            .query(
                "SELECT body_ref FROM dbo.spool_message \
                  WHERE body_ref IN (SELECT CAST(value AS NVARCHAR(512)) \
                                            COLLATE Latin1_General_100_BIN2 \
                                       FROM OPENJSON(@P1));",
                &[&json],
            )
            .await?
            .into_first_result()
            .await?;
        let out = rows
            .iter()
            .map(|r| text(r, "body_ref"))
            .collect::<Result<_, _>>()?;
        conn.broken = false;
        Ok(out)
    }

    async fn totals(&self) -> Result<SpoolTotals, QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let row = conn
            .client
            .simple_query(
                "SELECT CAST(COALESCE(SUM(CASE WHEN state IN (N'queued', N'leased') \
                                               THEN 1 ELSE 0 END), 0) AS BIGINT) AS messages, \
                        CAST(COALESCE(SUM(CASE WHEN body_ref IS NOT NULL \
                                               THEN body_bytes ELSE 0 END), 0) AS BIGINT) AS bytes \
                   FROM dbo.spool_message;",
            )
            .await?
            .into_row()
            .await?
            .ok_or_else(|| missing("spool totals"))?;
        let out = SpoolTotals {
            messages: int(&row, "messages")?,
            bytes: int(&row, "bytes")?,
        };
        conn.broken = false;
        Ok(out)
    }

    async fn lane_depth(&self, ramp: &str, domain_group: &str) -> Result<i64, QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let row = conn
            .client
            .query(
                "SELECT CAST(COUNT(*) AS BIGINT) AS n FROM dbo.spool_message \
                  WHERE ramp = @P1 AND domain_group = @P2 AND state IN (N'queued', N'leased');",
                &[&ramp, &domain_group],
            )
            .await?
            .into_row()
            .await?
            .ok_or_else(|| missing("lane depth"))?;
        let n = int(&row, "n")?;
        conn.broken = false;
        Ok(n)
    }

    async fn lanes(&self) -> Result<Vec<LaneStats>, QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let rows = conn
            .client
            .simple_query(
                "SELECT ramp, domain_group, CAST(COUNT(*) AS BIGINT) AS depth, \
                        CAST(COALESCE(SUM(body_bytes), 0) AS BIGINT) AS bytes, \
                        MIN(received_at) AS oldest, MIN(next_attempt_at) AS next_at \
                   FROM dbo.spool_message WHERE state IN (N'queued', N'leased') \
                  GROUP BY ramp, domain_group ORDER BY ramp, domain_group;",
            )
            .await?
            .into_first_result()
            .await?;
        let out = rows
            .iter()
            .map(|r| {
                Ok(LaneStats {
                    ramp: text(r, "ramp")?,
                    domain_group: text(r, "domain_group")?,
                    depth: int(r, "depth")?,
                    bytes: int(r, "bytes")?,
                    oldest_received_at: opt_instant(r, "oldest")?,
                    next_attempt_at: opt_instant(r, "next_at")?,
                })
            })
            .collect::<Result<_, QuotaError>>()?;
        conn.broken = false;
        Ok(out)
    }

    async fn dead_entries(&self, limit: u32) -> Result<Vec<DeadEntry>, QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let rows = conn
            .client
            .query(
                "SELECT TOP (@P1) id, ramp, domain_group, pinned_route, dead_reason, last_code, \
                        last_error, attempts, received_at, dead_at, \
                        CAST(CASE WHEN body_ref IS NOT NULL THEN 1 ELSE 0 END AS BIT) \
                            AS body_retained \
                   FROM dbo.spool_message WHERE state = N'dead' \
                  ORDER BY dead_at DESC, id;",
                &[&i64::from(limit)],
            )
            .await?
            .into_first_result()
            .await?;
        let out = rows
            .iter()
            .map(|r| {
                Ok(DeadEntry {
                    id: uuid(r, "id")?,
                    ramp: text(r, "ramp")?,
                    domain_group: text(r, "domain_group")?,
                    route: opt_text(r, "pinned_route")?,
                    reason: opt_text(r, "dead_reason")?
                        .as_deref()
                        .and_then(DeadReason::parse),
                    last_code: opt_int(r, "last_code")?,
                    last_error: opt_text(r, "last_error")?,
                    attempts: int(r, "attempts")?,
                    received_at: instant(r, "received_at")?,
                    dead_at: opt_instant(r, "dead_at")?,
                    body_retained: r.try_get::<bool, _>("body_retained")?.unwrap_or(false),
                })
            })
            .collect::<Result<_, QuotaError>>()?;
        conn.broken = false;
        Ok(out)
    }

    async fn retry_dead(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<RetryDead, QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let row = conn
            .client
            .query(
                "BEGIN TRANSACTION; \
                 DECLARE @has BIGINT = NULL; \
                 SELECT @has = CASE WHEN body_ref IS NULL THEN 0 ELSE 1 END \
                   FROM dbo.spool_message WITH (UPDLOCK, ROWLOCK) \
                  WHERE id = @P1 AND state = N'dead'; \
                 IF @has = 1 \
                     UPDATE dbo.spool_message \
                        SET state = N'queued', next_attempt_at = @P2, expires_at = @P3, \
                            dead_reason = NULL, dead_at = NULL \
                      WHERE id = @P1; \
                 COMMIT TRANSACTION; \
                 SELECT CAST(COALESCE(@has, -1) AS BIGINT) AS has;",
                &[&id, &now.naive_utc(), &expires_at.naive_utc()],
            )
            .await?
            .into_row()
            .await?
            .ok_or_else(|| missing("retrying a dead letter"))?;
        let out = match int(&row, "has")? {
            1 => RetryDead::Requeued,
            0 => RetryDead::NoBody,
            _ => RetryDead::NotFound,
        };
        conn.broken = false;
        Ok(out)
    }

    async fn delete_message(&self, id: Uuid) -> Result<Option<Option<String>>, QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let row = conn
            .client
            .query(
                "DECLARE @gone TABLE (body_ref NVARCHAR(512) COLLATE Latin1_General_100_BIN2); \
                 DELETE FROM dbo.spool_message OUTPUT deleted.body_ref INTO @gone \
                  WHERE id = @P1; \
                 SELECT body_ref FROM @gone;",
                &[&id],
            )
            .await?
            .into_row()
            .await?;
        let out = match row {
            Some(r) => Some(opt_text(&r, "body_ref")?),
            None => None,
        };
        conn.broken = false;
        Ok(out)
    }

    async fn set_spool_paused(&self, ramp: &str, paused: bool) -> Result<(), QuotaError> {
        self.upsert_spool_state("paused", ramp, paused).await
    }

    async fn set_spool_draining(&self, ramp: &str, draining: bool) -> Result<(), QuotaError> {
        self.upsert_spool_state("draining", ramp, draining).await
    }

    async fn spool_states(&self) -> Result<HashMap<String, SpoolRampState>, QuotaError> {
        let mut conn = mssql::get(self.pool()).await?;
        conn.broken = true;
        let rows = conn
            .client
            .simple_query("SELECT ramp, paused, draining FROM dbo.spool_ramp_state;")
            .await?
            .into_first_result()
            .await?;
        let out = rows
            .iter()
            .map(|r| {
                Ok((
                    text(r, "ramp")?,
                    SpoolRampState {
                        paused: r.try_get::<bool, _>("paused")?.unwrap_or(false),
                        draining: r.try_get::<bool, _>("draining")?.unwrap_or(false),
                    },
                ))
            })
            .collect::<Result<_, QuotaError>>()?;
        conn.broken = false;
        Ok(out)
    }
}
