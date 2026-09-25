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
) -> Result<(), SelectError> {
    let chain = resolve_chain(ramp, senders)?;

    match chain::any_eligible(ramp, &engine.groups, &engine.quota, chain, recipient).await {
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
        // Explicitly opted out of §7.5's default. Loud, because it means the
        // ramp is not being enforced right now.
        tracing::error!(
            error = %e,
            during,
            "quota store unavailable and fail_closed is false; proceeding WITHOUT \
             quota enforcement. The warm-up ramp is not being applied."
        );
        SelectError::ChainExhausted
    }
}

/// The whole of step 2 and step 3: reserve, relay, then commit or release.
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

    // -- §7.4 phase 1 -------------------------------------------------
    let mut evaluation = Vec::new();
    let selected = match chain::walk_and_reserve(
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
    )
    .await
    {
        Ok(Walk::Selected(s)) => {
            thread::observe(&ramp.name, &pin, Some(&s.route.name), s.over_cap);
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
        let _ = engine.quota.release(&selected.reservation).await;
        engine.registry.remove(selected.reservation.id);
        return Reply::new(451, "4.3.0 internal configuration error");
    };

    let rewritten = rewrite::rewrite(
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
            now: chrono::Utc::now(),
            uuid: &|| uuid::Uuid::new_v4().to_string(),
        },
    );

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
    .await;
    let elapsed = started.elapsed();
    metrics::downstream_latency(
        &selected.route.ramp,
        &selected.route.name,
        elapsed.as_secs_f64(),
    );

    let outcome = match &result {
        Ok(d) => {
            tracing::info!(
                correlation_id,
                route = %selected.route.name,
                code = d.code,
                latency_ms = elapsed.as_millis() as u64,
                "downstream accepted the message"
            );
            downstream::outcome::delivered(&selected.route.name, d)
        }
        Err(e) => downstream::outcome::failed(&selected.route.ramp, &selected.route.name, e),
    };

    // -- §7.4 phase 3 -------------------------------------------------
    //
    // "On downstream 2xx, move the count from reserved to committed... On any
    // failure, decrement reserved and delete the reservation." `outcome.commit`
    // is the §10.1 table's answer, carried since phase 2.
    let store = Arc::clone(&engine.quota);
    let resolution = if outcome.commit {
        // §7.4 phase 3, both halves in one transaction: the count moves from
        // `reserved` to `committed` and this message's recipient-frequency
        // events are recorded. `recipient_keys` is empty unless the selected
        // route declares a constraint.
        store
            .commit(&selected.reservation, &selected.recipient_keys)
            .await
    } else {
        store.release(&selected.reservation).await
    };
    engine.registry.remove(selected.reservation.id);

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
        // §9.1 gauges, from the row we just moved.
        if let Ok(usage) = store
            .usage(
                &selected.reservation.ramp,
                &selected.reservation.route,
                &selected.reservation.domain_group,
                selected.reservation.day_index,
            )
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
