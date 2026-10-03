//! §7.7's dispatcher (D-116): claims due messages and attempts each through the
//! same [`relay::attempt`] a session uses.
//!
//! One task polls every `dispatch.poll_interval` and claims as many due rows
//! as it has free slots (`dispatch.batch` in flight at most). Each claim is a
//! lease, renewed at half-time while its attempt runs; nothing holds a row lock
//! across a delivery. What an attempt came to decides the row's next state:
//!
//! | attempt                         | row                                     |
//! |---------------------------------|-----------------------------------------|
//! | `2xx`                           | deleted with the quota commit (D-122)   |
//! | deferred to a rate slot (D-118) | queued at the slot, the booking kept    |
//! | pool exhausted (§8.3)           | queued a second later; not an attempt   |
//! | `4xx`, connect, timeout, no route | queued after a jittered backoff       |
//! | ambiguous final dot (§10.2)     | queued after a backoff; at least once   |
//! | `5xx` at `RCPT TO`              | dead, `rejected` (D-120)                |
//! | hold ran out (Q4)               | dead, `expired`                         |
//! | body missing or changed         | dead, `corrupt`                         |
//!
//! Q3: once a message has been offered to a downstream it is pinned to that
//! route, and later attempts walk it alone unless it is paused, not started or
//! gone (`relay::attempt`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::field::Empty;
use tracing::{Instrument as _, Span};

use super::body::BodyError;
use super::envelope::Envelope;
use super::store::{BookedSlot, ClaimRequest, Claimed, DeadLetterRequest, DeadReason, Reschedule};
use super::webhook::{self, DeadLetterEvent};
use super::Spool;
use crate::config::SpoolRetry;
use crate::metrics;
use crate::relay::{self, AttemptCtx, AttemptKind, Engine, Failure, SelectError, SpoolCtx};
use crate::routing::ramp_select::{HeaderUse, Selection, Source};
use crate::routing::sender_match::Senders;
use crate::smtp::Shutdown;

/// Run until `stop`, then return the attempts still in flight for §10.4 to
/// await (D-106's rule: an attempt past the final dot is never dropped while
/// the bound allows it to finish).
pub async fn run(engine: Engine, spool: Arc<Spool>, stop: Shutdown) -> JoinSet<()> {
    let batch = spool.cfg.dispatch.batch.max(1) as usize;
    let slots = Arc::new(Semaphore::new(batch));
    let lease = lease_for(&engine);
    let mut tasks = JoinSet::new();
    let mut tick = tokio::time::interval(spool.cfg.dispatch.poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = tick.tick() => {}
        }
        while tasks.try_join_next().is_some() {}

        let free = slots.available_permits();
        if free == 0 {
            continue;
        }
        let claims = match spool
            .store
            .claim_due(&ClaimRequest {
                owner: spool.owner.clone(),
                now: Utc::now(),
                batch: u32::try_from(free).unwrap_or(u32::MAX),
                lease: chrono::Duration::from_std(lease)
                    .unwrap_or_else(|_| chrono::Duration::minutes(10)),
            })
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "spool claim failed; retrying next poll");
                continue;
            }
        };
        if !claims.is_empty() {
            tracing::info!(
                claimed = claims.len(),
                free_slots = free,
                owner = %spool.owner,
                "claimed spooled messages (D-116)"
            );
        }
        for claim in claims {
            let Ok(permit) = Arc::clone(&slots).acquire_owned().await else {
                break;
            };
            let engine = engine.clone();
            let spool = Arc::clone(&spool);
            tasks.spawn(async move {
                process(&engine, &spool, claim, lease).await;
                drop(permit);
            });
        }
    }
    tasks
}

/// The lease: the slowest route's whole downstream budget — the reservation's
/// own bound (§10.4's drain bound) — plus a margin for the body read and the
/// bookkeeping either side. Renewed at half of it.
pub fn lease_for(engine: &Engine) -> Duration {
    crate::smtp::relay_drain_bound(&engine.config) + Duration::from_secs(60)
}

