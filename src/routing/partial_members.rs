//! D-118's member sweeper: a route's `partial_ramp_member` rows go together,
//! `member_retention_days` after the route last committed anything.
//!
//! `frequency::sweeper`'s shape, hourly, on the same shutdown token. Started
//! whatever the config says, unlike that one: a route that *was* auto and no
//! longer is still has rows, and they still have to go. The pass on a quiet
//! instance is one `GROUP BY` over an empty table.
//!
//! **Why per route, not per row.** O-21's promise is that a recipient ramped onto
//! a route stays ramped for the rest of the ramp. Evicting a row because *that
//! recipient* had not been mailed lately would break it for exactly the
//! recipients mailed least often. A route idle for `member_retention_days` has
//! finished, or been abandoned, and its members mean nothing.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::config::{AutoShare, Config};
use crate::metrics;
use crate::quota::store::QuotaStore;
use crate::smtp::Shutdown;

const INTERVAL: Duration = Duration::from_secs(3_600);

/// How many idle days `(ramp, route)` keeps its members for: the route's own
/// `member_retention_days` while its share is auto, else the default — a route
/// that is no longer auto, or no longer configured, writes no more rows and its
/// old ones still need a bound.
pub fn retention_days(config: &Config, ramp: &str, route: &str) -> u32 {
    config
        .ramps
        .get(ramp)
        .and_then(|r| r.route(route))
        .and_then(|r| r.warmup.as_ref())
        .and_then(|w| w.schedule.share.auto())
        .map_or(AutoShare::DEFAULT_MEMBER_RETENTION_DAYS, |a| {
            a.member_retention_days
        })
}

/// Run until `shutdown` fires.
pub async fn run(store: Arc<dyn QuotaStore>, config: Arc<Config>, shutdown: Shutdown) {
    let mut ticker = tokio::time::interval(INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(
        interval_secs = INTERVAL.as_secs(),
        "partial-ramp member sweeper started"
    );
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {
                if let Err(e) = sweep_once(store.as_ref(), &config, Utc::now()).await {
                    // Nothing waits on this; the next tick retries.
                    tracing::error!(error = %e, "partial-ramp member sweep failed");
                }
            }
        }
    }
    tracing::info!("partial-ramp member sweeper stopped");
}

/// One pass at `now`. Returns how many rows went. Split out so a test can drive
/// it without waiting an hour, and with whatever clock it likes.
pub async fn sweep_once(
    store: &dyn QuotaStore,
    config: &Config,
    now: DateTime<Utc>,
) -> Result<u64, crate::quota::QuotaError> {
    let mut evicted = 0;
    for r in store.partial_member_routes().await? {
        let days = retention_days(config, &r.ramp, &r.route);
        let idle_since = now - chrono::Duration::days(i64::from(days));
        if r.last_committed_at >= idle_since {
            metrics::partial_ramp_members(&r.ramp, &r.route, r.members);
            continue;
        }
        let n = store
            .drop_partial_members(&r.ramp, &r.route, idle_since)
            .await?;
        if n > 0 {
            tracing::info!(
                ramp = %r.ramp,
                route = %r.route,
                evicted = n,
                retention_days = days,
                last_committed_at = %r.last_committed_at.to_rfc3339(),
                "dropped a partial ramp's members: the route has committed nothing for its \
                 member_retention_days (D-118)"
            );
            metrics::partial_ramp_members_evicted(&r.ramp, &r.route, n);
            metrics::partial_ramp_members(&r.ramp, &r.route, 0);
        } else {
            // A commit landed between the read and the drop; the guard kept it.
            metrics::partial_ramp_members(&r.ramp, &r.route, r.members);
        }
        evicted += n;
    }
    Ok(evicted)
}
