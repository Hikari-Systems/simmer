//! §7.7's accept path (D-116, D-119): what a `delivery: spool` ramp does at
//! the final dot instead of relaying.
//!
//! Every synchronous check has already run by the time this is reached — the
//! ACL, size, §5.5's end-of-data rule and ramp selection in the session — and
//! the sender policy (§3.2 step 1, `strict_senders`, §5.4's malformed `From:`)
//! runs here first, with the replies a synchronous ramp gives. Only a message
//! that would have been *walked* is stored. Then admission, then the body (put
//! durably), then the row, then `250` (D-117's ordering).

use chrono::{DateTime, Utc};
use tracing::field::Empty;
use tracing::{Instrument as _, Span};
use uuid::Uuid;

use super::envelope::Envelope;
use super::store::NewSpooled;
use super::Spool;
use crate::config::{OnLimit, Ramp};
use crate::metrics;
use crate::quota;
use crate::relay::{self, Engine, OwnedMessage};
use crate::routing::ramp_select::Selection;
use crate::routing::sender_match::Senders;
use crate::smtp::reply::{self, Reply};

pub async fn accept(
    engine: &Engine,
    spool: &Spool,
    selection: &Selection<'_>,
    senders: &Senders,
    from_header: Option<&str>,
    message: OwnedMessage,
) -> Reply {
    // §9.6 (D-127) — the accept path as one span under `smtp.transaction`,
    // which is current here: the admission verdict and its inputs, where the
    // body went, and the id the client was given. The transaction span learns
    // the id too, so a trace search by it finds the accept.
    let tx = Span::current();
    tx.record("delivery", "spool");
    let span = tracing::info_span!(
        "simmer.spool.accept",
        otel.name = "simmer.spool.accept",
        otel.status_code = Empty,
        correlation_id = %message.correlation_id,
        ramp = %selection.ramp.name,
        ramp_source = selection.source.as_str(),
        bytes = message.body.len(),
        domain_group = Empty,
        group_basis = Empty,
        admission = Empty,
        expected_wait_seconds = Empty,
        hold_seconds = Empty,
        body_store = spool.body.kind(),
        spool_id = Empty,
        smtp.reply.code = Empty,
    );
    let reply = accept_inner(engine, spool, selection, senders, from_header, message, &tx)
        .instrument(span.clone())
        .await;
    span.record("smtp.reply.code", reply.code);
    if reply.code >= 400 {
        span.record("otel.status_code", "ERROR");
    }
    reply
}

async fn accept_inner(
    engine: &Engine,
    spool: &Spool,
    selection: &Selection<'_>,
    senders: &Senders,
    from_header: Option<&str>,
    message: OwnedMessage,
    tx: &Span,
) -> Reply {
    let ramp = selection.ramp;
    let now = message.received_at;
    let cid = message.correlation_id.as_str();

    // §3.2 step 1 and §5.4, exactly as the synchronous walk applies them.
    let chain = match relay::resolve_chain(ramp, senders) {
        Ok(c) => c,
        Err(e) => {
            tracing::info!(correlation_id = cid, ramp = %ramp.name, reason = ?e, "no route selected");
            return e.to_reply(ramp);
        }
    };

    let refuse = |reason: &'static str, r: Reply| {
        metrics::spool_admission_refused(&ramp.name, reason);
        tracing::info!(
            correlation_id = cid,
            ramp = %ramp.name,
            reason,
            "spool admission refused (D-119)"
        );
        r
    };

    let recipient = message
        .envelope
        .recipients
        .first()
        .map(String::as_str)
        .unwrap_or_default();
    let resolved = engine.groups.resolve(ramp, recipient).await;
    let (group, basis) = match &resolved {
        Some(r) => (r.group.name.clone(), r.basis.describe()),
        None => ("catchall".to_string(), "fallback".to_string()),
    };
    let bytes = i64::try_from(message.body.len()).unwrap_or(i64::MAX);

    let span = Span::current();
    span.record("domain_group", group.as_str());
    span.record("group_basis", basis.as_str());
    let verdict = match admission(engine, spool, ramp, chain, &group, bytes, now)
        .instrument(tracing::info_span!(
            "simmer.spool.admission",
            otel.name = "simmer.spool.admission",
            correlation_id = cid,
        ))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(correlation_id = cid, error = %e, "spool admission state unavailable");
            return refuse("unavailable", reply::spool_unavailable());
        }
    };
    let expires_at = verdict.hold_until;
    span.record("admission", verdict.outcome.as_str());
    span.record("hold_seconds", (expires_at - now).num_seconds());
    if let Some(w) = verdict.expected_wait {
        span.record("expected_wait_seconds", w.num_seconds());
    }
    match verdict.outcome {
        Admit::Admit => {}
        Admit::Draining => return refuse("draining", reply::spool_draining()),
        Admit::Full => return refuse("full", reply::spool_full()),
        Admit::Backlog => return refuse("backlog", reply::spool_backlog()),
    }

    let id = Uuid::new_v4();
    let envelope = Envelope::of(&message, from_header, selection.source.as_str());
    let stored = match spool.body.put(id, &message.body).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(correlation_id = cid, error = %e, "storing a spooled body failed");
            return refuse("unavailable", reply::spool_unavailable());
        }
    };
    let row = NewSpooled {
        id,
        ramp: ramp.name.clone(),
        domain_group: group.clone(),
        group_basis: basis,
        received_at: now,
        expires_at,
        next_attempt_at: now,
        envelope: envelope.to_json(),
        body_ref: stored.body_ref.clone(),
        body_bytes: stored.bytes,
        body_sha256: stored.sha256,
        uuid_seed: Uuid::new_v4(),
    };
    let enqueued = spool
        .store
        .enqueue(&row)
        .instrument(tracing::info_span!(
            "simmer.spool.enqueue",
            otel.name = "simmer.spool.enqueue",
            correlation_id = cid,
            spool_id = %id,
        ))
        .await;
    if let Err(e) = enqueued {
        tracing::error!(correlation_id = cid, error = %e, "recording a spooled message failed");
        // Best effort: the orphan sweeper is the backstop (D-117).
        let _ = spool.body.delete(&stored.body_ref).await;
        return refuse("unavailable", reply::spool_unavailable());
    }

    metrics::spool_accepted(&ramp.name);
    span.record("spool_id", id.to_string());
    tx.record("spool_id", id.to_string());
    tracing::info!(
        correlation_id = cid,
        ramp = %ramp.name,
        ramp_source = selection.source.as_str(),
        domain_group = %group,
        spool_id = %id,
        expires_at = %expires_at.to_rfc3339(),
        bytes,
        "spooled"
    );
    reply::queued(id)
}