/// §9.6 (D-127) — one root span per attempt: which message, how old, which
/// try, what it was pinned to or booked, and what became of it. The relay's
/// `simmer.relay` tree hangs under it, so a spooled delivery reads like a
/// synchronous one with its spool facts around it. `correlation_id` is the one
/// the client's `250 queued` transaction carried, which joins the attempt to
/// the accept's trace through the logs.
fn attempt_span(spool: &Spool, claim: &Claimed) -> Span {
    let now = Utc::now();
    tracing::info_span!(
        "simmer.spool.attempt",
        otel.name = "simmer.spool.attempt",
        otel.status_code = Empty,
        spool_id = %claim.id,
        correlation_id = Empty,
        ramp = %claim.ramp,
        domain_group = %claim.domain_group,
        attempt = claim.attempts + 1,
        age_seconds = (now - claim.received_at).num_seconds(),
        expires_in_seconds = (claim.expires_at - now).num_seconds(),
        pinned_route = claim.pinned_route.as_deref().unwrap_or(""),
        booked_route = claim.booked.as_ref().map_or("", |b| b.route.as_str()),
        body_store = spool.body.kind(),
        route = Empty,
        outcome = Empty,
        next_attempt_in_seconds = Empty,
        dead_reason = Empty,
        last_code = Empty,
        lease_lost = Empty,
    )
}

async fn process(engine: &Engine, spool: &Spool, claim: Claimed, lease: Duration) {
    let span = attempt_span(spool, &claim);
    process_inner(engine, spool, claim, lease)
        .instrument(span)
        .await;
}

async fn process_inner(engine: &Engine, spool: &Spool, claim: Claimed, lease: Duration) {
    let lost = Arc::new(AtomicBool::new(false));
    let renewer =
        {
            let store = Arc::clone(&spool.store);
            let lost = Arc::clone(&lost);
            let (id, token) = (claim.id, claim.lease_token);
            tokio::spawn(async move {
            let half = lease / 2;
            loop {
                tokio::time::sleep(half).await;
                let until = Utc::now() + chrono::Duration::from_std(lease).unwrap_or_default();
                match store.renew_lease(id, token, until).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(spool_id = %id, "spool lease lost mid-attempt (D-122)");
                        lost.store(true, Ordering::Relaxed);
                        break;
                    }
                    Err(e) => tracing::warn!(spool_id = %id, error = %e, "renewing a spool lease"),
                }
            }
        }
        .instrument(Span::current()))
        };
    attempt(engine, spool, &claim).await;
    renewer.abort();
    if lost.load(Ordering::Relaxed) {
        metrics::spool_lease_lost();
        Span::current().record("lease_lost", true);
    }
}

