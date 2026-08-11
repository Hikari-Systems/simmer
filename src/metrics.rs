//! §9.1 counters, behind named functions.
//!
//! Phases 2–6 installed no recorder, so every call here compiled down to a
//! branch on a null pointer and did nothing (D-021). Phase 7 adds
//! `metrics-exporter-prometheus` and `GET /metrics`, and — as D-021 predicted —
//! exactly one call site moved: [`message`] gained the `domain_group` §9.1
//! specifies for it and phase 2 could not yet supply. See D-054.
//!
//! Wrapping the `metrics!` macros in functions rather than calling them inline
//! buys three things: a metric name is spelled once in the whole codebase, so a
//! typo cannot silently create a second series; the label sets stay in one
//! place; and [`describe`] can register a description for every metric against
//! that same single spelling.
//!
//! What §9.1 lists and this module does not emit, all by dependency rather than
//! oversight:
//!
//! - `simmer_pool_connections{route,state}` — there is no connection pool until
//!   phase 10 (D-019). One connection per message has no states to report.
//! - `simmer_preflight_ok{route,check}` — §6.7's DNS preflight is phase 8.
//! - `simmer_partial_delivery_total` — void. It counts a transaction whose
//!   recipients did not all share an outcome, and D-047 makes that unreachable.

use metrics::counter;
use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder, PrometheusHandle};

/// Install the §9.1 Prometheus recorder and return the handle `GET /metrics`
/// renders.
///
/// Fails only if a recorder is already installed, which in a process with one
/// `main` means it has been called twice.
pub fn install() -> Result<PrometheusHandle, BuildError> {
    let handle = PrometheusBuilder::new()
        // §9.1 says histogram. The exporter's default rendering for a histogram
        // is a summary with quantiles computed in-process, which cannot be
        // aggregated across instances — declaring buckets is what makes it an
        // actual Prometheus histogram.
        .set_buckets_for_metric(
            Matcher::Full("simmer_downstream_latency_seconds".to_string()),
            LATENCY_BUCKETS,
        )?
        .install_recorder()?;

    describe();
    Ok(handle)
}

/// Buckets for `simmer_downstream_latency_seconds`.
///
/// Spread for an SMTP conversation rather than an HTTP request: the fast end is
/// a warm local relay, the slow end is a provider under load, and the top bucket
/// is above §8.4's default data timeout so a run of timeouts is visible as
/// latency rather than only as errors.
const LATENCY_BUCKETS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];

/// Register a description for every metric §9.1 asks for.
///
/// These become `# HELP` lines. Worth the space because the audience for a
/// metric name at 3am is not the person who chose it, and three of these —
/// `simmer_downstream_config_error_total`, `simmer_reservation_expired_total`
/// and `simmer_unmatched_sender_total` — are ones the spec says to alert on.
fn describe() {
    use metrics::{describe_counter, describe_gauge, describe_histogram, Unit};

    describe_counter!(
        "simmer_messages_total",
        "Messages by route, domain group and outcome (§9.1)"
    );
    describe_counter!(
        "simmer_downstream_errors_total",
        "Downstream failures by route and error class (§9.1)"
    );
    describe_counter!(
        "simmer_downstream_config_error_total",
        "Downstream rejections that look like unfinished provider domain \
         authentication (§6.5, D-008). ALERT ON THIS: it is invisible in the mail flow"
    );
    describe_counter!(
        "simmer_ambiguous_delivery_total",
        "Sends whose fate is unknown: the connection dropped after the terminating \
         dot and before a reply (§10.2). A nonzero rate means duplicates or losses"
    );
    describe_counter!(
        "simmer_unmatched_sender_total",
        "Messages whose sender matched no rule and fell to the default chain (§14.2). \
         ALERT ON THIS: it is how a typo sends unwarmed traffic at full volume"
    );
    describe_counter!(
        "simmer_sender_mismatch_total",
        "Transactions where the envelope sender and the From: header disagreed (§5.4)"
    );
    describe_counter!(
        "simmer_body_rewrite_skipped_total",
        "text/* parts a route's body_rewrites was configured to change and did not (§6.4)"
    );
    describe_histogram!(
        "simmer_downstream_latency_seconds",
        Unit::Seconds,
        "Time from opening the downstream connection to its reply at the final dot"
    );
    describe_counter!(
        "simmer_connections_refused_total",
        "Connections refused before a session began: outside allowed_cidrs, or over \
         max_concurrent_sessions (§5.1)"
    );
    describe_gauge!(
        "simmer_quota_allowance",
        "Today's ceiling for a route and domain group; +Inf for an overflow route (§7.2, D-024)"
    );
    describe_gauge!(
        "simmer_quota_committed",
        "Messages delivered today against this route and domain group (§7.4)"
    );
    describe_gauge!(
        "simmer_quota_reserved",
        "Reservations outstanding right now for this route and domain group (§7.4)"
    );
    describe_gauge!(
        "simmer_warmup_day",
        "Elapsed-duration day index since warmup.started (§7.2). Never calendar arithmetic. \
         Negative means the route has not started, and its allowance reads 0"
    );
    describe_gauge!(
        "simmer_route_paused",
        "1 when an operator has paused this route through §9.3, 0 otherwise"
    );
    describe_counter!(
        "simmer_route_skipped_total",
        "Routes passed over during a chain walk, by reason (§3.2 step 3)"
    );
    describe_counter!(
        "simmer_reservation_expired_total",
        "Reservations released by the sweeper rather than by their own send (§7.4). \
         ALERT ON THIS: it means crashes or a mistuned timeout, and it under-reports \
         headroom for the rest of the day"
    );
    describe_counter!(
        "simmer_recipient_events_evicted_total",
        "recipient_event rows evicted past their retention (§7.3). Deliberately \
         unlabelled — a label per route would accumulate what the hashing exists to avoid"
    );
    describe_counter!(
        "simmer_quota_unavailable_total",
        "Messages answered 451 because the quota store was unreachable (§7.5)"
    );
    describe_counter!(
        "simmer_admin_mutations_total",
        "§9.3 write API mutations, by action and outcome"
    );
    describe_counter!(
        "simmer_admin_auth_failures_total",
        "Admin requests refused for authentication (§9.3). ALERT ON reason=\"invalid\": \
         the write API can pause a route or zero an allowance"
    );
}

