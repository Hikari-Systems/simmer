//! §9.1 counters, behind named functions.
//!
//! Phase 2 installs no recorder, so every call here compiles down to a
//! branch on a null pointer and does nothing. Phase 7 adds
//! `metrics-exporter-prometheus` and the Prometheus endpoint, and no call site
//! changes. See `DECISIONS.md` D-021.
//!
//! Wrapping the `metrics!` macros in functions rather than calling them inline
//! buys two things: a metric name is spelled once in the whole codebase, so a
//! typo cannot silently create a second series; and the label sets stay in one
//! place, which is where phase 7 will need to register descriptions.

use metrics::counter;

/// §9.1 `simmer_messages_total{route,domain_group,result}`.
///
/// `domain_group` is `"-"` until phase 3 resolves it — an empty label value and
/// an absent label are different series in Prometheus, and a placeholder that
/// later becomes a real value is easier to spot than a blank.
pub fn message(route: &str, result: MessageResult) {
    counter!(
        "simmer_messages_total",
        "route" => route.to_string(),
        "domain_group" => "-",
        "result" => result.as_str(),
    )
    .increment(1);
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

/// §7.5 — a message answered `451` because the quota store was unreachable.
/// Not in §9.1's list; added because fail-closed is otherwise indistinguishable
/// from a downstream outage in the metrics.
pub fn quota_unavailable(route: &str) {
    counter!("simmer_quota_unavailable_total", "route" => route.to_string()).increment(1);
}
