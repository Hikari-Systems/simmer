//! §9.1's Prometheus exposition, against a real recorder.
//!
//! **This suite exists as its own binary on purpose.** `metrics` permits exactly
//! one global recorder per process and installing a second fails, so any test
//! that needs a real one has to be alone with it. Every other suite builds its
//! `AdminState` with `metrics: None` and gets the no-op recorder phases 2–6 ran
//! against (D-021).
//!
//! **And every test here takes [`exclusive`] first.** The recorder is global and
//! a gauge is keyed only by its labels, so two tests scraping in parallel read
//! each other's `simmer_quota_allowance{route="warming",…}` — each has its own
//! `#[sqlx::test]` database, but they share one series. Serialising the whole
//! file costs about a second and is the only thing that makes an equality
//! assertion on a gauge mean anything.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use simmer::admin::{self, AdminState};
use simmer::config::Config;
use simmer::quota::store::ReserveRequest;
use simmer::quota::{self, PgQuotaStore};
use simmer::relay::Engine;
use sqlx::PgPool;
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef-default";

const CFG: &str = r#"
server:
  listeners:
    - address: "127.0.0.1:0"
  hostname: simmer.test
  max_message_bytes: 100000
  max_recipients: 1
  max_concurrent_sessions: 16
  allowed_cidrs: ["127.0.0.0/8"]
  timeouts: { command: 5s, data: 5s, session: 60s }
  auth: { allow_insecure_auth: true }
database: { url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }
admin: { listen: "127.0.0.1:0", auth_token: "0123456789abcdef-default" }
logging: { level: warn, format: text }
domain_groups:
  - { name: google, domains: ["gmail.com"] }
  - { name: catchall, domains: ["*"] }
senders:
  - { match: "oldbrand.com", match_on: envelope, chain: [warming, overflow] }
default_chain: [overflow]
routes:
  - name: warming
    downstream:
      host: "127.0.0.1"
      port: 2525
      tls: off
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "bounce@newbrand.com" }
    warmup:
      started: "2020-01-01T00:00:00Z"
      schedule:
        default: [3]
        overrides:
          google: [7]
  - name: overflow
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: 2525
      tls: off
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "bounce@established.com" }
"#;

/// Serialise the whole file against the one global recorder.
///
/// `tokio::sync::Mutex` rather than `std::sync::Mutex`: the guard is held across
/// `.await` points, which a `std` guard is not `Send` enough for. There is no
/// poisoning to handle, which is the other reason.
async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// Install the recorder exactly once, however many tests run.
fn handle() -> metrics_exporter_prometheus::PrometheusHandle {
    use std::sync::OnceLock;
    static HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(|| simmer::metrics::install().expect("no recorder installed yet"))
        .clone()
}

fn state(pool: PgPool) -> AdminState {
    let cfg: Arc<Config> = Arc::new(simmer::config::from_str(CFG, "test").expect("valid"));
    let (tls, _) = simmer::downstream::TlsConfigs::load().expect("tls");
    let rewriters = simmer::rewrite::Rewriters::compile(&cfg).expect("templates compile");

    AdminState {
        engine: Engine {
            config: Arc::clone(&cfg),
            tls: Arc::new(tls),
            pools: Arc::new(simmer::downstream::Pool::build(&cfg)),
            quota: Arc::new(PgQuotaStore::new(pool)),
            registry: quota::ReservationRegistry::new(),
            rewriters: Arc::new(rewriters),
            frequency: Arc::new(simmer::frequency::Frequency::with_salt(
                b"test salt".to_vec(),
            )),
            preflight: Arc::new(simmer::preflight::Registry::new()),
        },
        metrics: Some(handle()),
    }
}

