//! §7.4's sweeper: release reservations that outlived their expiry.
//!
//! "A sweeper releases expired reservations, covering process crashes mid-send.
//! Expiry release is logged and counted, since a nonzero rate indicates crashes
//! or a mistuned timeout."
//!
//! That last clause is the reason this is noisy rather than silent. In steady
//! state it should sweep nothing at all: every reservation is resolved by the
//! relay path, and §10.4 releases whatever is still outstanding at shutdown. A
//! nonzero `simmer_reservation_expired_total` means either the process is dying
//! without running its shutdown path, or `reservation_expiry` is tuned shorter
//! than real downstream latency — and the second one silently under-reports
//! headroom for the rest of the day.

use std::sync::Arc;
use std::time::Duration;

use crate::metrics;
use crate::quota::store::QuotaStore;
use crate::smtp::Shutdown;

/// How often to sweep. Reservations live for minutes (see
/// [`crate::quota::reservation_expiry`]), so a minute's granularity loses
/// nothing and keeps the query off the hot path.
const INTERVAL: Duration = Duration::from_secs(60);

/// Run until `shutdown` fires.
pub async fn run(store: Arc<dyn QuotaStore>, shutdown: Shutdown) {
    let mut ticker = tokio::time::interval(INTERVAL);
    // The first tick fires immediately; skipping the burst behaviour matters
    // because a delayed sweep is harmless and a stampede at startup is not.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => sweep_once(store.as_ref()).await,
        }
    }

    tracing::info!("reservation sweeper stopped");
}

/// One pass. Split out so a test can drive it without waiting a minute.
pub async fn sweep_once(store: &dyn QuotaStore) {
    match store.sweep_expired().await {
        Ok(expired) if expired.is_empty() => {}
        Ok(expired) => {
            for e in &expired {
                tracing::warn!(
                    route = %e.route,
                    count = e.count,
                    "released an expired reservation; the process either died mid-send \
                     or the reservation expiry is shorter than real downstream latency (§7.4)"
                );
                metrics::reservation_expired(&e.route, e.count);
            }
        }
        Err(e) => {
            // §7.5's fail-closed posture is about *granting* quota. A sweeper
            // that cannot run does not grant anything; it just delays a release,
            // and the next tick retries.
            tracing::error!(error = %e, "reservation sweep failed");
        }
    }
}
