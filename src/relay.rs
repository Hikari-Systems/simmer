//! Where a buffered message becomes a downstream conversation.
//!
//! The three steps O-1 asked to be kept apart:
//!
//! 1. **Decide** — resolve the incoming identity and match a sender rule (§3.2
//!    steps 1–2).
//! 2. **Reserve** — §7.4 phase 1, walking the chain (§3.2 step 3) in the order
//!    §3.2 step 2a's thread affinity gives it (D-090). This happens
//!    **immediately before** the downstream conversation, never before `DATA`, so
//!    a reservation cannot outlive §7.4's expiry while a body transfers.
//! 3. **Rewrite** — §6.1 steps 2 and 4–10, now that the route, and therefore the
//!    identity, is known.
//! 4. **Relay, then commit or release** — §7.4 phases 2 and 3, with the §10.1
//!    mapping deciding which.
//!
//! D-111 puts one more thing between 2 and 3: a route with a `rate` booked a
//! slot during the walk, and when that slot is in the future (at most the
//! route's `max_wait` away) the relay **waits for it, holding the
//! reservation**, before rewriting. Every outcome that does not commit gives
//! the slot back.
//!
//! §3.3 governs the seam between 2 and 4: **no failover**. A route that reserved
//! and then failed downstream releases and reports. It does not fall through to
//! the next link, because that would emit under the wrong identity and corrupt
//! both the ramp accounting and the reputation being built.
//!
//! Rewriting sits *inside* the reservation rather than before it because §6.1
//! step 3 — "resolve incoming identity; select route" — is what tells it which
//! identity to apply, and selection is what takes the reservation. The cost is
//! that a rewrite happens while a reservation is held; it is CPU-bound work over
//! bytes already in memory, and the alternative is not knowing what to write.

use std::sync::Arc;

use tracing::field::Empty;
use tracing::{Instrument, Span};

use crate::config::{Config, Ramp};
use crate::downstream::{self, TlsConfigs};
use crate::frequency::Frequency;
use crate::metrics;
use crate::quota::{self, QuotaStore, ReservationRegistry};
use crate::rewrite::{self, Rewriters};
use crate::routing::chain::{self, Walk};
use crate::routing::ramp_select;
use crate::routing::sender_match::{self, Senders};
use crate::routing::thread;
use crate::smtp::reply::{self, Reply};

/// Everything a session needs to relay, assembled once at startup.
#[derive(Clone)]
pub struct Engine {
    pub config: Arc<Config>,
    pub tls: Arc<TlsConfigs>,
    /// §8.3 — one pool per route, shared by every session. Shared is the whole
    /// point: a pool per session would bound nothing, and §8.3's last sentence
    /// asks the pool to bound concurrency against each downstream.
    pub pools: Arc<downstream::Pool>,
    /// §11 — the storage layer behind its trait.
    pub quota: Arc<dyn QuotaStore>,
    /// §10.4 — reservations this process is holding.
    pub registry: ReservationRegistry,
    /// §6 — every route's identity, with its templates compiled. Built once at
    /// startup because a template parse failure is a configuration error
    /// (D-034), and configuration errors belong at startup rather than on a
    /// connection that has already been accepted.
    pub rewriters: Arc<Rewriters>,
    /// §7.3 — the instance's recipient-hash salt, resolved from storage on first
    /// use. Shared, because "generated once and persisted" is a property of the
    /// instance rather than of a message.
    pub frequency: Arc<Frequency>,
    /// §6.7 — the last preflight pass's verdict per route, refreshed on a timer.
    /// Shared and read-mostly: the walk consults it per message and only a
    /// `strict` route can be eliminated by it.
    pub preflight: Arc<crate::preflight::Registry>,
    /// §3.2 step 2 with D-100's MX step, and its cache. Shared so the early
    /// check, the walk and the dry run agree about a domain.
    pub groups: Arc<crate::routing::domain_group::Grouper>,
    /// D-085 — the optional debugging capture. `None` is the whole of "off":
    /// no directory, no task, no metric, and an `Option<Capture>` one word wide,
    /// so cloning the engine per session costs nothing when it is absent.
    ///
    /// **Write-only.** `Capture` exposes no read method, so nothing downstream
    /// of this field can consult the capture to decide what to deliver. That
    /// absence is what keeps §2.2 true — see `src/capture/mod.rs`.
    pub capture: Option<crate::capture::Capture>,
}

/// Why no route could be selected. Each maps to a specific reply, and the
/// mapping is §14.1-sensitive, so it is spelled out rather than inferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// §3.2 step 1 with `strict_senders: true`.
    StrictSenderRejected { domain: String },
    /// §5.4 — `From:` absent or unparseable when `match_on` requires it.
    MalformedFromHeader,
    /// §3.2 step 4 / §10.3.
    ChainExhausted,
    /// §7.5 — the quota store is unreachable and `fail_closed` is set. "A quota
    /// enforcer that stops enforcing under failure provides no guarantee at
    /// all."
    QuotaUnavailable,
}