/// §9.1 `simmer_messages_total{route,domain_group,result}`.
///
/// The `domain_group` label was `"-"` from phase 2 to phase 6, because phase 2
/// wrote this function before §3.2 step 2 existed to resolve a group. It is real
/// now: the one call site is in `relay.rs`, after the chain walk, and the walk's
/// whole job is to produce the `(route, domain_group)` pair the quota is keyed
/// on. See D-054 — this is the single call site D-021 said phase 7 would not
/// need to touch, and it is worth understanding rather than preserving.
pub fn message(route: &str, domain_group: &str, result: MessageResult) {
    counter!(
        "simmer_messages_total",
        "route" => route.to_string(),
        "domain_group" => domain_group.to_string(),
        "result" => result.as_str(),
    )
    .increment(1);
}

/// §9.3 — one mutation attempt, by action and outcome.
///
/// Not in §9.1's list. Added because a pause and an allowance override are the
/// two things in this service that can make every message `451`, and the audit
/// log is per-line while an alert needs a rate. `outcome` distinguishes an
/// applied mutation from one refused for authentication, so a run of
/// `rejected` is visible as the credential-guessing it would be.
pub fn admin_mutation(action: &'static str, outcome: &'static str) {
    counter!(
        "simmer_admin_mutations_total",
        "action" => action,
        "outcome" => outcome,
    )
    .increment(1);
}

/// §9.3 — a request refused for authentication, by why.
///
/// `missing` and `malformed` are usually a misconfigured client; a run of
/// `invalid` is somebody guessing. **Alert on `invalid`**: the write API can
/// pause a route, and §14.1 makes that a serious operational event however
/// correct the resulting `451` is.
pub fn admin_auth_failure(reason: &'static str) {
    counter!("simmer_admin_auth_failures_total", "reason" => reason).increment(1);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageResult {
    Delivered,
    Deferred,
    Rejected,
}

impl MessageResult {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageResult::Delivered => "delivered",
            MessageResult::Deferred => "deferred",
            MessageResult::Rejected => "rejected",
        }
    }
}

/// §9.1 `simmer_downstream_errors_total{route,class}`.
pub fn downstream_error(route: &str, class: &'static str) {
    counter!("simmer_downstream_errors_total", "route" => route.to_string(), "class" => class)
        .increment(1);
}

/// D-008 `simmer_downstream_config_error_total{route,stage}`.
///
/// The signal that catches the §6.5 provisioning risk — a downstream rejecting
/// our envelope sender because provider domain authentication is unfinished —
/// which is otherwise completely invisible in the mail flow. **Alert on this.**
pub fn downstream_config_error(route: &str, stage: &'static str) {
    counter!(
        "simmer_downstream_config_error_total",
        "route" => route.to_string(),
        "stage" => stage,
    )
    .increment(1);
}

/// §10.2 `simmer_ambiguous_delivery_total`. A nonzero rate means messages are
/// being duplicated by client retries, or lost.
pub fn ambiguous_delivery() {
    counter!("simmer_ambiguous_delivery_total").increment(1);
}

/// §9.1 `simmer_unmatched_sender_total{domain}`.
///
/// §14.2: a typo in a sender rule sends unwarmed traffic at full volume via the
/// established identity, and this counter is the only thing that would show it.
pub fn unmatched_sender(domain: &str) {
    counter!("simmer_unmatched_sender_total", "domain" => domain.to_string()).increment(1);
}

/// §9.1 `simmer_sender_mismatch_total` (§5.4).
pub fn sender_mismatch() {
    counter!("simmer_sender_mismatch_total").increment(1);
}