/// One sample from a scrape: its label set, and its value as text.
///
/// Parsed rather than string-matched because the exporter renders labels in the
/// order the call site declared them, and asserting on that order would make
/// every test here fail the day a label is added — which is a change to the
/// call site, not to what the series means.
fn samples(body: &str, name: &str) -> Vec<(std::collections::BTreeMap<String, String>, String)> {
    let mut out = Vec::new();

    for line in body.lines() {
        let line = line.trim();
        if line.starts_with('#') || !line.starts_with(name) {
            continue;
        }
        let rest = &line[name.len()..];
        // `simmer_quota_allowance` must not match `simmer_quota_allowance_foo`.
        if !(rest.starts_with('{') || rest.starts_with(' ')) {
            continue;
        }

        let (labels, value) = match rest.split_once('}') {
            Some((labels, value)) => (labels.trim_start_matches('{'), value),
            None => ("", rest),
        };

        let labels = labels
            .split(',')
            .filter(|p| !p.is_empty())
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim_matches('"').to_string()))
            .collect();

        out.push((labels, value.trim().to_string()));
    }

    out
}

/// The value of one series, by name and label set.
#[track_caller]
fn value(body: &str, name: &str, labels: &[(&str, &str)]) -> String {
    let wanted: std::collections::BTreeMap<String, String> = labels
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    samples(body, name)
        .into_iter()
        .find(|(have, _)| *have == wanted)
        .map(|(_, value)| value)
        .unwrap_or_else(|| panic!("no series {name}{labels:?} in:\n{body}"))
}

async fn scrape(state: &AdminState) -> (StatusCode, String) {
    let response = admin::router(state.clone())
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .expect("router responded");
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[sqlx::test]
async fn the_endpoint_renders_prometheus_text_without_a_token(pool: PgPool) {
    let _serialised = exclusive().await;
    // D-055 — a scraper cannot usually carry a credential, and a blind dashboard
    // is its own outage.
    let (status, body) = scrape(&state(pool)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("# HELP") || body.contains("simmer_"),
        "not Prometheus exposition: {body}"
    );
}

#[sqlx::test]
async fn quota_gauges_are_refreshed_from_storage_on_every_scrape(pool: PgPool) {
    let _serialised = exclusive().await;
    // The trap this closes: the gauges were only ever set by a message that
    // relayed, so a route that has sent nothing today would export *yesterday's*
    // numbers under a label set claiming to describe today. Nothing has relayed
    // in this process at all, and the gauges must still be right.
    let state = state(pool.clone());
    let (_, body) = scrape(&state).await;

    let allowance = |group, route| {
        value(
            &body,
            "simmer_quota_allowance",
            &[("domain_group", group), ("route", route)],
        )
    };

    assert_eq!(allowance("google", "warming"), "7", "the override series");
    assert_eq!(allowance("catchall", "warming"), "3", "the default series");
    // §3.1: an overflow route is never quota-limited, and an infinite ceiling
    // says so in a form a dashboard can plot beside the warming routes (D-024).
    assert_eq!(
        allowance("catchall", "overflow").parse::<f64>().unwrap(),
        f64::INFINITY
    );
}

#[sqlx::test]
async fn a_reservation_moves_the_gauges_without_a_relay(pool: PgPool) {
    let _serialised = exclusive().await;
    let state = state(pool.clone());
    let cfg = Arc::clone(&state.engine.config);
    let day = quota::day::for_route(cfg.route("warming").unwrap(), chrono::Utc::now());

    let taken = state
        .engine
        .quota
        .reserve(&ReserveRequest {
            route: "warming".into(),
            domain_group: "google".into(),
            day_index: day,
            allowance: Some(7),
            count: 1,
            correlation_id: "metrics".into(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
        })
        .await
        .expect("reserve");
    let simmer::quota::Reserved::Taken(taken) = taken else {
        panic!("expected a reservation");
    };

    let labels = [("domain_group", "google"), ("route", "warming")];

    let (_, body) = scrape(&state).await;
    assert_eq!(
        value(&body, "simmer_quota_reserved", &labels),
        "1",
        "reserved should be visible before the send completes"
    );

    state
        .engine
        .quota
        .commit(&taken, &[])
        .await
        .expect("commit");

    let (_, body) = scrape(&state).await;
    assert_eq!(value(&body, "simmer_quota_committed", &labels), "1");
    assert_eq!(value(&body, "simmer_quota_reserved", &labels), "0");
}

#[sqlx::test]
async fn an_allowance_override_is_what_the_gauge_reports(pool: PgPool) {
    let _serialised = exclusive().await;
    // §7.4 enforces the override, so the gauge has to show it. A dashboard
    // showing the schedule while the reservation protocol honours something else
    // is the D-026 trap in the place nobody checks.
    let state = state(pool.clone());
    let request = Request::post("/routes/warming/allowance")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"domain_group":"google","allowance":99}"#.to_string(),
        ))
        .unwrap();
    let response = admin::router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let (_, body) = scrape(&state).await;
    assert_eq!(
        value(
            &body,
            "simmer_quota_allowance",
            &[("domain_group", "google"), ("route", "warming")]
        ),
        "99"
    );
}

