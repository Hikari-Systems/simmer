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
    Expired, QuotaError, QuotaStore, Reservation, ReserveRequest, Reserved, Reset, RouteState,
    Usage, UsageKey,
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

    async fn commit(
        &self,
        reservation: &Reservation,
        recipient_keys: &[crate::frequency::Key],
    ) -> Result<(), QuotaError> {
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

        // §7.4 phase 3's second clause, in the same transaction as the first:
        // "move the count from `reserved` to `committed` **and record
        // recipient-frequency events**". Empty for a route with no constraint.
        if !recipient_keys.is_empty() {
            models::recipient_event::record(
                &mut tx,
                &reservation.route,
                recipient_keys,
                chrono::Utc::now(),
            )
            .await?;
        }

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

    async fn usage_many(
        &self,
        keys: &[UsageKey],
    ) -> Result<std::collections::HashMap<(String, String), Usage>, QuotaError> {
        models::quota::read_usage_many(&self.pool, keys).await
    }

    async fn set_paused(&self, route: &str, paused: bool) -> Result<(), QuotaError> {
        models::route_state::set_paused(&self.pool, route, paused).await
    }

    async fn set_graduated(&self, route: &str, graduated: bool) -> Result<(), QuotaError> {
        models::route_state::set_graduated(&self.pool, route, graduated).await
    }

    async fn set_allowance_override(
        &self,
        route: &str,
        domain_group: &str,
        day_index: i64,
        allowance: Option<i64>,
        scheduled: Option<i64>,
    ) -> Result<(), QuotaError> {
        models::route_state::set_allowance_override(
            &self.pool,
            route,
            domain_group,
            day_index,
            allowance,
            scheduled,
        )
        .await
    }

    async fn reset_counters(
        &self,
        route: &str,
        domain_group: &str,
        day_index: i64,
    ) -> Result<Option<Reset>, QuotaError> {
        models::quota::reset_counters(&self.pool, route, domain_group, day_index).await
    }

    async fn route_states(
        &self,
    ) -> Result<std::collections::HashMap<String, RouteState>, QuotaError> {
        models::route_state::all(&self.pool).await
    }

    async fn sweep_expired(&self) -> Result<Vec<Expired>, QuotaError> {
        models::quota::sweep_expired(&self.pool).await
    }

    async fn recipient_event_count(
        &self,
        route: &str,
        key: &crate::frequency::Key,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<i64, QuotaError> {
        models::recipient_event::count_since(&self.pool, route, key, since).await
    }

    async fn recipient_hash_salt(&self) -> Result<Vec<u8>, QuotaError> {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;

        // Generated locally and offered to the table; whoever got there first
        // wins, and both replicas end up using that one (D-050).
        let mine = B64.encode(crate::frequency::generate_salt());
        let stored = models::instance_config::get_or_insert(
            &self.pool,
            models::instance_config::RECIPIENT_HASH_SALT,
            &mine,
        )
        .await?;

        B64.decode(stored.as_bytes()).map_err(|e| {
            // The row exists but is not what we wrote. Failing is the only safe
            // answer: inventing a salt here would silently reset every window,
            // and §7.3's whole point is that the salt is the *same* one.
            QuotaError::Storage(format!(
                "instance_config '{}' is not valid base64: {e}",
                models::instance_config::RECIPIENT_HASH_SALT
            ))
        })
    }

    async fn sweep_recipient_events(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, QuotaError> {
        models::recipient_event::evict_before(&self.pool, cutoff).await
    }

    async fn is_available(&self) -> bool {
        crate::db::is_reachable(&self.pool).await
    }
}