async fn attempt(engine: &Engine, spool: &Spool, claim: &Claimed) {
    let now = Utc::now();
    let id = claim.id;

    if now >= claim.expires_at {
        return dead(
            spool,
            claim,
            DeadReason::Expired,
            None,
            None,
            None,
            false,
            now,
        )
        .await;
    }

    let envelope = match Envelope::from_json(&claim.envelope) {
        Ok(e) => e,
        Err(e) => {
            let text = format!("unreadable envelope: {e}");
            return dead(
                spool,
                claim,
                DeadReason::Corrupt,
                None,
                None,
                Some(text),
                false,
                now,
            )
            .await;
        }
    };
    Span::current().record("correlation_id", envelope.correlation_id.as_str());
    let Some(ramp) = engine.config.ramps.get(&claim.ramp) else {
        // A configuration change since acceptance removed the ramp. Nothing
        // here can route it, and no later attempt could either.
        let text = format!("ramp '{}' is no longer configured", claim.ramp);
        return dead(
            spool,
            claim,
            DeadReason::Rejected,
            None,
            None,
            Some(text),
            false,
            now,
        )
        .await;
    };
    let Some(body_ref) = claim.body_ref.as_deref() else {
        let text = "the row names no body".to_string();
        return dead(
            spool,
            claim,
            DeadReason::Corrupt,
            None,
            None,
            Some(text),
            false,
            now,
        )
        .await;
    };
    let body = match spool.body.get(body_ref, &claim.body_sha256).await {
        Ok(b) => b,
        Err(e @ (BodyError::NotFound(_) | BodyError::Corrupt(_))) => {
            return dead(
                spool,
                claim,
                DeadReason::Corrupt,
                None,
                None,
                Some(e.to_string()),
                false,
                now,
            )
            .await;
        }
        Err(e) => {
            tracing::warn!(spool_id = %id, error = %e, "reading a spooled body; retrying");
            let at = cap(now + chrono::Duration::seconds(30), claim);
            requeue(
                spool,
                claim,
                at,
                false,
                None,
                None,
                None,
                Some(e.to_string()),
            )
            .await;
            return;
        }
    };

    let senders = Senders::new(
        envelope.mail_from.as_deref(),
        envelope.from_header.as_deref(),
    );
    let source = match envelope.ramp_source.as_str() {
        "affinity" => Source::Affinity,
        "header" => Source::Header,
        _ => Source::Default,
    };
    let selection = Selection {
        ramp,
        source,
        header: HeaderUse::Absent,
    };
    let message = envelope.into_message(body, claim.received_at);
    let mut peer = String::new();
    let ctx = AttemptCtx {
        walk_now: now,
        rewrite_now: claim.received_at,
        uuid_seed: claim.uuid_seed,
        spool: Some(SpoolCtx {
            hold_until: claim.expires_at,
            booked: claim.booked.as_ref(),
            pinned_route: claim.pinned_route.as_deref(),
            store: spool.store.as_ref(),
            id,
            token: claim.lease_token,
        }),
    };
    let outcome = relay::attempt(
        engine,
        &selection,
        &senders,
        message.as_message(&mut peer),
        &message.correlation_id,
        &ctx,
    )
    .await;

    let backoff_at = |attempts: i64| cap(now + backoff(&spool.cfg.retry, attempts), claim);
    match outcome.kind {
        AttemptKind::Relayed(r) if r.delivered => {
            metrics::spool_attempt(&claim.ramp, &r.route, "delivered");
            let span = Span::current();
            span.record("outcome", "delivered");
            span.record("route", r.route.as_str());
            if r.lease_held == Some(false) {
                span.record("lease_lost", true);
            }
            if r.lease_held == Some(false) {
                metrics::spool_lease_lost();
                tracing::warn!(
                    spool_id = %id,
                    "delivered after the lease was lost; another attempt may also deliver it \
                     (at least once, D-122)"
                );
            }
            if let Err(e) = spool.body.delete(body_ref).await {
                // The row is gone; the orphan sweeper will find the body.
                tracing::warn!(spool_id = %id, error = %e, "deleting a delivered body");
            }
        }
        AttemptKind::Relayed(r) => match r.failure {
            Some(Failure::PoolExhausted) => {
                metrics::spool_attempt(&claim.ramp, &r.route, "pool_exhausted");
                Span::current().record("outcome", "pool_exhausted");
                Span::current().record("route", r.route.as_str());
                let at = cap(now + chrono::Duration::seconds(1), claim);
                requeue(spool, claim, at, false, None, None, None, None).await;
            }
            Some(Failure::Rejected { code, text }) => {
                metrics::spool_attempt(&claim.ramp, &r.route, "rejected");
                dead(
                    spool,
                    claim,
                    DeadReason::Rejected,
                    Some(r.route),
                    Some(i64::from(code)),
                    Some(text),
                    true,
                    now,
                )
                .await;
            }
            failure => {
                let (code, text) = match failure {
                    Some(Failure::Transient { code, text }) => (code.map(i64::from), text),
                    Some(Failure::Ambiguous) => (None, "delivery unknown (§10.2)".to_string()),
                    _ => (None, String::new()),
                };
                metrics::spool_attempt(&claim.ramp, &r.route, "retry");
                let span = Span::current();
                span.record("outcome", "retry");
                span.record("route", r.route.as_str());
                if let Some(c) = code {
                    span.record("last_code", c);
                }
                let at = backoff_at(claim.attempts + 1);
                requeue(
                    spool,
                    claim,
                    at,
                    true,
                    Some(r.route),
                    None,
                    code,
                    Some(text),
                )
                .await;
            }
        },
        AttemptKind::Deferred {
            route,
            domain_group,
            booking,
            until,
        } => {
            metrics::spool_attempt(&claim.ramp, &route, "deferred");
            Span::current().record("outcome", "deferred");
            Span::current().record("route", route.as_str());
            let slot = BookedSlot {
                route,
                domain_group,
                tat: booking.booked_tat,
            };
            if !requeue(spool, claim, until, false, None, Some(slot), None, None).await {
                // Lost the lease: nobody will use this slot.
                crate::routing::chain::unbook(engine.quota.as_ref(), &booking, "lease lost").await;
            }
        }
        AttemptKind::NotSelected(
            e @ (SelectError::StrictSenderRejected { .. } | SelectError::MalformedFromHeader),
        ) => {
            // Acceptance passed this sender; a configuration change since has
            // made it unroutable for good.
            let text = format!("{e:?} after a configuration change");
            dead(
                spool,
                claim,
                DeadReason::Rejected,
                None,
                None,
                Some(text),
                false,
                now,
            )
            .await;
        }
        AttemptKind::NotSelected(e) => {
            metrics::spool_attempt(&claim.ramp, "-", "retry");
            Span::current().record("outcome", "no_route");
            let at = backoff_at(claim.attempts.max(1));
            requeue(
                spool,
                claim,
                at,
                false,
                None,
                None,
                None,
                Some(format!("{e:?}")),
            )
            .await;
        }
        AttemptKind::Internal => {
            metrics::spool_attempt(&claim.ramp, "-", "retry");
            Span::current().record("outcome", "internal");
            let at = backoff_at(claim.attempts.max(1));
            requeue(
                spool,
                claim,
                at,
                false,
                None,
                None,
                None,
                Some("internal".into()),
            )
            .await;
        }
    }
}