#[sqlx::test]
async fn a_route_that_has_not_started_reports_a_ceiling_of_zero_not_infinity(pool: PgPool) {
    let _serialised = exclusive().await;
    // The two available wrong answers here are opposites. A not-started route has
    // no `Allowance` column, and passing that through as "no ceiling" would export
    // +Inf — which reads on a dashboard as *unlimited* for a route that cannot
    // send at all. `simmer_warmup_day` going negative is what says why.
    let cfg: Arc<Config> = Arc::new(
        simmer::config::from_str(
            &CFG.replace("2020-01-01T00:00:00Z", "2099-01-01T00:00:00Z"),
            "test",
        )
        .expect("valid"),
    );
    let (tls, _) = simmer::downstream::TlsConfigs::load().expect("tls");
    let rewriters = simmer::rewrite::Rewriters::compile(&cfg).expect("templates compile");
    let state = AdminState {
        engine: Engine {
            config: Arc::clone(&cfg),
            tls: Arc::new(tls),
            pools: Arc::new(simmer::downstream::Pool::build(&cfg)),
            quota: Arc::new(PgQuotaStore::new(pool)),
            registry: quota::ReservationRegistry::new(),
            rewriters: Arc::new(rewriters),
            frequency: Arc::new(simmer::frequency::Frequency::with_salt(
                b"test salt".to_vec(),
            )),
            preflight: Arc::new(simmer::preflight::Registry::new()),
        },
        metrics: Some(handle()),
    };

    let (_, body) = scrape(&state).await;

    assert_eq!(
        value(
            &body,
            "simmer_quota_allowance",
            &[("domain_group", "google"), ("route", "warming")]
        ),
        "0"
    );
    assert!(
        value(&body, "simmer_warmup_day", &[("route", "warming")])
            .parse::<f64>()
            .unwrap()
            < 0.0,
        "the day index is what says the route has not begun: {body}"
    );
    // The overflow route in the same configuration is unaffected.
    assert_eq!(
        value(
            &body,
            "simmer_quota_allowance",
            &[("domain_group", "google"), ("route", "overflow")]
        )
        .parse::<f64>()
        .unwrap(),
        f64::INFINITY
    );
}

#[sqlx::test]
async fn a_paused_route_is_visible_on_a_dashboard_with_no_traffic(pool: PgPool) {
    let _serialised = exclusive().await;
    // `simmer_route_skipped_total{reason="paused"}` only moves when a message is
    // steered past the route. A route paused three weeks ago on a chain nothing
    // reaches leaves no other trace, and that is the state somebody eventually
    // goes looking for. D-056.
    let state = state(pool);

    let (_, body) = scrape(&state).await;
    assert_eq!(
        value(&body, "simmer_route_paused", &[("route", "warming")]),
        "0"
    );

    let request = Request::post("/routes/warming/pause")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let response = admin::router(state.clone()).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let (_, body) = scrape(&state).await;
    assert_eq!(
        value(&body, "simmer_route_paused", &[("route", "warming")]),
        "1"
    );
    assert_eq!(
        value(&body, "simmer_route_paused", &[("route", "overflow")]),
        "0",
        "only the route that was paused"
    );
}

#[sqlx::test]
async fn the_warmup_day_gauge_is_exported_for_every_route(pool: PgPool) {
    let _serialised = exclusive().await;
    let (_, body) = scrape(&state(pool)).await;
    assert!(!value(&body, "simmer_warmup_day", &[("route", "warming")]).is_empty());
    assert!(!value(&body, "simmer_warmup_day", &[("route", "overflow")]).is_empty());
}