/// §9.1 `simmer_body_rewrite_skipped_total{route,reason}` (§6.4).
///
/// The one §9.1 counter phase 5 owns. Every increment is a `text/*` part the
/// route's `body_rewrites` was configured to change and did not: `signed` and
/// `encrypted` are §6.4 protecting a signature, and the rest are a part Simmer
/// cannot read or cannot write back. A rewrite that silently stops applying is
/// invisible everywhere else in the mail flow, which is why the reasons that are
/// working as designed are counted alongside the ones that are not.
pub fn body_rewrite_skipped(route: &str, reason: &'static str) {
    counter!(
        "simmer_body_rewrite_skipped_total",
        "route" => route.to_string(),
        "reason" => reason,
    )
    .increment(1);
}

/// §9.1 `simmer_downstream_latency_seconds{route}` — a histogram in phase 7.
pub fn downstream_latency(route: &str, seconds: f64) {
    metrics::histogram!("simmer_downstream_latency_seconds", "route" => route.to_string())
        .record(seconds);
}

/// Connections refused before a session began: outside `allowed_cidrs` (§5.1) or
/// over `max_concurrent_sessions` (§5.1). Not in §9.1's list; added because a
/// refusal at this layer produces no message and would otherwise leave no trace
/// in the metrics at all.
pub fn connection_refused(reason: &'static str) {
    counter!("simmer_connections_refused_total", "reason" => reason).increment(1);
}

// ---------------------------------------------------------------------------
// §7 quota (phase 3)
// ---------------------------------------------------------------------------

/// §9.1 `simmer_quota_allowance{route,domain_group}` — today's ceiling.
///
/// An overflow route reports `+Inf` rather than being omitted: §3.1 says it is
/// never quota-limited, and `+Inf` says exactly that in a way a dashboard can
/// plot alongside the warming routes (D-024).
pub fn quota_allowance(route: &str, domain_group: &str, allowance: f64) {
    metrics::gauge!(
        "simmer_quota_allowance",
        "route" => route.to_string(),
        "domain_group" => domain_group.to_string(),
    )
    .set(allowance);
}

/// §9.1 `simmer_quota_committed{route,domain_group}` — used today.
pub fn quota_committed(route: &str, domain_group: &str, committed: f64) {
    metrics::gauge!(
        "simmer_quota_committed",
        "route" => route.to_string(),
        "domain_group" => domain_group.to_string(),
    )
    .set(committed);
}

/// §9.1 `simmer_quota_reserved{route,domain_group}`.
pub fn quota_reserved(route: &str, domain_group: &str, reserved: f64) {
    metrics::gauge!(
        "simmer_quota_reserved",
        "route" => route.to_string(),
        "domain_group" => domain_group.to_string(),
    )
    .set(reserved);
}

/// §9.1 `simmer_warmup_day{route}`.
pub fn warmup_day(route: &str, day_index: i64) {
    metrics::gauge!("simmer_warmup_day", "route" => route.to_string()).set(day_index as f64);
}

/// §9.3's pause, as a gauge. Not in §9.1's list — see D-056.
///
/// `simmer_route_skipped_total{reason="paused"}` only moves when a message is
/// actually steered past the route, so a route paused weeks ago on a chain that
/// nothing currently reaches leaves no trace at all. That is precisely the state
/// somebody eventually goes looking for, usually while asking why a ramp stopped
/// advancing.
pub fn route_paused(route: &str, paused: bool) {
    metrics::gauge!("simmer_route_paused", "route" => route.to_string()).set(if paused {
        1.0
    } else {
        0.0
    });
}

/// §9.1 `simmer_route_skipped_total{route,reason}` — reason: `quota`,
/// `frequency`, `paused`, `preflight`, and `not_started` (see
/// `chain::SkipReason`).
pub fn route_skipped(route: &str, reason: &str) {
    counter!(
        "simmer_route_skipped_total",
        "route" => route.to_string(),
        "reason" => reason.to_string(),
    )
    .increment(1);
}

/// §9.1 `simmer_reservation_expired_total{route}`.
///
/// §7.4: "a nonzero rate indicates crashes or a mistuned timeout." **Alert on
/// this** — a mistuned expiry silently under-reports headroom for the rest of
/// the day.
pub fn reservation_expired(route: &str, count: i64) {
    counter!("simmer_reservation_expired_total", "route" => route.to_string())
        .increment(count.max(0) as u64);
}

/// §7.3 — `recipient_event` rows evicted past their retention.
///
/// Not in §9.1's list. Deliberately unlabelled: the whole point of §7.3's
/// hashing is that the container does not accumulate a record of who was mailed,
/// and a label per route would be the thin end of that. A count is enough to see
/// that the sweeper is running and that the table is not growing without bound.
pub fn recipient_events_evicted(count: u64) {
    counter!("simmer_recipient_events_evicted_total").increment(count);
}

/// §7.5 — a message answered `451` because the quota store was unreachable.
/// Not in §9.1's list; added because fail-closed is otherwise indistinguishable
/// from a downstream outage in the metrics.
pub fn quota_unavailable(route: &str) {
    counter!("simmer_quota_unavailable_total", "route" => route.to_string()).increment(1);
}