/// Never later than the hold: a message whose next try would fall past its
/// expiry is claimed at the expiry instead, and dead-lettered `expired` then.
fn cap(at: DateTime<Utc>, claim: &Claimed) -> DateTime<Utc> {
    at.min(claim.expires_at)
}

/// The retry schedule: `initial × factor^(n−1)`, capped at `max`, with
/// *equal* jitter — half the step fixed, half random — so a burst of failures
/// spreads out and no retry comes back at once (D-124).
pub fn backoff(retry: &SpoolRetry, attempts: i64) -> chrono::Duration {
    let n = i32::try_from(attempts.max(1) - 1).unwrap_or(i32::MAX);
    let step = retry.initial.as_secs_f64() * retry.factor.powi(n);
    let step = step.min(retry.max.as_secs_f64()).max(0.0);
    let jitter = (uuid::Uuid::new_v4().as_u128() % 1_000_000) as f64 / 1_000_000.0;
    let secs = step / 2.0 + jitter * step / 2.0;
    chrono::Duration::milliseconds((secs * 1000.0) as i64)
}

/// Back to `queued`, fenced. False if the lease was lost.
#[allow(clippy::too_many_arguments)]
async fn requeue(
    spool: &Spool,
    claim: &Claimed,
    at: DateTime<Utc>,
    attempted: bool,
    pinned_route: Option<String>,
    booked: Option<BookedSlot>,
    last_code: Option<i64>,
    last_error: Option<String>,
) -> bool {
    Span::current().record(
        "next_attempt_in_seconds",
        (at - Utc::now()).num_seconds().max(0),
    );
    match spool
        .store
        .reschedule(&Reschedule {
            id: claim.id,
            token: claim.lease_token,
            next_attempt_at: at,
            attempted,
            pinned_route,
            booked,
            last_code,
            last_error,
        })
        .await
    {
        Ok(true) => true,
        Ok(false) => {
            metrics::spool_lease_lost();
            tracing::warn!(spool_id = %claim.id, "rescheduling found the lease taken (D-122)");
            false
        }
        Err(e) => {
            // The lease expires and the next claim retries; nothing is lost.
            tracing::warn!(spool_id = %claim.id, error = %e, "rescheduling a spooled message");
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dead(
    spool: &Spool,
    claim: &Claimed,
    reason: DeadReason,
    route: Option<String>,
    code: Option<i64>,
    text: Option<String>,
    attempted: bool,
    now: DateTime<Utc>,
) {
    // D-118: a booked slot nobody will use goes back first.
    let keep_body = !spool.cfg.dead_letter.keep_body.is_zero();
    let request = DeadLetterRequest {
        id: claim.id,
        token: claim.lease_token,
        reason,
        at: now,
        attempted,
        pinned_route: route.clone(),
        last_code: code,
        last_error: text.clone(),
        keep_body,
    };
    match spool.store.dead_letter(&request).await {
        Ok(true) => {}
        Ok(false) => {
            metrics::spool_lease_lost();
            tracing::warn!(spool_id = %claim.id, "dead-lettering found the lease taken (D-122)");
            return;
        }
        Err(e) => {
            tracing::warn!(spool_id = %claim.id, error = %e, "dead-lettering a spooled message");
            return;
        }
    }
    metrics::spool_dead(&claim.ramp, reason.as_str());
    let span = Span::current();
    span.record("outcome", "dead");
    span.record("dead_reason", reason.as_str());
    span.record("otel.status_code", "ERROR");
    if let Some(c) = code {
        span.record("last_code", c);
    }
    if let Some(r) = &route {
        span.record("route", r.as_str());
    }
    if reason == DeadReason::Expired {
        metrics::spool_attempt(&claim.ramp, route.as_deref().unwrap_or("-"), "expired");
    }
    tracing::warn!(
        spool_id = %claim.id,
        ramp = %claim.ramp,
        domain_group = %claim.domain_group,
        route = route.as_deref().unwrap_or("-"),
        reason = reason.as_str(),
        code,
        "spooled message dead-lettered (D-120)"
    );
    if !keep_body {
        if let Some(body_ref) = &claim.body_ref {
            if let Err(e) = spool.body.delete(body_ref).await {
                tracing::warn!(spool_id = %claim.id, error = %e, "deleting a dead letter's body");
            }
        }
    }
    if let Some((client, cfg)) = &spool.webhook {
        let addresses = cfg
            .include_addresses
            .then(|| Envelope::from_json(&claim.envelope).ok())
            .flatten();
        webhook::notify(
            client.clone(),
            cfg.clone(),
            DeadLetterEvent {
                id: claim.id.to_string(),
                ramp: claim.ramp.clone(),
                domain_group: claim.domain_group.clone(),
                route: route.or_else(|| claim.pinned_route.clone()),
                reason: reason.as_str(),
                code,
                text,
                attempts: claim.attempts + i64::from(attempted),
                received_at: claim.received_at,
                dead_at: now,
                mail_from: addresses.as_ref().map(|e| e.mail_from.clone()),
                rcpt: addresses.map(|e| e.recipients),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_by_the_factor_and_stops_at_the_max() {
        let retry = SpoolRetry {
            initial: Duration::from_secs(60),
            max: Duration::from_secs(600),
            factor: 2.0,
        };
        for _ in 0..50 {
            let first = backoff(&retry, 1).num_milliseconds();
            assert!((30_000..=60_000).contains(&first), "{first}");
            let third = backoff(&retry, 3).num_milliseconds();
            assert!((120_000..=240_000).contains(&third), "{third}");
            let late = backoff(&retry, 40).num_milliseconds();
            assert!((300_000..=600_000).contains(&late), "{late}");
        }
    }
}
