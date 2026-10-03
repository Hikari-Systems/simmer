//! The spool's housekeeping (D-117, D-120, D-121), every ten minutes:
//!
//! 1. a dead letter whose `keep_body` has run out loses its body;
//! 2. a dead letter past `retention` is deleted, with any body it still names;
//! 3. a body no row names, older than [`ORPHAN_AGE`], is deleted — the
//!    backstop for a crash between `put` and the row's insert, and for a
//!    delete that failed after a delivery.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::Utc;

use super::body::ORPHAN_AGE;
use super::Spool;
use crate::metrics;
use crate::smtp::Shutdown;

pub const INTERVAL: Duration = Duration::from_secs(600);

/// How many refs one `known_body_refs` call asks about.
const CHUNK: usize = 500;

pub async fn run(spool: Arc<Spool>, stop: Shutdown) {
    let mut tick = tokio::time::interval(INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => sweep_once(&spool).await,
        }
    }
}

pub async fn sweep_once(spool: &Spool) {
    let now = Utc::now();
    let to_chrono = |d: Duration| chrono::Duration::from_std(d).unwrap_or_default();

    match spool
        .store
        .take_dead_bodies(now - to_chrono(spool.cfg.dead_letter.keep_body))
        .await
    {
        Ok(refs) => delete_all(spool, &refs, "dead letter past keep_body").await,
        Err(e) => tracing::warn!(error = %e, "spool sweep: releasing dead letters' bodies"),
    }

    match spool
        .store
        .purge_dead(now - to_chrono(spool.cfg.dead_letter.retention))
        .await
    {
        Ok(refs) => delete_all(spool, &refs, "dead letter past retention").await,
        Err(e) => tracing::warn!(error = %e, "spool sweep: purging dead letters"),
    }

    let cutoff = SystemTime::now() - ORPHAN_AGE;
    let candidates = match spool.body.list_older_than(cutoff).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "spool sweep: listing bodies");
            return;
        }
    };
    let mut swept = 0u64;
    for chunk in candidates.chunks(CHUNK) {
        let known = match spool.store.known_body_refs(chunk).await {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(error = %e, "spool sweep: checking bodies against rows");
                return;
            }
        };
        for body_ref in chunk.iter().filter(|r| !known.contains(*r)) {
            match spool.body.delete(body_ref).await {
                Ok(()) => swept += 1,
                Err(e) => tracing::warn!(error = %e, body_ref, "spool sweep: deleting an orphan"),
            }
        }
    }
    if swept > 0 {
        metrics::spool_orphans_swept(swept);
        tracing::info!(swept, "spool sweep deleted bodies no row named (D-117)");
    }
}

async fn delete_all(spool: &Spool, refs: &[String], why: &str) {
    for body_ref in refs {
        if let Err(e) = spool.body.delete(body_ref).await {
            tracing::warn!(error = %e, body_ref, why, "spool sweep: deleting a body");
        }
    }
}