impl SelectError {
    pub fn to_reply(&self, ramp: &Ramp) -> Reply {
        match self {
            SelectError::StrictSenderRejected { .. } => reply::sender_not_configured(),
            SelectError::MalformedFromHeader => reply::malformed_from_header(),
            SelectError::ChainExhausted => reply::no_eligible_route(matches!(
                ramp.exhausted_chain_reply,
                crate::config::ExhaustedChainReply::Permanent
            )),
            // §7.5's own wording. Temporary, necessarily: the recipient is fine,
            // Simmer is not.
            SelectError::QuotaUnavailable => Reply::new(451, "4.3.0 quota service unavailable"),
        }
    }
}

/// §3.2 steps 1–2: match a sender rule and produce the chain to walk.
///
/// Pure and synchronous. Splitting it out from the chain walk is what lets the
/// §5.4 early check and the final-dot path share exactly one implementation of
/// the sender policy.
pub fn resolve_chain<'a>(ramp: &'a Ramp, senders: &Senders) -> Result<&'a [String], SelectError> {
    // §5.4: a rule that tests the From: header needs one to exist.
    //
    // The check is "does *any* rule need it", not "does the rule that would have
    // matched need it" — a rule cannot be known to match until the header it
    // tests has been parsed, so the narrower reading is unimplementable
    // (D-028).
    let needs_from_header = ramp
        .senders
        .iter()
        .any(|r| !matches!(r.match_on, crate::config::MatchOn::Envelope));
    if needs_from_header && senders.from_header.is_none() {
        return Err(SelectError::MalformedFromHeader);
    }

    if senders.disagree() {
        // §5.4: log both values and count it. Often the first sign that an
        // application is half-migrated.
        tracing::warn!(
            envelope = senders.envelope.as_deref().unwrap_or("<>"),
            from_header = senders.from_header.as_deref().unwrap_or(""),
            "envelope and header senders disagree"
        );
        metrics::sender_mismatch();
    }

    match sender_match::match_sender(ramp, senders) {
        sender_match::Match::Rule { rule, .. } => Ok(&rule.chain),
        sender_match::Match::Unmatched => {
            let domain = senders
                .from_header
                .as_deref()
                .or(senders.envelope.as_deref())
                .and_then(|a| a.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase()))
                .unwrap_or_default();

            if ramp.strict_senders {
                return Err(SelectError::StrictSenderRejected { domain });
            }

            // §14.2: "a typo in a sender rule sends unwarmed traffic at full
            // volume via the established identity. The WARN and counter must
            // actually be alerted on."
            tracing::warn!(
                domain = %domain,
                "sender matched no rule; falling back to default_chain"
            );
            metrics::unmatched_sender(&ramp.name, &domain);

            // O-6: walk it normally, like any other chain.
            Ok(ramp
                .default_chain
                .as_deref()
                // §4.2 guarantees this exists whenever strict_senders is false.
                .unwrap_or(&[]))
        }
    }
}

/// The §5.4 early check, run at `RCPT TO` when every rule is envelope-only.
///
/// Takes no reservation (O-1). Returns the sender-policy verdict immediately and
/// only then asks the store whether anything in the chain has headroom.
pub async fn check_early(
    engine: &Engine,
    ramp: &Ramp,
    senders: &Senders,
    recipient: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), SelectError> {
    let chain = resolve_chain(ramp, senders)?;

    match chain::any_eligible(ramp, &engine.groups, &engine.quota, chain, recipient, now).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(SelectError::ChainExhausted),
        Err(e) => Err(quota_failure(
            &engine.config,
            ramp,
            e,
            "early eligibility check",
        )),
    }
}

/// §7.5 — turn a storage failure into the configured posture.
fn quota_failure(cfg: &Config, ramp: &Ramp, e: quota::QuotaError, during: &str) -> SelectError {
    if cfg.database.fail_closed {
        tracing::error!(error = %e, during, "quota store unavailable; failing closed");
        metrics::quota_unavailable(&ramp.name, "-");
        SelectError::QuotaUnavailable
    } else {
        // Explicitly opted out of §7.5's default. What that opt-out *does* is
        // not specified: §7.5 defines only the `true` case. The implementation
        // has always treated the outage as an exhausted chain, so the client
        // gets the ramp's `exhausted_chain_reply` (`451 4.7.1` by default)
        // rather than §7.5's `451 4.3.0` — nothing is sent either way, and the
        // ramp is not bypassed. The log used to claim the message proceeded
        // "WITHOUT quota enforcement", which was never true. Raised as O-19;
        // until it is answered the behaviour is kept and only described
        // truthfully (D-107).
        tracing::error!(
            error = %e,
            during,
            "quota store unavailable and fail_closed is false; refusing as an exhausted \
             chain (exhausted_chain_reply, 451 4.7.1 by default) rather than \
             451 4.3.0. Nothing is sent; see O-19"
        );
        SelectError::ChainExhausted
    }
}

