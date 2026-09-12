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
//! - `simmer_partial_delivery_total` — void. It counts a transaction whose
//!   recipients did not all share an outcome, and D-047 makes that unreachable.

use std::sync::OnceLock;

use metrics::counter;
use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder, PrometheusHandle};

/// When the recorder was installed, as Unix seconds — `process_start_time_seconds`
/// (D-075). Installation is the first thing `main` does after reading its config,
/// so this is the process start to within milliseconds, without parsing
/// `/proc/self/stat` against the boot time.
static STARTED: OnceLock<f64> = OnceLock::new();

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
    let _ = STARTED.set(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
    );
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
    describe_counter!(
        "simmer_inbound_tls_failures_total",
        "Inbound TLS that did not complete (§5.1, D-070), by listener mode and reason: \
         handshake, or plaintext_after_starttls — bytes pipelined behind STARTTLS, the \
         command-injection shape, which drops the connection"
    );
    describe_counter!(
        "simmer_sender_not_permitted_total",
        "Messages refused because the authenticated user's grants do not cover the \
         sender (D-071), by stage: mail_from or from_header. ALERT ON THIS: it is a \
         misconfigured application or somebody else's credentials"
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
    describe_gauge!(
        "simmer_pool_connections",
        "Downstream connections this process holds for a route (§8.3), by state: \
         active is checked out for a message, idle is waiting to be reused. \
         active pinned at max_connections means sessions are queueing for one"
    );
    describe_counter!(
        "simmer_pool_retries_total",
        "Messages re-sent on a fresh connection because a pooled one was dead on \
         reuse (§8.3). A steady rate means idle_ttl is above what the downstream allows"
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

    // D-075 — the process and runtime, read on every scrape. The process_* names
    // are the Prometheus client libraries' standard ones, so existing dashboards
    // and alerts work unchanged.
    describe_gauge!(
        "process_resident_memory_bytes",
        Unit::Bytes,
        "Resident set size (VmRSS). Excludes the tmpfs the §8.1 buffer spills to"
    );
    describe_gauge!("process_open_fds", "Open file descriptors");
    describe_gauge!("process_max_fds", "The soft limit on open file descriptors");
    describe_gauge!(
        "process_threads",
        "OS threads, including tokio's blocking pool"
    );
    describe_gauge!(
        "process_start_time_seconds",
        Unit::Seconds,
        "When the process started, as Unix time"
    );
    describe_gauge!(
        "simmer_sessions_active",
        "SMTP sessions holding a §5.1 permit right now, across every listener. At \
         simmer_sessions_max, new connections are refused 421 4.3.2"
    );
    describe_gauge!(
        "simmer_sessions_max",
        "server.max_concurrent_sessions (§5.1)"
    );
    describe_gauge!(
        "simmer_reservations_in_flight",
        "§7.4 reservations this process is holding in its §10.4 registry. Nonzero on an \
         idle instance means a reservation was stranded"
    );
    describe_gauge!(
        "simmer_db_pool_connections",
        "Postgres connections this process holds, by state: in_use or idle. in_use \
         pinned at simmer_db_pool_max precedes 451 4.3.0 (§7.5)"
    );
    describe_gauge!("simmer_db_pool_max", "database.max_connections");
    describe_gauge!(
        "simmer_tasks_alive",
        "Live tokio tasks: one per session plus the accept loops, sweepers and admin \
         server. Growth at a flat session count is a task leak"
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

/// D-070 — an inbound TLS session that did not start. `mode` is the listener's
/// (`implicit` or `starttls`), `reason` is `handshake` or
/// `plaintext_after_starttls`.
pub fn inbound_tls_failure(mode: &'static str, reason: &'static str) {
    counter!(
        "simmer_inbound_tls_failures_total",
        "mode" => mode,
        "reason" => reason
    )
    .increment(1);
}

/// D-071 — an authenticated sender outside its grants. `stage` is `mail_from` or
/// `from_header`. Unlabelled by user or address on purpose: the log line names
/// both, and a label per address would be the high-cardinality series §7.3's
/// hashing exists to avoid.
pub fn sender_not_permitted(stage: &'static str) {
    counter!("simmer_sender_not_permitted_total", "stage" => stage).increment(1);
}

/// What `/proc/self` says about this process (D-075). Each field is `None` when
/// its source cannot be read — not Linux, or procfs unavailable — and the gauge
/// is then simply not written, rather than written as a misleading zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessStats {
    pub resident_bytes: Option<u64>,
    pub open_fds: Option<u64>,
    pub max_fds: Option<u64>,
    pub threads: Option<u64>,
}

impl ProcessStats {
    pub fn read() -> ProcessStats {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let field = |name: &str| -> Option<u64> {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        };
        let limits = std::fs::read_to_string("/proc/self/limits").unwrap_or_default();
        ProcessStats {
            resident_bytes: field("VmRSS:").map(|kb| kb * 1024),
            threads: field("Threads:"),
            open_fds: std::fs::read_dir("/proc/self/fd")
                .ok()
                .map(|d| d.count() as u64),
            // "Max open files            1048576              1048576              files"
            max_fds: limits
                .lines()
                .find_map(|l| l.strip_prefix("Max open files"))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|soft| soft.parse().ok()),
        }
    }
}

/// D-075's process gauges, from one [`ProcessStats`] reading.
pub fn process(stats: ProcessStats) {
    let set = |name: &'static str, v: Option<u64>| {
        if let Some(v) = v {
            metrics::gauge!(name).set(v as f64);
        }
    };
    set("process_resident_memory_bytes", stats.resident_bytes);
    set("process_open_fds", stats.open_fds);
    set("process_max_fds", stats.max_fds);
    set("process_threads", stats.threads);
    if let Some(started) = STARTED.get() {
        metrics::gauge!("process_start_time_seconds").set(*started);
    }
}