#[sqlx::test]
async fn the_pool_gauges_are_exported_for_a_route_that_has_never_sent(pool: PgPool) {
    // §9.1's `simmer_pool_connections{route,state}`, and the D-056 rule that
    // decides where it is written. A gauge set by the relay would publish no
    // series at all here — no message has ever been sent in this process — and
    // an absent series reads on a dashboard as a route that does not exist.
    // Zero is the answer, and it comes off the pool at scrape time.
    let _serialised = exclusive().await;
    let (_, body) = scrape(&state(pool)).await;

    for route in ["warming", "overflow"] {
        for state_label in ["idle", "active"] {
            assert_eq!(
                value(
                    &body,
                    "simmer_pool_connections",
                    &[("route", route), ("state", state_label)]
                ),
                "0",
                "{route}/{state_label} in:\n{body}"
            );
        }
    }
}

#[sqlx::test]
async fn counters_recorded_through_the_named_functions_reach_the_scrape(pool: PgPool) {
    let _serialised = exclusive().await;
    // The D-021 claim, finally testable: the call sites written in phases 2–6
    // against a null recorder produce real series once one is installed, with no
    // change to any of them.
    let state = state(pool);

    simmer::metrics::message(
        "warming",
        "google",
        simmer::metrics::MessageResult::Delivered,
    );
    simmer::metrics::route_skipped("warming", "quota");
    simmer::metrics::downstream_latency("warming", 0.42);
    simmer::metrics::body_rewrite_skipped("warming", "signed");
    simmer::metrics::recipient_events_evicted(3);

    let (_, body) = scrape(&state).await;

    // D-054 — the domain_group label must be real, not phase 2's placeholder.
    assert_eq!(
        value(
            &body,
            "simmer_messages_total",
            &[
                ("domain_group", "google"),
                ("result", "delivered"),
                ("route", "warming")
            ]
        ),
        "1"
    );
    assert!(!body.contains(r#"domain_group="-""#), "{body}");

    assert_eq!(
        value(
            &body,
            "simmer_route_skipped_total",
            &[("reason", "quota"), ("route", "warming")]
        ),
        "1"
    );
    assert_eq!(
        value(
            &body,
            "simmer_body_rewrite_skipped_total",
            &[("reason", "signed"), ("route", "warming")]
        ),
        "1"
    );
    // §9.1 says histogram, and a histogram has buckets — the exporter's default
    // for a histogram is a summary with in-process quantiles, which cannot be
    // aggregated across instances.
    assert!(
        body.contains("simmer_downstream_latency_seconds_bucket"),
        "{body}"
    );

    // §7.3 — deliberately unlabelled. A per-route label would accumulate exactly
    // what the hashing exists to avoid.
    assert!(
        body.contains("simmer_recipient_events_evicted_total"),
        "{body}"
    );
    assert!(
        !body.contains("simmer_recipient_events_evicted_total{"),
        "this counter must carry no labels: {body}"
    );
}

#[sqlx::test]
async fn the_metrics_carry_help_text(pool: PgPool) {
    let _serialised = exclusive().await;
    // The audience for a metric name at 3am is not the person who chose it.
    let state = state(pool);
    simmer::metrics::unmatched_sender("example.com");

    let (_, body) = scrape(&state).await;
    assert!(
        body.contains("# HELP simmer_unmatched_sender_total"),
        "{body}"
    );
    // §14.2's caveat, in the exposition itself.
    assert!(body.contains("ALERT ON THIS"), "{body}");
}

#[sqlx::test]
async fn no_scrape_exposes_a_recipient(pool: PgPool) {
    let _serialised = exclusive().await;
    // §7.3, applied to the one place where a high-cardinality label would be
    // easiest to add and hardest to notice.
    let state = state(pool);
    simmer::metrics::recipient_events_evicted(9);

    let (_, body) = scrape(&state).await;
    assert!(!body.contains('@'), "{body}");
    assert!(!body.contains("recipient_hash"), "{body}");
}