/// The whole of step 2 and step 3: reserve, relay, then commit or release.
///
/// `now` is the session's one clock read for this message (D-108).
///
/// Returns the reply for the client. Every path through this function resolves
/// the reservation exactly once — that is the invariant §7.4 rests on, and it is
/// why commit and release are not exposed separately to the session.
pub async fn reserve_relay_commit(
    engine: &Engine,
    selection: &ramp_select::Selection<'_>,
    senders: &Senders,
    message: Message<'_>,
    correlation_id: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Reply {
    // §9.6 (D-126) — the decision and its consequences as one span, with the
    // walk, the rewrite, the downstream conversation and the quota resolution
    // as its children. Every log line below lands inside it, which is what
    // gives the downstream outcome lines the `correlation_id` they never had.
    let span = tracing::info_span!(
        "simmer.relay",
        otel.name = "simmer.relay",
        otel.status_code = Empty,
        correlation_id,
        ramp = %selection.ramp.name,
        ramp_source = selection.source.as_str(),
        route = Empty,
        domain_group = Empty,
        day_index = Empty,
        thread_pin = Empty,
        over_cap = Empty,
        result = Empty,
        smtp.reply.code = Empty,
    );
    let reply =
        reserve_relay_commit_inner(engine, selection, senders, message, correlation_id, now)
            .instrument(span.clone())
            .await;
    span.record("smtp.reply.code", reply.code);
    if reply.code >= 400 {
        span.record("otel.status_code", "ERROR");
    }
    reply
}

async fn reserve_relay_commit_inner(
    engine: &Engine,
    selection: &ramp_select::Selection<'_>,
    senders: &Senders,
    message: Message<'_>,
    correlation_id: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Reply {
    let cfg = &engine.config;
    // §5.8 (D-099): chosen by the session before this runs. Everything below
    // is within this ramp.
    let ramp = selection.ramp;

    let chain = match resolve_chain(ramp, senders) {
        Ok(c) => c,
        Err(e) => {
            tracing::info!(
                correlation_id,
                ramp = %ramp.name,
                ramp_source = selection.source.as_str(),
                reason = ?e,
                "no route selected"
            );
            return e.to_reply(ramp);
        }
    };

    // -- §3.2 step 2a (D-090) -----------------------------------------
    //
    // A reply into a thread Simmer started walks the route that started it
    // first, past its §7.3 threshold and, when it has none left, past its
    // day's cap — counted, under the row lock, but not refused. Pause, strict
    // preflight and a future start still apply: they say the route cannot
    // send, not that it has sent enough.
    let pin = thread::pin_for_message(ramp, chain, message.body);
    let walk_order = thread::order(chain, &pin);
    Span::current().record("thread_pin", pin.as_log());

    // -- §7.4 phase 1 -------------------------------------------------
    let mut evaluation = Vec::new();
    // §3.2 step 3 as its own span: the walk is where a message is steered, and
    // the rendered chain — every link with its skip reason — is the one
    // attribute that answers "why did it go there".
    let walk_span = tracing::info_span!(
        "simmer.route",
        otel.name = "simmer.route",
        // On every child too: stdout carries only the innermost span's
        // fields, and §9.5 wants the id on every line (D-126).
        correlation_id,
        chain = Empty,
        route = Empty,
        domain_group = Empty,
        day_index = Empty,
        over_cap = Empty,
    );
    let walk = chain::walk_and_reserve(
        ramp,
        &engine.groups,
        &cfg.dot_insensitive_domains,
        &engine.quota,
        &engine.frequency,
        &engine.preflight,
        &walk_order,
        pin.route(),
        message.recipients,
        correlation_id,
        &mut evaluation,
        now,
    )
    .instrument(walk_span.clone())
    .await;
    walk_span.record("chain", chain::render(&evaluation).to_string());
    let selected = match walk {
        Ok(Walk::Selected(s)) => {
            thread::observe(&ramp.name, &pin, Some(&s.route.name), s.over_cap);
            for span in [&walk_span, &Span::current()] {
                span.record("route", s.route.name.as_str());
                span.record("domain_group", s.domain_group.as_str());
                span.record("day_index", s.day_index);
                span.record("over_cap", s.over_cap);
            }
            s
        }
        Ok(Walk::Exhausted) => {
            thread::observe(&ramp.name, &pin, None, false);
            // §10.3, and the whole of §14.1: `451` by default, because a `550`
            // here would permanently suppress a deliverable recipient in systems
            // that outlive Simmer by years.
            tracing::info!(
                correlation_id,
                chain = %chain::render(&evaluation),
                thread_pin = pin.as_log(),
                "no eligible route in chain"
            );
            return SelectError::ChainExhausted.to_reply(ramp);
        }
        Err(e) => {
            let err = quota_failure(cfg, ramp, e, "reservation");
            return err.to_reply(ramp);
        }
    };

    engine.registry.insert(&selected.reservation);

    // -- D-111: wait for the booked rate slot -------------------------
    //
    // After the reservation, not before it (D-112): the walk has already
    // decided, under both row locks, that this route takes this message, and
    // holding the headroom through the wait is what stops a wait from ending
    // in "no headroom after all" — which would waste the slot and could not
    // always give it back. The reservation's expiry and §10.4's drain bound
    // both include `max_wait`. Tokio's clock, so a test can pause it.
    if let Some(b) = &selected.rate {
        let wait = (b.send_at - now).to_std().unwrap_or_default();
        metrics::rate_wait(
            &selected.route.ramp,
            &selected.route.name,
            &selected.domain_group,
            wait.as_secs_f64(),
        );
        if !wait.is_zero() {
            tracing::debug!(
                correlation_id,
                route = %selected.route.name,
                domain_group = %selected.domain_group,
                wait_ms = wait.as_millis() as u64,
                "holding the client for the route's next rate slot (D-111)"
            );
            tokio::time::sleep(wait).await;
        }
    }

    // -- §6.1 steps 2 and 4–10 -----------------------------------------
    //
    // The identity is known only now, because it belongs to the route the walk
    // just picked. Everything before this point has treated the message as
    // opaque bytes.
    let Some(rewriter) = engine
        .rewriters
        .get(&selected.route.ramp, &selected.route.name)
    else {
        // Unreachable: `Rewriters` is compiled from the same `cfg.routes` the
        // walk selected from. Answered rather than panicked because the message
        // has already been accepted from the client, and §14.1 makes the answer
        // temporary.
        tracing::error!(
            correlation_id,
            route = %selected.route.name,
            "no compiled rewrite for the selected route"
        );
        if let Some(b) = &selected.rate {
            chain::unbook(engine.quota.as_ref(), b, "internal configuration error").await;
        }
        let _ = engine.quota.release(&selected.reservation).await;
        engine.registry.remove(selected.reservation.id);
        return Reply::new(451, "4.3.0 internal configuration error");
    };

    let rewrite_span = tracing::info_span!(
        "simmer.rewrite",
        otel.name = "simmer.rewrite",
        correlation_id,
        route = %selected.route.name,
        skipped_parts = Empty,
        skipped_headers = Empty,
    );
    let rewritten = rewrite_span.in_scope(|| {
        rewrite::rewrite(
            rewriter,
            &rewrite::Inbound {
                raw: message.body,
                envelope_from: message.mail_from,
                recipients: message.recipients,
                route_name: &selected.route.name,
                correlation_id,
                received: rewrite::Received {
                    helo: message.helo,
                    peer: message.peer,
                    by: &cfg.server.hostname,
                    authenticated: message.authenticated,
                    tls: message.tls,
                },
                // D-108 — the instant the walk was evaluated at, so the Date the
                // rewrite writes and the day the quota was charged to agree.
                now,
                uuid: &|| uuid::Uuid::new_v4().to_string(),
            },
        )
    });
    rewrite_span.record("skipped_parts", rewritten.skipped_parts.len());
    rewrite_span.record("skipped_headers", rewritten.skipped_headers.len());
    drop(rewrite_span);

    // §6.4 — a `text/*` part the route's `body_rewrites` did not reach. The
    // engine has already logged each one; this is the counter §9.1 asks for.
    for reason in &rewritten.skipped_parts {
        metrics::body_rewrite_skipped(&selected.route.ramp, &selected.route.name, reason.as_str());
    }
    // D-089 — the same, for a header a `header_rewrites` entry named.
    for (header, reason) in &rewritten.skipped_headers {
        metrics::header_rewrite_skipped(
            &selected.route.ramp,
            &selected.route.name,
            header,
            reason.as_str(),
        );
    }

    tracing::info!(
        correlation_id,
        // §9.5: the ramp and which §5.8 rule chose it.
        ramp = %ramp.name,
        ramp_source = selection.source.as_str(),
        route = %selected.route.name,
        domain_group = %selected.domain_group,
        day_index = selected.day_index,
        reservation = %selected.reservation.id,
        chain = %chain::render(&evaluation),
        thread_pin = pin.as_log(),
        // §9.5 forbids logging bodies; the two envelope senders are the whole
        // point of the component and are exactly what an operator needs when
        // asking "which identity did this leave under".
        envelope_from = message.mail_from.unwrap_or("<>"),
        outbound_envelope_from = rewritten.envelope_from.as_deref().unwrap_or("<>"),
        recipients = message.recipients.len(),
        bytes = rewritten.raw.len(),
        "relaying"
    );

    // -- §7.4 phase 2 -------------------------------------------------
    // The outbound conversation. A client span to the downstream's address;
    // its reply code and outcome class, never its reply text, which can quote
    // the recipient back (§9.5). The D-068 retry is an event inside it.
    let downstream_span = tracing::info_span!(
        "smtp.downstream",
        otel.name = "smtp.downstream",
        correlation_id,
        otel.kind = "client",
        otel.status_code = Empty,
        route = %selected.route.name,
        server.address = %selected.route.downstream.host,
        server.port = selected.route.downstream.port,
        smtp.response.code = Empty,
        smtp.stage = Empty,
        outcome = Empty,
        retried = Empty,
    );
    let started = std::time::Instant::now();
    let result = downstream::relay(
        selected.route,
        &engine.tls,
        &engine.pools,
        &cfg.server.hostname,
        &downstream::Message {
            mail_from: rewritten.envelope_from.as_deref(),
            recipients: message.recipients,
            body: &rewritten.raw,
            smtputf8: message.smtputf8,
            body_8bitmime: message.body_8bitmime,
            assume_8bitmime: selected.route.downstream.assume_8bitmime,
        },
    )
    .instrument(downstream_span.clone())
    .await;
    let elapsed = started.elapsed();
    metrics::downstream_latency(
        &selected.route.ramp,
        &selected.route.name,
        elapsed.as_secs_f64(),
    );

    let outcome = downstream_span.in_scope(|| match &result {
        Ok(d) => {
            let span = Span::current();
            span.record("smtp.response.code", d.code);
            span.record("outcome", "delivered");
            tracing::info!(
                correlation_id,
                route = %selected.route.name,
                code = d.code,
                latency_ms = elapsed.as_millis() as u64,
                "downstream accepted the message"
            );
            downstream::outcome::delivered(&selected.route.name, d)
        }
        Err(e) => {
            let span = Span::current();
            span.record("otel.status_code", "ERROR");
            span.record("outcome", e.class());
            if let Some(code) = e.code() {
                span.record("smtp.response.code", code);
            }
            if let Some(stage) = e.stage() {
                span.record("smtp.stage", stage.as_str());
            }
            downstream::outcome::failed(&selected.route.ramp, &selected.route.name, e)
        }
    });
    drop(downstream_span);
    Span::current().record("result", outcome.result.as_str());

    // -- §7.4 phase 3 -------------------------------------------------
    //
    // "On downstream 2xx, move the count from reserved to committed... On any
    // failure, decrement reserved and delete the reservation." `outcome.commit`
    // is the §10.1 table's answer, carried since phase 2.
    let store = Arc::clone(&engine.quota);
    // D-114: a message that was not sent gives its rate slot back — with one
    // exception, §10.2's ambiguous final dot. The downstream may already have
    // the message, so the slot stays spent: over-counting a send only paces the
    // next message more conservatively, and under-counting one is the overshoot
    // the limit exists to prevent.
    if let Some(b) = &selected.rate {
        let maybe_sent = matches!(result, Err(downstream::RelayError::Ambiguous));
        if !outcome.commit && !maybe_sent {
            chain::unbook(store.as_ref(), b, "downstream failure").await;
        }
    }
    let resolve_span = tracing::info_span!(
        "simmer.quota.resolve",
        otel.name = "simmer.quota.resolve",
        correlation_id,
        otel.status_code = Empty,
        committed = outcome.commit,
        reservation = %selected.reservation.id,
    );
    let resolution = async {
        if outcome.commit {
            // §7.4 phase 3, both halves in one transaction: the count moves from
            // `reserved` to `committed` and this message's recipient-frequency
            // events are recorded. `recipient_keys` is empty unless the selected
            // route declares a constraint.
            store
                .commit(&selected.reservation, &selected.recipient_keys)
                .await
        } else {
            store.release(&selected.reservation).await
        }
    }
    .instrument(resolve_span.clone())
    .await;
    engine.registry.remove(selected.reservation.id);
    if resolution.is_err() {
        resolve_span.record("otel.status_code", "ERROR");
    }
    drop(resolve_span);

    if let Err(e) = resolution {
        // The message's fate is already decided and already correct; only the
        // accounting failed. Log loudly — a committed send that did not increment
        // the counter means the ramp is under-counting, and the sweeper will
        // eventually release the reservation as if the send had failed.
        tracing::error!(
            correlation_id,
            route = %selected.route.name,
            reservation = %selected.reservation.id,
            committed = outcome.commit,
            error = %e,
            "failed to resolve the quota reservation; ramp accounting may be short"
        );
    } else if outcome.commit {
        // §9.1 gauges, from the row we just moved. A read of the row other
        // instances are locking, on the client's time: its own span, so a trace
        // shows it rather than a gap after `simmer.quota.resolve` — §18's
        // slowest message spent 430 ms here (D-126).
        if let Ok(usage) = store
            .usage(
                &selected.reservation.ramp,
                &selected.reservation.route,
                &selected.reservation.domain_group,
                selected.reservation.day_index,
            )
            .instrument(tracing::info_span!(
                "simmer.quota.usage",
                otel.name = "simmer.quota.usage",
                correlation_id,
            ))
            .await
        {
            metrics::quota_committed(
                &selected.route.ramp,
                &selected.route.name,
                &selected.domain_group,
                usage.committed as f64,
            );
            metrics::quota_reserved(
                &selected.route.ramp,
                &selected.route.name,
                &selected.domain_group,
                usage.reserved as f64,
            );
        }
    }

    metrics::message(
        &selected.route.ramp,
        &selected.route.name,
        &selected.domain_group,
        outcome.result,
    );
    outcome.reply
}

/// What to relay. Distinct from [`downstream::Message`] so the session does not
/// have to know the outbound leg's types.
pub struct Message<'a> {
    pub mail_from: Option<&'a str>,
    pub recipients: &'a [String],
    pub body: &'a [u8],
    pub smtputf8: bool,
    pub body_8bitmime: bool,
    /// §6.1 step 8 — what the client said in `EHLO`/`HELO`.
    pub helo: &'a str,
    /// §6.1 step 8 — the client's address, as the listener saw it.
    pub peer: &'a str,
    /// §6.1 step 8 — RFC 3848's `ESMTPA` versus `ESMTP`.
    pub authenticated: bool,
    /// §6.1 step 8 — RFC 3848's `S`, for a session that was encrypted (D-070).
    pub tls: bool,
}

/// A message as the session hands it over at the final dot, owning everything it
/// needs (D-110).
///
/// [`Message`] borrows from the session, so it cannot outlive the session's
/// stack frame. This is the same data with nothing borrowed — the shape a future
/// hand-off across a task boundary needs. Today it is built at the final dot and
/// relayed at once through [`reserve_relay_commit_owned`]; it is **not** stored
/// anywhere, and nothing reads one back (CLAUDE.md's first rule).
#[derive(Debug, Clone)]
pub struct OwnedMessage {
    pub envelope: OwnedEnvelope,
    /// The `DATA` payload, unstuffed and CRLF-normalised, as received.
    pub body: Vec<u8>,
    /// §6.1 step 8 — what the client said in `EHLO`/`HELO`.
    pub helo: String,
    /// §6.1 step 8 — the client's address, as the listener saw it.
    pub peer: std::net::IpAddr,
    /// The authenticated username, if any (§5.3). RFC 3848's `ESMTPA` when set.
    pub auth: Option<String>,
    /// §6.1 step 8 — RFC 3848's `S`, for a session that was encrypted (D-070).
    pub tls: bool,
    /// The final dot: the instant the walk and the rewrite use (D-108).
    pub received_at: chrono::DateTime<chrono::Utc>,
    /// §9.5's id for this message.
    pub correlation_id: String,
}

/// The envelope half of an [`OwnedMessage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedEnvelope {
    pub mail_from: Option<String>,
    /// Exactly one in every message that arrives over SMTP (D-047).
    pub recipients: Vec<String>,
    pub smtputf8: bool,
    pub body_8bitmime: bool,
}

