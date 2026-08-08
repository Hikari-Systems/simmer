//! The one §11 backend: §7.4's reserve/send/commit protocol over Postgres.
//!
//! ## Why three phases
//!
//! §7.4: "Counters increment on downstream success only. A failed send must not
//! consume allowance. That requires a three-phase protocol, because a post-hoc
//! increment allows two concurrent sessions to both observe the last remaining
//! slot."
//!
//! ## Why a row lock, and why no retry
//!
//! §3.2 step 3d says to "re-evaluate this route once" if reservation fails "due
//! to a concurrent claim". Under `INSERT … ON CONFLICT DO UPDATE`, contenders for
//! one `(route, domain_group, day_index)` row are serialised by the lock that
//! statement takes: by the time a session reads the row it is reading the truth,
//! and there is no lost claim to re-evaluate. The retry is dropped and a
//! lock-wait timeout is treated as a database failure per §7.5 (O-5, D-027).

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use super::store::{
    Expired, QuotaError, QuotaStore, Reservation, ReserveRequest, Reserved, RouteState, Usage,
};
use crate::models;

pub struct PgQuotaStore {
    pool: PgPool,
}

impl PgQuotaStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl QuotaStore for PgQuotaStore {
    async fn reserve(&self, req: &ReserveRequest) -> Result<Reserved, QuotaError> {
        let mut tx = self.pool.begin().await?;

        // Takes the row lock. Everything below runs with contenders queued
        // behind it, which is what makes the read-then-write safe.
        let usage = models::quota::lock_usage(
            &mut tx,
            &req.route,
            &req.domain_group,
            req.day_index,
            req.allowance,
        )
        .await?;

        if !usage.has_headroom_for(req.count) {
            // Roll back rather than commit: the only thing the transaction did
            // was touch `updated_at`, and rolling back also releases the lock
            // immediately for the next contender.
            tx.rollback().await?;
            return Ok(Reserved::NoHeadroom { usage });
        }

        let id = Uuid::new_v4();
        models::quota::insert_reservation(&mut tx, id, req).await?;

        tx.commit().await?;

        Ok(Reserved::Taken(Reservation {
            id,
            route: req.route.clone(),
            domain_group: req.domain_group.clone(),
            day_index: req.day_index,
            count: req.count,
        }))
    }

    async fn commit(&self, reservation: &Reservation) -> Result<(), QuotaError> {
        let mut tx = self.pool.begin().await?;

        let still_reserved = models::quota::take_reservation(&mut tx, reservation.id).await?;
        if !still_reserved {
            // The sweeper beat us: the send took longer than `expires_at`. The
            // message was still delivered, so the ramp has to count it.
            tracing::warn!(
                route = %reservation.route,
                reservation = %reservation.id,
                "reservation expired before the downstream replied; committing anyway. \
                 A nonzero rate here means the reservation expiry is tuned shorter than \
                 real downstream latency (§7.4)"
            );
        }

        models::quota::commit_usage(
            &mut tx,
            &reservation.route,
            &reservation.domain_group,
            reservation.day_index,
            reservation.count,
            still_reserved,
        )
        .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn release(&self, reservation: &Reservation) -> Result<(), QuotaError> {
        let mut tx = self.pool.begin().await?;

        if models::quota::take_reservation(&mut tx, reservation.id).await? {
            models::quota::release_usage(
                &mut tx,
                &reservation.route,
                &reservation.domain_group,
                reservation.day_index,
                reservation.count,
            )
            .await?;
        }
        // If it was already swept, the headroom is already back. Decrementing
        // again would take it from a different message's live reservation.

        tx.commit().await?;
        Ok(())
    }

    async fn usage(
        &self,
        route: &str,
        domain_group: &str,
        day_index: i64,
    ) -> Result<Usage, QuotaError> {
        Ok(
            models::quota::read_usage(&self.pool, route, domain_group, day_index)
                .await?
                .unwrap_or_default(),
        )
    }

    async fn route_states(
        &self,
    ) -> Result<std::collections::HashMap<String, RouteState>, QuotaError> {
        models::route_state::all(&self.pool).await
    }

    async fn sweep_expired(&self) -> Result<Vec<Expired>, QuotaError> {
        models::quota::sweep_expired(&self.pool).await
    }

    async fn is_available(&self) -> bool {
        crate::db::is_reachable(&self.pool).await
    }
}
