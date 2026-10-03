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
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

/// When the recorder was installed, as Unix seconds — `process_start_time_seconds`
/// (D-075). Installation is the first thing `main` does after reading its config,
/// so this is the process start to within milliseconds, without parsing
/// `/proc/self/stat` against the boot time.
static STARTED: OnceLock<f64> = OnceLock::new();

/// Install the §9.1 Prometheus recorder alone and return the handle
/// `GET /metrics` renders — [`install_with`] with no OTLP export.
///
/// Fails only if a recorder is already installed, which in a process with one
/// `main` means it has been called twice.
pub fn install(idle_timeout: std::time::Duration) -> anyhow::Result<PrometheusHandle> {
    install_with(Some(idle_timeout), None).map(|h| h.expect("a Prometheus recorder was asked for"))
}

/// Install the §9.1 recorder: Prometheus when `prometheus` carries D-093's idle
/// timeout (`admin.metrics` is on), the OTLP bridge when `otel` is given
/// (`telemetry.metrics` is on, D-126), both behind one fan-out when both are,
/// and nothing at all when neither is — every `metrics::` call then stays the
/// no-op D-093 promises.
///
/// Returns the Prometheus handle when there is one. Fails only if a recorder is
/// already installed.
pub fn install_with(
    prometheus: Option<std::time::Duration>,
    otel: Option<crate::telemetry::metrics::OtelRecorder>,
) -> anyhow::Result<Option<PrometheusHandle>> {
    let prometheus = match prometheus {
        Some(idle_timeout) => {
            let mut builder = PrometheusBuilder::new()
                // D-093: a counter idle this long is dropped, which is what
                // returns F7's memory — one series per unmatched sender domain,
                // held for the life of the process until now. Counters only:
                // gauges are recomputed at every scrape (D-056), and histograms
                // are few and fixed.
                .idle_timeout(metrics_util::MetricKindMask::COUNTER, Some(idle_timeout));
            // §9.1 says histogram. The exporter's default rendering for a
            // histogram is a summary with quantiles computed in-process, which
            // cannot be aggregated across instances — declaring buckets is what
            // makes it an actual Prometheus histogram.
            for (name, buckets) in HISTOGRAM_BUCKETS {
                builder =
                    builder.set_buckets_for_metric(Matcher::Full((*name).to_string()), buckets)?;
            }
            Some(builder.build_recorder())
        }
        None => None,
    };
    let handle = prometheus.as_ref().map(|r| r.handle());

    match (prometheus, otel) {
        (None, None) => return Ok(None),
        (Some(p), None) => metrics::set_global_recorder(p).map_err(already_installed)?,
        (None, Some(o)) => metrics::set_global_recorder(o).map_err(already_installed)?,
        (Some(p), Some(o)) => metrics::set_global_recorder(
            metrics_util::layers::FanoutBuilder::default()
                .add_recorder(p)
                .add_recorder(o)
                .build(),
        )
        .map_err(already_installed)?,
    }

    describe();
    let _ = STARTED.set(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
    );
    Ok(handle)
}

fn already_installed<R>(_: metrics::SetRecorderError<R>) -> anyhow::Error {
    anyhow::anyhow!("a metrics recorder is already installed")
}

/// Every histogram's explicit buckets, by name — one table for both exporters,
/// so Prometheus and OTLP cannot disagree about a bucket edge.
pub const HISTOGRAM_BUCKETS: &[(&str, &[f64])] = &[
    ("simmer_downstream_latency_seconds", LATENCY_BUCKETS),
    ("simmer_link_proxy_duration_seconds", LINK_PROXY_BUCKETS),
    ("simmer_rate_wait_seconds", RATE_WAIT_BUCKETS),
];

/// Buckets for `simmer_downstream_latency_seconds`.
///
/// Spread for an SMTP conversation rather than an HTTP request: the fast end is
/// a warm local relay, the slow end is a provider under load, and the top bucket
/// is above §8.4's default data timeout so a run of timeouts is visible as
/// latency rather than only as errors.
const LATENCY_BUCKETS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
];