impl OwnedMessage {
    /// The borrowed view the relay path takes. `peer` is rendered as the
    /// session always rendered it, into `peer_buf`, so the view can borrow it.
    pub fn as_message<'a>(&'a self, peer_buf: &'a mut String) -> Message<'a> {
        *peer_buf = self.peer.to_string();
        Message {
            mail_from: self.envelope.mail_from.as_deref(),
            recipients: &self.envelope.recipients,
            body: &self.body,
            smtputf8: self.envelope.smtputf8,
            body_8bitmime: self.envelope.body_8bitmime,
            helo: &self.helo,
            peer: peer_buf,
            authenticated: self.auth.is_some(),
            tls: self.tls,
        }
    }
}

/// [`reserve_relay_commit`] over the owned forms (D-110): the session's entry
/// point since phase 0 of the segment work. Resolves the recorded ramp choice —
/// it never re-selects — and relays at `message.received_at`.
pub async fn reserve_relay_commit_owned(
    engine: &Engine,
    selection: &ramp_select::OwnedSelection,
    senders: &Senders,
    message: &OwnedMessage,
) -> Reply {
    let Some(selection) = selection.resolve(&engine.config) else {
        // Unreachable while the config is fixed for the life of the process;
        // answered rather than panicked because the message is accepted, and
        // §14.1 makes the answer temporary.
        tracing::error!(
            correlation_id = %message.correlation_id,
            ramp = %selection.ramp,
            "the selected ramp is not in the configuration"
        );
        return Reply::new(451, "4.3.0 internal configuration error");
    };
    let mut peer = String::new();
    reserve_relay_commit(
        engine,
        &selection,
        senders,
        message.as_message(&mut peer),
        &message.correlation_id,
        message.received_at,
    )
    .await
}