/// D-075 — §5.1's session bound as it stands.
pub fn sessions(active: usize, max: usize) {
    metrics::gauge!("simmer_sessions_active").set(active as f64);
    metrics::gauge!("simmer_sessions_max").set(max as f64);
}

/// D-075 — the §10.4 registry's size.
pub fn reservations_in_flight(n: usize) {
    metrics::gauge!("simmer_reservations_in_flight").set(n as f64);
}

/// D-075 — the Postgres pool.
pub fn db_pool(in_use: u64, idle: u64, max: u64) {
    metrics::gauge!("simmer_db_pool_connections", "state" => "in_use").set(in_use as f64);
    metrics::gauge!("simmer_db_pool_connections", "state" => "idle").set(idle as f64);
    metrics::gauge!("simmer_db_pool_max").set(max as f64);
}

/// D-075 — live tokio tasks.
pub fn tasks_alive(n: usize) {
    metrics::gauge!("simmer_tasks_alive").set(n as f64);
}

/// §9.1 `simmer_pool_connections{route,state}` — `state` is `idle` or `active`.
///
/// Read off the pool on every scrape rather than written by the relay, which is
/// D-056 applied again: a gauge only ever set by a message that relayed reports
/// stale numbers for an idle route, and here it would report *nothing at all*
/// for a route that has never sent — the one whose pool an operator is most
/// likely to be asking about. It also keeps a metric update off the latency path
/// of every message.
///
/// A gauge over the pool's own counters rather than anything derived from the
/// semaphore: `available_permits` does not distinguish a connection being used
/// from one that has not been opened yet, and the difference between "four active"
/// and "four permits gone" is the difference between a busy downstream and a
/// stuck one.
pub fn pool_connections(route: &str, state: &'static str, count: f64) {
    metrics::gauge!(
        "simmer_pool_connections",
        "route" => route.to_string(),
        "state" => state,
    )
    .set(count);
}

/// §8.3 — a message re-sent because the pooled connection it started on was
/// already closed at the far end.
///
/// Not in §9.1's list. Added because the retry is invisible by design — the
/// client sees a normal `250` — and a steady rate is the signal that a route's
/// `idle_ttl` is longer than the downstream's own idle timeout, which costs an
/// extra connection and an extra round trip on that fraction of all mail.
pub fn pool_retry(route: &str) {
    counter!("simmer_pool_retries_total", "route" => route.to_string()).increment(1);
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

/// §9.1 `simmer_preflight_ok{route,check}` — §6.7's three checks, as 1 or 0.
///
/// A gauge rather than a counter because it is a *state*: "SPF is currently
/// wrong for this route" is the thing worth alerting on, and it stays wrong until
/// somebody edits a DNS zone. `check` is one of `spf`, `dkim`, `dmarc`.
///
/// Only routes preflight actually checks ever emit this. A route with the block
/// absent, disabled, or with a non-constant identity domain (D-064) publishes no
/// series at all rather than a misleading `1`.
pub fn preflight_ok(route: &str, check: &str, ok: bool) {
    metrics::gauge!(
        "simmer_preflight_ok",
        "route" => route.to_string(),
        "check" => check.to_string(),
    )
    .set(if ok { 1.0 } else { 0.0 });
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
