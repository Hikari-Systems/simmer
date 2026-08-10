//! §7.3's sweeper: "A sweeper evicts rows older than the longest configured
//! window plus a margin, on an interval."
//!
//! `quota::sweeper`'s shape exactly, on the same shutdown token, with two
//! differences that follow from what it is sweeping.
//!
//! **It runs hourly rather than every minute.** The quota sweeper is releasing
//! headroom that a live route is waiting for, so a minute's delay is a minute of
//! under-reported allowance. Nothing waits for this one: a `recipient_event` row
//! past its retention is invisible to every window that will ever be evaluated,
//! so it costs only the disk it sits on.
//!
//! **A failure is not interesting on its own.** The quota sweeper's counter is a
//! signal — §7.4 asks for it, because a nonzero rate means crashes or a mistuned
//! expiry. This one sweeping nothing is the *normal* state on a quiet instance,
//! and sweeping thousands is equally normal on a busy one. What matters is that
//! it keeps running, which is why a failed pass logs and waits for the next tick
//! rather than giving up.

use std::sync::Arc;
use std::time::Duration;

use crate::metrics;
use crate::quota::store::QuotaStore;
use crate::smtp::Shutdown;

/// How often to sweep. Retention is at least an hour (§7.3's shortest window is
/// `hourly × 1`) plus a margin, so an hour's granularity can never let a row
/// outlive its retention by more than the retention itself.
const INTERVAL: Duration = Duration::from_secs(3_600);

/// Run until `shutdown` fires.
///
/// `retention` is [`crate::frequency::retention`]'s answer. The caller does not
/// start this task at all when no route declares a constraint, because then
/// nothing ever writes a row for it to sweep.
pub async fn run(store: Arc<dyn QuotaStore>, retention: Duration, shutdown: Shutdown) {
    let mut ticker = tokio::time::interval(INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::info!(
        retention_secs = retention.as_secs(),
        interval_secs = INTERVAL.as_secs(),
        "recipient-event sweeper started"
    );

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => sweep_once(store.as_ref(), retention).await,
        }
    }

    tracing::info!("recipient-event sweeper stopped");
}

/// One pass. Split out so a test can drive it without waiting an hour.
pub async fn sweep_once(store: &dyn QuotaStore, retention: Duration) {
    let cutoff = match chrono::Duration::from_std(retention) {
        Ok(d) => chrono::Utc::now() - d,
        Err(_) => {
            // Only reachable past ~292 million years of retention. Sweeping
            // nothing is the safe answer: the alternative is a cutoff in the
            // future, which would delete rows that are still inside a window.
            tracing::error!(
                retention_secs = retention.as_secs(),
                "recipient-event retention is too large to compute a cutoff; skipping the sweep"
            );
            return;
        }
    };

    match store.sweep_recipient_events(cutoff).await {
        Ok(0) => {}
        Ok(n) => {
            tracing::debug!(
                evicted = n,
                cutoff = %cutoff.to_rfc3339(),
                "evicted recipient events past their retention"
            );
            metrics::recipient_events_evicted(n);
        }
        Err(e) => {
            // Nothing is waiting on this. §7.5's fail-closed posture is about
            // *granting* quota; a sweep that does not run grants nothing, it just
            // leaves rows that no window can see. The next tick retries.
            tracing::error!(error = %e, "recipient-event sweep failed");
        }
    }
}