/// §10.4 — release whatever this process is still holding.
pub async fn release_outstanding(engine: &Engine) {
    let outstanding = engine.registry.drain();
    if outstanding.is_empty() {
        return;
    }

    tracing::info!(
        count = outstanding.len(),
        "releasing reservations held by in-flight sessions"
    );

    for reservation in &outstanding {
        if let Err(e) = engine.quota.release(reservation).await {
            // The sweeper is the backstop; it will pick this up at `expires_at`.
            tracing::error!(
                reservation = %reservation.id,
                error = %e,
                "failed to release a reservation at shutdown; the sweeper will collect it"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
server:
  listeners:
    - address: "127.0.0.1:25"
  hostname: simmer.test
  max_message_bytes: 1024
  max_recipients: 10
  max_concurrent_sessions: 4
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth: { allow_insecure_auth: true }
database: { url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }
admin: { listen: "127.0.0.1:8080", auth_token: "t" }
default_ramp: main
ramps:
 main:
  domain_groups:
  - { name: catchall, domains: ["*"] }
  senders:
  - { match: "oldbrand.com", match_on: envelope, chain: [warming, overflow] }
  default_chain: [overflow]
  routes:
  - name: warming
    downstream:
      host: warm.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@newbrand.com" }
    warmup:
      started: "2026-01-01T00:00:00Z"
      schedule: { default: [10] }
  - name: overflow
    overflow: true
    downstream:
      host: over.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@established.com" }
"#;

    fn config(extra: &[(&str, &str)]) -> Config {
        let mut yaml = CFG.to_string();
        // Ramp-level keys: two spaces, inside the fixture's one ramp (D-099).
        for (k, v) in extra {
            yaml.push_str(&format!("  {k}: {v}\n"));
        }
        crate::config::from_str(&yaml, "test").expect("fixture is valid")
    }

    #[test]
    fn an_owned_message_lends_exactly_what_the_session_used_to_pass() {
        // D-110: the borrowed view of an owned message is field for field the
        // `Message` the session built before the owned hand-off existed.
        let owned = OwnedMessage {
            envelope: OwnedEnvelope {
                mail_from: Some("a@oldbrand.com".into()),
                recipients: vec!["b@example.com".into()],
                smtputf8: true,
                body_8bitmime: true,
            },
            body: b"Subject: x\r\n\r\nbody\r\n".to_vec(),
            helo: "app.internal".into(),
            peer: "10.1.2.3".parse().unwrap(),
            auth: Some("cfapp".into()),
            tls: true,
            received_at: chrono::DateTime::from_timestamp(1_767_225_600, 0).unwrap(),
            correlation_id: "cid".into(),
        };
        let mut peer = String::new();
        let m = owned.as_message(&mut peer);
        assert_eq!(m.mail_from, Some("a@oldbrand.com"));
        assert_eq!(m.recipients, ["b@example.com".to_string()]);
        assert_eq!(m.body, owned.body.as_slice());
        assert!(m.smtputf8 && m.body_8bitmime && m.authenticated && m.tls);
        assert_eq!(m.helo, "app.internal");
        // `SocketAddr::ip().to_string()`, as `session.rs` rendered it.
        assert_eq!(m.peer, "10.1.2.3");

        let anonymous = OwnedMessage {
            auth: None,
            peer: "::1".parse().unwrap(),
            ..owned.clone()
        };
        let mut peer = String::new();
        let m = anonymous.as_message(&mut peer);
        assert!(!m.authenticated);
        assert_eq!(m.peer, "::1");
    }

    #[test]
    fn a_matched_sender_yields_its_own_chain() {
        let cfg = config(&[]);
        let chain = resolve_chain(
            cfg.default_ramp(),
            &Senders::new(Some("a@oldbrand.com"), None),
        )
        .unwrap();
        assert_eq!(chain, ["warming".to_string(), "overflow".to_string()]);
    }

    #[test]
    fn an_unmatched_sender_falls_to_the_default_chain() {
        let cfg = config(&[]);
        let chain = resolve_chain(
            cfg.default_ramp(),
            &Senders::new(Some("a@elsewhere.com"), None),
        )
        .unwrap();
        assert_eq!(chain, ["overflow".to_string()]);
    }

    #[test]
    fn strict_senders_rejects_an_unmatched_sender() {
        let cfg = config(&[("strict_senders", "true")]);
        assert_eq!(
            resolve_chain(
                cfg.default_ramp(),
                &Senders::new(Some("a@elsewhere.com"), None)
            )
            .err(),
            Some(SelectError::StrictSenderRejected {
                domain: "elsewhere.com".into()
            })
        );
    }

    #[test]
    fn the_strict_sender_rejection_is_the_one_permitted_550() {
        // §10.3: "a policy statement about the *sender*, will not trigger
        // recipient suppression, and should be loud".
        let cfg = config(&[("strict_senders", "true")]);
        let err = SelectError::StrictSenderRejected {
            domain: "x.com".into(),
        };
        assert_eq!(err.to_reply(cfg.default_ramp()).code, 550);
    }

    #[test]
    fn chain_exhaustion_defaults_to_451_not_550() {
        let cfg = config(&[]);
        assert_eq!(
            SelectError::ChainExhausted
                .to_reply(cfg.default_ramp())
                .code,
            451
        );
    }

    #[test]
    fn chain_exhaustion_honours_an_explicit_550_policy() {
        let cfg = config(&[("exhausted_chain_reply", "\"550\"")]);
        assert_eq!(
            SelectError::ChainExhausted
                .to_reply(cfg.default_ramp())
                .code,
            550
        );
    }

    #[test]
    fn quota_unavailability_is_451_never_550() {
        // §7.5's reply, and §14.1's reasoning: Simmer's database being down says
        // nothing permanent about the recipient.
        let cfg = config(&[]);
        let r = SelectError::QuotaUnavailable.to_reply(cfg.default_ramp());
        assert_eq!(r.code, 451);
        assert!(r.to_wire().contains("4.3.0"));
    }

    #[test]
    fn fail_open_is_answered_as_an_exhausted_chain_today() {
        // D-107 / O-19: pins what `fail_closed: false` does now, pending the
        // spec author's answer — NOT what it should do. It never sends: the
        // outage becomes the ramp's exhausted-chain reply (451 4.7.1 by default,
        // and 550 under `exhausted_chain_reply: "550"`), not §7.5's 451 4.3.0.
        let mut cfg = config(&[]);
        cfg.database.fail_closed = false;
        let ramp = cfg.default_ramp();
        let err = quota_failure(
            &cfg,
            ramp,
            quota::QuotaError::Storage("down".into()),
            "test",
        );
        assert_eq!(err, SelectError::ChainExhausted);
        let r = err.to_reply(ramp);
        assert_eq!(r.code, 451);
        assert!(r.to_wire().contains("4.7.1"), "{}", r.to_wire());

        let mut cfg = config(&[("exhausted_chain_reply", "\"550\"")]);
        cfg.database.fail_closed = false;
        let ramp = cfg.default_ramp();
        let err = quota_failure(
            &cfg,
            ramp,
            quota::QuotaError::Storage("down".into()),
            "test",
        );
        assert_eq!(
            err.to_reply(ramp).code,
            550,
            "O-19 asks about this case too"
        );

        let cfg = config(&[]);
        let ramp = cfg.default_ramp();
        let err = quota_failure(
            &cfg,
            ramp,
            quota::QuotaError::Storage("down".into()),
            "test",
        );
        assert_eq!(err, SelectError::QuotaUnavailable, "the default is §7.5's");
    }

    #[test]
    fn a_missing_from_header_is_only_fatal_when_a_rule_needs_it() {
        let cfg = config(&[]);
        assert!(resolve_chain(
            cfg.default_ramp(),
            &Senders::new(Some("a@oldbrand.com"), None)
        )
        .is_ok());

        let mut cfg = config(&[]);
        cfg.default_ramp_mut().senders[0].match_on = crate::config::MatchOn::FromHeader;
        assert_eq!(
            resolve_chain(
                cfg.default_ramp(),
                &Senders::new(Some("a@oldbrand.com"), None)
            )
            .err(),
            Some(SelectError::MalformedFromHeader)
        );
    }

    #[test]
    fn a_null_envelope_sender_still_routes() {
        let cfg = config(&[]);
        let chain = resolve_chain(cfg.default_ramp(), &Senders::new(None, None)).unwrap();
        assert_eq!(chain, ["overflow".to_string()]);
    }
}