/// D-119's verdict on one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    Admit,
    Draining,
    Full,
    Backlog,
}

impl Admit {
    pub fn as_str(self) -> &'static str {
        match self {
            Admit::Admit => "admit",
            Admit::Draining => "draining",
            Admit::Full => "full",
            Admit::Backlog => "backlog",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Admission {
    pub outcome: Admit,
    pub hold_until: DateTime<Utc>,
    /// D-119's forecast: how long the lane's queue would take to drain ahead
    /// of this message. `None` when no route in front would queue it.
    pub expected_wait: Option<chrono::Duration>,
}

/// D-119 — the one admission decision, for the accept path and §9.4's dry run
/// alike, so the two cannot disagree. Read-only.
pub async fn admission(
    engine: &Engine,
    spool: &Spool,
    ramp: &Ramp,
    chain: &[String],
    group: &str,
    bytes: i64,
    now: DateTime<Utc>,
) -> Result<Admission, quota::QuotaError> {
    let hold_until = hold_until(spool, ramp, chain, now);
    let expected_wait = forecast(engine, spool, ramp, chain, group, now).await?;
    let mut verdict = Admission {
        outcome: Admit::Admit,
        hold_until,
        expected_wait,
    };
    let states = spool.store.spool_states().await?;
    if states.get(&ramp.name).is_some_and(|s| s.draining) {
        verdict.outcome = Admit::Draining;
        return Ok(verdict);
    }
    let t = spool.store.totals().await?;
    let max_messages = i64::try_from(spool.cfg.max_messages).unwrap_or(i64::MAX);
    let max_bytes = i64::try_from(spool.cfg.max_bytes).unwrap_or(i64::MAX);
    if t.messages >= max_messages || t.bytes.saturating_add(bytes) > max_bytes {
        verdict.outcome = Admit::Full;
        return Ok(verdict);
    }
    if expected_wait.is_some_and(|w| now + w > hold_until) {
        verdict.outcome = Admit::Backlog;
    }
    Ok(verdict)
}

/// Q4 — `received_at + max_hold`, and never past the next day boundary of the
/// first route in the chain unless `cross_day_boundary`: yesterday's backlog
/// must not spend tomorrow's cap.
pub fn hold_until(
    spool: &Spool,
    ramp: &Ramp,
    chain: &[String],
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    let hold = chrono::Duration::from_std(spool.cfg.max_hold)
        .unwrap_or_else(|_| chrono::Duration::hours(6));
    let by_hold = now + hold;
    if spool.cfg.cross_day_boundary {
        return by_hold;
    }
    match chain.first().and_then(|name| ramp.route(name)) {
        Some(route) => by_hold.min(quota::day::next_boundary(route, now)),
        None => by_hold,
    }
}

/// D-119 — the lane's queue divided by the hourly rate of the chain's first
/// route that would *queue* it: a route with `on_limit: wait`. A route before
/// it that does not wait — unrated, or rated but steering — takes what the
/// waiting route cannot, so nothing queues and the forecast is `None` (zero).
pub async fn forecast(
    engine: &Engine,
    spool: &Spool,
    ramp: &Ramp,
    chain: &[String],
    group: &str,
    now: DateTime<Utc>,
) -> Result<Option<chrono::Duration>, quota::QuotaError> {
    let Some(route) = chain
        .first()
        .and_then(|name| ramp.route(name))
        .filter(|r| r.rate.as_ref().is_some_and(|l| l.on_limit == OnLimit::Wait))
    else {
        return Ok(None);
    };
    let states = engine.quota.route_states(&ramp.name).await?;
    let state = states.get(&route.name).copied().unwrap_or_default();
    let day = quota::day::for_route(route, now);
    let Some(rate) = quota::rate::rate_for(route, group, day, state) else {
        return Ok(None);
    };
    let per_hour = rate.per_hour;
    if per_hour <= 0 {
        return Ok(None);
    }
    let depth = spool.store.lane_depth(&ramp.name, group).await?;
    // The message being admitted waits behind every message already queued.
    let ahead = depth + 1;
    let millis = ahead.saturating_mul(3_600_000) / per_hour;
    Ok(Some(chrono::Duration::milliseconds(millis)))
}