/// Buckets for `simmer_link_proxy_duration_seconds` (D-083): a click is one HTTP
/// round trip to a tracking service, and the top bucket sits above the default
/// `upstream_response` timeout of 30 s.
const LINK_PROXY_BUCKETS: &[f64] = &[
    0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

/// Buckets for `simmer_rate_wait_seconds` (D-111). The first bucket is a send
/// that did not wait at all; the top is `rate.max_wait`'s ceiling of 60 s.
const RATE_WAIT_BUCKETS: &[f64] = &[0.0, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 45.0, 60.0];

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
        "simmer_ambiguous_terminator_total",
        "Messages whose DATA held an end-of-data marker with a bare CR or LF beside it — \
         the SMTP-smuggling shape (D-095). Every one is refused and relayed nowhere, \
         with 554, or 552 if it was also over-length. ALERT ON THIS: it is either a \
         client emitting bare line endings or an attempt to inject a second envelope"
    );
    describe_counter!(
        "simmer_sender_mismatch_total",
        "Transactions where the envelope sender and the From: header disagreed (§5.4)"
    );
    describe_counter!(
        "simmer_body_rewrite_skipped_total",
        "text/* parts a route's body_rewrites was configured to change and did not (§6.4)"
    );
    describe_counter!(
        "simmer_header_rewrite_skipped_total",
        "Header instances a route's header_rewrites was configured to change and did not (D-089)"
    );
    describe_counter!(
        "simmer_thread_affinity_total",
        "Messages that referred to a message ID (§3.2 step 2a, D-090), by the route that \
         emitted it and outcome: hit — that route carried the reply within its cap; over_cap — \
         it carried the reply past its day's cap; ineligible — it was paused, failing strict \
         preflight or not yet started, and the ordinary walk decided; unmatched — no route in \
         the chain emitted any ID referred to (route is \"-\")"
    );
    describe_histogram!(
        "simmer_rate_wait_seconds",
        Unit::Seconds,
        "How long a message on a rate-limited route was held for its booked slot before the \
         downstream conversation began (D-111), by ramp, route and domain group. 0 is a slot \
         that was free at once; the ceiling is the route's rate.max_wait"
    );
    describe_counter!(
        "simmer_rate_slots_unbooked_total",
        "Rate slots given back because the message they were booked for was not sent — no \
         headroom, a downstream failure, a storage error (D-111). A slot is given back only \
         if nothing was booked after it"
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
        "simmer_partial_ramp_share",
        "The fraction of the traffic reaching this route and domain group that is offered \
         to it right now (§3.2 step 3c′). Absent when every message is offered. Under \
         share: auto it moves as the cap fills (D-097); watch it against \
         simmer_route_skipped_total{reason=\"partial_ramp\"}"
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
        "Routes passed over during a chain walk, by reason (§3.2 step 3). \
         reason=\"partial_ramp\" is a route given less traffic on purpose (D-091), not a \
         shortage"
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
        "simmer_auth_verifies_in_flight",
        "argon2 verifications running right now (D-079). Each holds auth.m_cost of \
         memory while it runs; pinned at simmer_auth_verifies_max means logins are \
         queueing for a permit"
    );
    describe_gauge!(
        "simmer_auth_verifies_max",
        "D-079's bound on concurrent argon2 verifications: twice the usable cores, \
         at least 4"
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
    describe_counter!(
        "simmer_ramp_selected_total",
        "D-099 messages by the ramp §5.8 chose and the rule that chose it: affinity \
         (the listener's ramp), header (X-Simmer-Ramp) or default (default_ramp)"
    );
    describe_counter!(
        "simmer_ramp_header_rejected_total",
        "D-099 X-Simmer-Ramp headers ignored, by reason: unknown, not_permitted, \
         malformed, conflicting or affinity_locked. Never a refusal: the message \
         falls back to the listener's ramp or default_ramp. A typo shows here"
    );
    describe_counter!(
        "simmer_mx_lookups_total",
        "D-100 MX lookups for domain grouping, by result: ok, cached, error or timeout. \
         error and timeout put the recipient in the catch-all group"
    );
    describe_counter!(
        "simmer_link_proxy_requests_total",
        "D-083 link proxy requests by status class and origin: upstream (the upstream's \
         own response) or proxy (502, 504, 413, 501 or 508 answered by Simmer itself)"
    );
    describe_histogram!(
        "simmer_link_proxy_duration_seconds",
        Unit::Seconds,
        "D-083 link proxy time to response headers"
    );
    describe_counter!(
        "simmer_link_proxy_connections_refused_total",
        "D-083 link proxy connections refused before a request: outside \
         link_proxy.allowed_cidrs (cidr) or over link_proxy.max_connections (limit)"
    );
    describe_gauge!(
        "simmer_link_proxy_connections",
        "D-083 open link proxy client connections"
    );
    describe_gauge!(
        "simmer_tasks_alive",
        "Live tokio tasks: one per session plus the accept loops, sweepers and admin \
         server. Growth at a flat session count is a task leak"
    );

    // D-085 — the debugging capture. None of these carries a route, a sender, a
    // recipient or a path: the capture runs before route selection and does not
    // know a route, and a label naming anything about the message would put in
    // /metrics exactly what §7.3 hashes to keep out of the database. `reason` is
    // a closed set of &'static str, which is F7's lesson about label values the
    // client can influence.
    describe_counter!(
        "simmer_capture_records_total",
        "D-085 messages appended to the capture log"
    );
    describe_counter!(
        "simmer_capture_bytes_total",
        Unit::Bytes,
        "D-085 bytes appended to the capture log, serialised lines including base64"
    );
    describe_counter!(
        "simmer_capture_dropped_total",
        "D-085 records that never reached the file: queue_full, queue_bytes, \
         write_error, open_error or shutdown. Under the default on_error: continue \
         these are gaps in the capture and nothing else — the mail was unaffected"
    );
    describe_counter!(
        "simmer_capture_deferred_total",
        "D-085 messages answered 451 because the capture could not be written and \
         capture.on_error is defer. ALERT ON THIS: mail is being stopped for a \
         debugging feature"
    );
    describe_counter!(
        "simmer_capture_body_omitted_total",
        "D-085 records written without their body, over capture.max_body_bytes"
    );
    describe_counter!(
        "simmer_capture_late_writes_total",
        "D-085 records appended to a bucket file that had already been closed, \
         because they arrived at the writer out of order. Harmless — the record is \
         still in the bucket its timestamp belongs to, which is the invariant a \
         replay's file selection rests on"
    );
    describe_counter!(
        "simmer_capture_clock_regressions_total",
        "D-085 records whose timestamp was more than one bucket behind the newest \
         seen. Nonzero means this instance's wall clock stepped backwards; a replay \
         of that range may need a wider --pad-buckets"
    );
    describe_counter!(
        "simmer_capture_files_swept_total",
        "D-085 bucket files deleted past capture.retention"
    );
    describe_gauge!(
        "simmer_capture_queue_depth",
        "D-085 records queued for the capture writer"
    );
    describe_gauge!(
        "simmer_capture_queue_bytes",
        Unit::Bytes,
        "D-085 bytes queued for the capture writer, bounded by capture.max_queue_bytes"
    );
    describe_gauge!(
        "simmer_capture_disk_bytes",
        Unit::Bytes,
        "D-085 bytes on disk in the capture directory: incremented as the writer \
         flushes and recounted from the directory by each sweeper pass. This is \
         what tells you a capture left on will fill the volume"
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
pub fn message(ramp: &str, route: &str, domain_group: &str, result: MessageResult) {
    counter!(
        "simmer_messages_total",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
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
pub fn downstream_error(ramp: &str, route: &str, class: &'static str) {
    counter!("simmer_downstream_errors_total", "ramp" => ramp.to_string(), "route" => route.to_string(), "class" => class)
        .increment(1);
}

/// D-008 `simmer_downstream_config_error_total{route,stage}`.
///
/// The signal that catches the §6.5 provisioning risk — a downstream rejecting
/// our envelope sender because provider domain authentication is unfinished —
/// which is otherwise completely invisible in the mail flow. **Alert on this.**
pub fn downstream_config_error(ramp: &str, route: &str, stage: &'static str) {
    counter!(
        "simmer_downstream_config_error_total",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
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
pub fn unmatched_sender(ramp: &str, domain: &str) {
    counter!(
        "simmer_unmatched_sender_total",
        "ramp" => ramp.to_string(),
        "domain" => domain.to_string(),
    )
    .increment(1);
}

/// `simmer_ambiguous_terminator_total` — see [`crate::smtp`]'s DATA reader.
pub fn ambiguous_terminator() {
    counter!("simmer_ambiguous_terminator_total").increment(1);
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
pub fn body_rewrite_skipped(ramp: &str, route: &str, reason: &'static str) {
    counter!(
        "simmer_body_rewrite_skipped_total",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
        "reason" => reason,
    )
    .increment(1);
}

/// §9.1 `simmer_header_rewrite_skipped_total{route,header,reason}` (§6.2,
/// D-089). `body_rewrite_skipped`'s argument for headers: a configured
/// rewrite that silently stops applying is invisible everywhere else. `header`
/// is bounded by configuration — it is always a name some route's
/// `header_rewrites` spells — never by what a message carries.
pub fn header_rewrite_skipped(ramp: &str, route: &str, header: &str, reason: &'static str) {
    counter!(
        "simmer_header_rewrite_skipped_total",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
        "header" => header.to_string(),
        "reason" => reason,
    )
    .increment(1);
}

/// §9.1 `simmer_thread_affinity_total{route,outcome}` (§3.2 step 2a, D-090).
/// `over_cap` is the one to watch: each is a send the ramp did not schedule,
/// which the spec's author chose over a conversation changing identity
/// mid-thread. `ineligible` is a thread that did change identity. `route` is
/// bounded by configuration — a pinned route is always a configured one — and
/// is `-` for `unmatched`, which names no route.
pub fn thread_affinity(ramp: &str, route: &str, outcome: &'static str) {
    counter!(
        "simmer_thread_affinity_total",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
        "outcome" => outcome,
    )
    .increment(1);
}

/// §9.1 `simmer_ramp_selected_total{ramp,source}` (D-099).
pub fn ramp_selected(ramp: &str, source: &'static str) {
    counter!(
        "simmer_ramp_selected_total",
        "ramp" => ramp.to_string(),
        "source" => source,
    )
    .increment(1);
}

/// §9.1 `simmer_ramp_header_rejected_total{reason}` (D-099). No value label:
/// the client chooses the value, and a label it chooses grows without bound
/// (D-093's lesson).
pub fn ramp_header_rejected(reason: &'static str) {
    counter!("simmer_ramp_header_rejected_total", "reason" => reason).increment(1);
}

/// D-100 — one MX lookup for §3.2 step 2, or one answered from the cache.
/// `result` is a fixed set; the domain is deliberately not a label.
pub fn mx_lookup(result: &'static str) {
    counter!("simmer_mx_lookups_total", "result" => result).increment(1);
}

/// §9.1 `simmer_downstream_latency_seconds{route}` — a histogram in phase 7.
pub fn downstream_latency(ramp: &str, route: &str, seconds: f64) {
    metrics::histogram!("simmer_downstream_latency_seconds", "ramp" => ramp.to_string(), "route" => route.to_string())
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

/// D-079 — how many argon2 verifications are running, against the bound.
///
/// Takes the permits *available* rather than the count in flight, because that is
/// what the semaphore can tell us without a second counter to keep in step.
pub fn auth_verifies_in_flight(available: usize, max: usize) {
    metrics::gauge!("simmer_auth_verifies_in_flight").set(max.saturating_sub(available) as f64);
    metrics::gauge!("simmer_auth_verifies_max").set(max as f64);
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
pub fn pool_connections(ramp: &str, route: &str, state: &'static str, count: f64) {
    metrics::gauge!(
        "simmer_pool_connections",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
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
pub fn pool_retry(ramp: &str, route: &str) {
    counter!("simmer_pool_retries_total", "ramp" => ramp.to_string(), "route" => route.to_string())
        .increment(1);
}

// ---------------------------------------------------------------------------
// §7 quota (phase 3)
// ---------------------------------------------------------------------------

/// §9.1 `simmer_quota_allowance{route,domain_group}` — today's ceiling.
///
/// An overflow route reports `+Inf` rather than being omitted: §3.1 says it is
/// never quota-limited, and `+Inf` says exactly that in a way a dashboard can
/// plot alongside the warming routes (D-024).
pub fn quota_allowance(ramp: &str, route: &str, domain_group: &str, allowance: f64) {
    metrics::gauge!(
        "simmer_quota_allowance",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
        "domain_group" => domain_group.to_string(),
    )
    .set(allowance);
}

/// §9.1 `simmer_quota_committed{route,domain_group}` — used today.
pub fn quota_committed(ramp: &str, route: &str, domain_group: &str, committed: f64) {
    metrics::gauge!(
        "simmer_quota_committed",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
        "domain_group" => domain_group.to_string(),
    )
    .set(committed);
}

/// §9.1 `simmer_quota_reserved{route,domain_group}`.
pub fn quota_reserved(ramp: &str, route: &str, domain_group: &str, reserved: f64) {
    metrics::gauge!(
        "simmer_quota_reserved",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
        "domain_group" => domain_group.to_string(),
    )
    .set(reserved);
}

/// §9.1 `simmer_partial_ramp_share{route,domain_group}` — §3.2 step 3c′'s share.
///
/// Recomputed at every scrape from the same function the walk calls (D-056), so
/// a dashboard cannot show a share the walk was not applying. A route offered
/// every message reports nothing at all rather than 1: the series existing is
/// what says a ramp is in force.
pub fn partial_ramp_share(ramp: &str, route: &str, domain_group: &str, share: f64) {
    metrics::gauge!(
        "simmer_partial_ramp_share",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
        "domain_group" => domain_group.to_string(),
    )
    .set(share);
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
pub fn preflight_ok(ramp: &str, route: &str, check: &str, ok: bool) {
    metrics::gauge!(
        "simmer_preflight_ok",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
        "check" => check.to_string(),
    )
    .set(if ok { 1.0 } else { 0.0 });
}

/// §9.1 `simmer_warmup_day{route}`.
pub fn warmup_day(ramp: &str, route: &str, day_index: i64) {
    metrics::gauge!("simmer_warmup_day", "ramp" => ramp.to_string(), "route" => route.to_string())
        .set(day_index as f64);
}

/// §9.3's pause, as a gauge. Not in §9.1's list — see D-056.
///
/// `simmer_route_skipped_total{reason="paused"}` only moves when a message is
/// actually steered past the route, so a route paused weeks ago on a chain that
/// nothing currently reaches leaves no trace at all. That is precisely the state
/// somebody eventually goes looking for, usually while asking why a ramp stopped
/// advancing.
pub fn route_paused(ramp: &str, route: &str, paused: bool) {
    metrics::gauge!("simmer_route_paused", "ramp" => ramp.to_string(), "route" => route.to_string()).set(if paused {
        1.0
    } else {
        0.0
    });
}

/// D-111 — `simmer_rate_wait_seconds{ramp,route,domain_group}`.
pub fn rate_wait(ramp: &str, route: &str, domain_group: &str, seconds: f64) {
    metrics::histogram!(
        "simmer_rate_wait_seconds",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
        "domain_group" => domain_group.to_string(),
    )
    .record(seconds);
}

/// D-111 — `simmer_rate_slots_unbooked_total{ramp,route}`.
pub fn rate_slot_unbooked(ramp: &str, route: &str) {
    counter!(
        "simmer_rate_slots_unbooked_total",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
    )
    .increment(1);
}

/// §9.1 `simmer_route_skipped_total{route,reason}` — reason: `quota`,
/// `frequency`, `paused`, `preflight`, `not_started`, `partial_ramp` (D-091),
/// and `rate` (D-111; see `chain::SkipReason`).
pub fn route_skipped(ramp: &str, route: &str, reason: &str) {
    counter!(
        "simmer_route_skipped_total",
        "ramp" => ramp.to_string(), "route" => route.to_string(),
        "reason" => reason.to_string(),
    )
    .increment(1);
}

/// §9.1 `simmer_reservation_expired_total{route}`.
///
/// §7.4: "a nonzero rate indicates crashes or a mistuned timeout." **Alert on
/// this** — a mistuned expiry silently under-reports headroom for the rest of
/// the day.
pub fn reservation_expired(ramp: &str, route: &str, count: i64) {
    counter!("simmer_reservation_expired_total", "ramp" => ramp.to_string(), "route" => route.to_string())
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
pub fn quota_unavailable(ramp: &str, route: &str) {
    counter!("simmer_quota_unavailable_total", "ramp" => ramp.to_string(), "route" => route.to_string()).increment(1);
}

/// D-083 — one link proxy request. `origin` is `upstream` or `proxy`.
pub fn link_proxy_request(status: u16, origin: &'static str, seconds: f64) {
    let class = match status {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    };
    counter!("simmer_link_proxy_requests_total", "status_class" => class, "origin" => origin)
        .increment(1);
    metrics::histogram!("simmer_link_proxy_duration_seconds").record(seconds);
}

/// D-083 — a link proxy connection refused before any request was read.
pub fn link_proxy_connection_refused(reason: &'static str) {
    counter!("simmer_link_proxy_connections_refused_total", "reason" => reason).increment(1);
}

/// D-083 — open link proxy client connections.
pub fn link_proxy_connections(open: usize) {
    metrics::gauge!("simmer_link_proxy_connections").set(open as f64);
}

// ---------------------------------------------------------------------------
// D-085 — the debugging capture
// ---------------------------------------------------------------------------

/// One record appended to the capture log, and the bytes it took.
pub fn capture_written(bytes: u64) {
    counter!("simmer_capture_records_total").increment(1);
    counter!("simmer_capture_bytes_total").increment(bytes);
}

/// A record that never reached the file. `reason` is one of `queue_full`,
/// `queue_bytes`, `write_error`, `open_error`, `shutdown`.
pub fn capture_dropped(reason: &'static str) {
    counter!("simmer_capture_dropped_total", "reason" => reason).increment(1);
}

/// A message answered `451` because `capture.on_error` is `defer`.
pub fn capture_deferred() {
    counter!("simmer_capture_deferred_total").increment(1);
}

/// A record written without its body, over `capture.max_body_bytes`.
pub fn capture_body_omitted() {
    counter!("simmer_capture_body_omitted_total").increment(1);
}

/// A record appended to a bucket file that had already been closed.
pub fn capture_late_write() {
    counter!("simmer_capture_late_writes_total").increment(1);
}

/// A record whose timestamp was more than one bucket behind the newest seen.
pub fn capture_clock_regression() {
    counter!("simmer_capture_clock_regressions_total").increment(1);
}

/// Bucket files deleted past their retention.
pub fn capture_files_swept(n: u64) {
    counter!("simmer_capture_files_swept_total").increment(n);
}

/// The writer's queue, as it stood at the last idle tick.
pub fn capture_queue(depth: usize, bytes: u64) {
    metrics::gauge!("simmer_capture_queue_depth").set(depth as f64);
    metrics::gauge!("simmer_capture_queue_bytes").set(bytes as f64);
}

/// Bytes on disk in the capture directory, **counted** from the directory itself.
///
/// The sweeper's authority: it has just looked at every file, so this is the truth
/// and it resets whatever [`capture_disk_grew`] has been adding since the last
/// pass. It is also what initialises the gauge at startup, where the directory may
/// already hold hours of a previous run's buckets — the sweeper's first pass runs
/// immediately for exactly that reason.
pub fn capture_disk_bytes(n: u64) {
    metrics::gauge!("simmer_capture_disk_bytes").set(n as f64);
}

/// Bytes the writer has just flushed to the current bucket.
///
/// Why the gauge is not left to the sweeper alone (F17): the sweeper runs hourly,
/// so a gauge only it wrote read 0 for the whole first hour and was up to an hour
/// stale after that — no use at all for the one thing docs/CAPTURE.md offers it
/// for, noticing that a capture left on is filling a volume. Adding what was
/// flushed, as it is flushed, makes it live to within the writer's flush policy.
///
/// It is an **estimate between sweeps**, deliberately. It cannot see a bucket an
/// operator deleted by hand, a file something else gzipped, or the difference
/// between bytes written and blocks occupied, and nothing here tries to: the
/// sweeper's pass corrects all of it, so the drift is bounded by one interval.
/// D-056's rule — that a number derived from the real directory cannot drift — is
/// why that pass stays authoritative rather than being replaced by this.
pub fn capture_disk_grew(n: u64) {
    metrics::gauge!("simmer_capture_disk_bytes").increment(n as f64);
}
