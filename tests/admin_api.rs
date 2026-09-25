//! §9 against the real router and real Postgres.
//!
//! These drive `admin::router` in process through `tower::ServiceExt::oneshot`,
//! so the middleware stack, the extractors and the status codes are the shipped
//! ones rather than a reimplementation. Storage is a real `PgQuotaStore` on a
//! `#[sqlx::test]` database, because the interesting half of §9.2 is which of
//! two disagreeing numbers gets reported and the disagreement only exists once
//! a row does.
//!
//! Three things here are worth more than the rest:
//!
//! - `dry_run_agrees_with_the_real_walk_*` — §9.4 is only useful if it answers
//!   about the relay rather than about itself.
//! - `a_dry_run_reserves_nothing` and `a_dry_run_creates_no_row` — §9.4 "sends
//!   nothing and takes no reservation", asserted against the tables.
//! - `no_endpoint_exposes_a_recipient` — §7.3's reason for hashing, applied to
//!   the control plane.

// Postgres-backed: the storage layer under test is `PgQuotaStore`. The SQL
// Server build runs the backend-neutral suite instead (tests/store_mssql.rs,
// D-084).
#![cfg(feature = "postgres")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use simmer::admin::{self, AdminState};
use simmer::config::Config;
use simmer::quota::store::{QuotaStore, ReserveRequest};
use simmer::quota::{self, PgQuotaStore};
use simmer::relay::Engine;
use sqlx::PgPool;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

const TOKEN: &str = "0123456789abcdef-default";
const ONCALL: &str = "0123456789abcdef-oncall";

/// A warming route capped at 3/day (1/day for Google), then an uncapped
/// overflow. `warmup.started` is far in the past, so the day index is large and
/// stable.
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
admin:
  listen: "127.0.0.1:0"
  auth_token: "0123456789abcdef-default"
  tokens:
    - { name: oncall, token: "0123456789abcdef-oncall" }
logging: { level: warn, format: text }
default_ramp: main
ramps:
 main:
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
      timeouts: { connect: 2s, command: 2s, data: 2s }
    identity:
      envelope_from: "bounce@newbrand.com"
      set_headers:
        From: "New Brand <hello@newbrand.com>"
      body_rewrites:
        - { pattern: "https://oldbrand\\.com", replacement: "https://newbrand.com" }
    warmup:
      started: "2020-01-01T00:00:00Z"
      schedule:
        default: [3]
        overrides:
          google: [1]
  - name: overflow
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: 2525
      tls: off
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
      timeouts: { connect: 2s, command: 2s, data: 2s }
    identity: { envelope_from: "bounce@established.com" }
"#;

fn config(extra: &str) -> Config {
    let yaml = format!("{CFG}{extra}");
    simmer::config::from_str(&yaml, "test").expect("fixture is valid")
}

fn state_from(pool: PgPool, cfg: Config) -> AdminState {
    let cfg = Arc::new(cfg);
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
            // A fixed salt, so §7.3's keys never depend on what the database
            // happened to mint for this test's fresh instance.
            frequency: Arc::new(simmer::frequency::Frequency::with_salt(
                b"test salt".to_vec(),
            )),
            preflight: Arc::new(simmer::preflight::Registry::new()),
            groups: Arc::new(simmer::routing::domain_group::Grouper::literal()),
            capture: None,
        },
        // No recorder: `metrics` allows exactly one per process and installing it
        // here would fail in whichever test ran second.
        metrics: None,
        sessions: None,
        db: None,
    }
}

fn state(pool: PgPool) -> AdminState {
    state_from(pool, config(""))
}

fn store(state: &AdminState) -> Arc<dyn QuotaStore> {
    Arc::clone(&state.engine.quota)
}

fn today(cfg: &Config, route: &str) -> i64 {
    quota::day::for_route(cfg.default_ramp().route(route).expect("route"), Utc::now())
}

// ---------------------------------------------------------------------------
// driving the router
// ---------------------------------------------------------------------------

struct Response {
    status: StatusCode,
    body: String,
}

impl Response {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.body))
    }
}

async fn send(state: &AdminState, request: Request<Body>) -> Response {
    let response = admin::router(state.clone())
        .oneshot(request)
        .await
        .expect("router responded");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();

    Response {
        status,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn get(state: &AdminState, uri: &str, token: Option<&str>) -> Response {
    let mut builder = Request::get(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    send(state, builder.body(Body::empty()).expect("request")).await
}

async fn post(state: &AdminState, uri: &str, token: Option<&str>, body: &str) -> Response {
    let mut builder = Request::post(uri).header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    send(
        state,
        builder.body(Body::from(body.to_string())).expect("request"),
    )
    .await
}

/// Find one route in a `/routes` document.
fn route_of(doc: &serde_json::Value, name: &str) -> serde_json::Value {
    doc["routes"]
        .as_array()
        .expect("routes array")
        .iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("no route '{name}' in {doc}"))
        .clone()
}

fn group_of(route: &serde_json::Value, name: &str) -> serde_json::Value {
    route["groups"]
        .as_array()
        .expect("groups array")
        .iter()
        .find(|g| g["domain_group"] == name)
        .unwrap_or_else(|| panic!("no group '{name}' in {route}"))
        .clone()
}

// ---------------------------------------------------------------------------
// §9.3 authentication (D-055)
// ---------------------------------------------------------------------------

/// Every path that must refuse an anonymous request, as `(method, uri, body)`.
const PROTECTED: &[(&str, &str, &str)] = &[
    ("GET", "/routes", ""),
    ("GET", "/routes/warming", ""),
    ("GET", "/quota", ""),
    ("POST", "/routes/warming/pause", ""),
    ("POST", "/routes/warming/resume", ""),
    ("POST", "/routes/warming/graduate", ""),
    (
        "POST",
        "/routes/warming/allowance",
        r#"{"domain_group":"google","allowance":5}"#,
    ),
    (
        "POST",
        "/quota/reset",
        r#"{"route":"warming","domain_group":"google","confirm":"reset"}"#,
    ),
    (
        "POST",
        "/dryrun",
        r#"{"envelope_from":"a@oldbrand.com","recipients":["b@gmail.com"]}"#,
    ),
];

#[sqlx::test]
async fn every_protected_endpoint_refuses_an_anonymous_request(pool: PgPool) {
    let state = state(pool);

    for (method, uri, body) in PROTECTED {
        let response = match *method {
            "GET" => get(&state, uri, None).await,
            _ => post(&state, uri, None, body).await,
        };
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} should require a token"
        );
        assert_eq!(response.json()["error"], "unauthorised");
    }
}

#[sqlx::test]
async fn every_protected_endpoint_refuses_a_wrong_token(pool: PgPool) {
    let state = state(pool);

    for (method, uri, body) in PROTECTED {
        let response = match *method {
            "GET" => get(&state, uri, Some("not-the-token")).await,
            _ => post(&state, uri, Some("not-the-token"), body).await,
        };
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} accepted a wrong token"
        );
    }
}

#[sqlx::test]
async fn health_and_metrics_stay_open(pool: PgPool) {
    // An orchestrator's probe and a Prometheus scrape cannot usually carry a
    // credential, and a blind dashboard is its own outage (D-055). `/metrics`
    // has to be switched on to exist at all (D-093).
    let state = state_from(pool, metrics_on());

    assert_eq!(get(&state, "/health", None).await.status, StatusCode::OK);
    assert_eq!(
        get(&state, "/healthcheck", None).await.status,
        StatusCode::OK
    );
    // 503 because this state has no recorder installed, not 401 — the point is
    // that authentication did not refuse it.
    assert_eq!(
        get(&state, "/metrics", None).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

/// The fixture with `admin.metrics: true`.
fn metrics_on() -> Config {
    let yaml = CFG.replace(
        "  auth_token: \"0123456789abcdef-default\"\n",
        "  auth_token: \"0123456789abcdef-default\"\n  metrics: true\n",
    );
    assert_ne!(yaml, CFG, "the fixture's admin block moved");
    simmer::config::from_str(&yaml, "test").expect("fixture is valid")
}

#[sqlx::test]
async fn metrics_is_not_served_unless_enabled(pool: PgPool) {
    // D-093: off by default, and off means absent — 404, with or without a
    // token — not a 503 that reads like a fault.
    let state = state(pool);
    assert!(!state.engine.config.admin.metrics().enabled);
    assert_eq!(
        get(&state, "/metrics", None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&state, "/metrics", Some("0123456789abcdef-default"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test]
async fn a_401_carries_a_challenge_and_no_hint(pool: PgPool) {
    let state = state(pool);
    let response = admin::router(state.clone())
        .oneshot(Request::get("/routes").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().contains_key("www-authenticate"));

    // A missing token and a wrong token must be indistinguishable to the client.
    let missing = get(&state, "/routes", None).await;
    let wrong = get(&state, "/routes", Some("wrong")).await;
    assert_eq!(missing.body, wrong.body);
}

#[sqlx::test]
async fn a_named_token_is_recorded_as_the_actor(pool: PgPool) {
    // O-11 / D-053 — the whole reason named tokens exist.
    let state = state(pool);

    let response = post(&state, "/routes/warming/pause", Some(ONCALL), "").await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["actor"], "oncall");

    let response = post(&state, "/routes/warming/resume", Some(TOKEN), "").await;
    assert_eq!(response.json()["actor"], "default");
}

// ---------------------------------------------------------------------------
// §9.2 read
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn routes_reports_every_route_and_every_group(pool: PgPool) {
    let state = state(pool);
    let doc = get(&state, "/routes", Some(TOKEN)).await.json();

    assert_eq!(doc["routes"].as_array().unwrap().len(), 2);

    let warming = route_of(&doc, "warming");
    assert_eq!(warming["status"], "active");
    assert_eq!(warming["overflow"], false);
    assert_eq!(group_of(&warming, "google")["scheduled"], 1);
    assert_eq!(group_of(&warming, "catchall")["scheduled"], 3);
    // Nothing has been sent, so no row exists and the numbers are what the first
    // message would write.
    assert_eq!(group_of(&warming, "google")["row_exists"], false);
    assert_eq!(group_of(&warming, "google")["headroom"], 1);

    let overflow = route_of(&doc, "overflow");
    assert_eq!(overflow["overflow"], true);
    assert!(group_of(&overflow, "catchall")["headroom"].is_null());
}

#[sqlx::test]
async fn routes_reports_the_pool_statistics_9_2_asks_for(pool: PgPool) {
    // §9.2's last clause, unanswered until phase 10. The route has sent nothing,
    // so the honest answer is its configured ceiling and zeros against it —
    // rather than the `null` this field carried while there was no pool.
    let state = state(pool);
    let doc = get(&state, "/routes", Some(TOKEN)).await.json();

    let warming = route_of(&doc, "warming")["pool"].clone();
    assert!(
        !warming.is_null(),
        "a configured route always has a pool: {warming}"
    );
    assert_eq!(warming["max_connections"], 1);
    assert_eq!(warming["idle"], 0);
    assert_eq!(warming["active"], 0);
    assert_eq!(warming["opened"], 0);
    assert_eq!(warming["reused"], 0);
    assert_eq!(warming["retired"], 0);
    assert_eq!(warming["discarded"], 0);

    // And the overflow route, whose pool is configured separately — the bound is
    // per route because the downstreams are different providers.
    assert!(!route_of(&doc, "overflow")["pool"].is_null());
}

#[sqlx::test]
async fn a_reservation_shows_up_as_reserved_and_reduces_headroom(pool: PgPool) {
    let state = state(pool.clone());
    let cfg = state.engine.config.clone();
    let day = today(&cfg, "warming");

    store(&state)
        .reserve(&ReserveRequest {
            ramp: "main".into(),
            route: "warming".into(),
            domain_group: "catchall".into(),
            day_index: day,
            allowance: Some(3),
            count: 1,
            correlation_id: "test".into(),
            expires_at: Utc::now() + chrono::Duration::minutes(10),
            over_cap: false,
        })
        .await
        .expect("reserved");

    let warming = route_of(&get(&state, "/routes", Some(TOKEN)).await.json(), "warming");
    let group = group_of(&warming, "catchall");

    assert_eq!(group["row_exists"], true);
    assert_eq!(group["reserved"], 1);
    assert_eq!(group["committed"], 0);
    assert_eq!(group["headroom"], 2, "reserved counts against headroom");
}

#[sqlx::test]
async fn the_read_api_reports_the_row_when_the_schedule_has_moved_under_it(pool: PgPool) {
    // D-026, end to end: the row was written under the old schedule and §7.4
    // enforces it. Reporting the configured 3 while the reservation protocol
    // refuses at 1 would send an operator hunting a bug in the ramp.
    let state = state(pool.clone());
    let day = today(&state.engine.config, "warming");

    // A row created under a ceiling of 1...
    store(&state)
        .reserve(&ReserveRequest {
            ramp: "main".into(),
            route: "warming".into(),
            domain_group: "catchall".into(),
            day_index: day,
            allowance: Some(1),
            count: 1,
            correlation_id: "test".into(),
            expires_at: Utc::now() + chrono::Duration::minutes(10),
            over_cap: false,
        })
        .await
        .expect("reserved");

    // ...and a configuration that now says 3.
    let group = group_of(
        &route_of(&get(&state, "/routes", Some(TOKEN)).await.json(), "warming"),
        "catchall",
    );

    assert_eq!(group["scheduled"], 3, "what the configuration says");
    assert_eq!(
        group["allowance"], 1,
        "what the row says, and what §7.4 enforces"
    );
    assert_eq!(group["drift"], true);
    assert_eq!(group["headroom"], 0);
}

#[sqlx::test]
async fn a_single_route_reads_the_same_as_its_entry_in_the_list(pool: PgPool) {
    let state = state(pool);
    let from_list = route_of(&get(&state, "/routes", Some(TOKEN)).await.json(), "warming");
    let alone = get(&state, "/routes/warming", Some(TOKEN)).await.json();

    // `generated_at` differs by microseconds between the two calls; everything
    // that describes the route must not.
    assert_eq!(from_list["groups"], alone["groups"]);
    assert_eq!(from_list["status"], alone["status"]);
    assert_eq!(from_list["day_index"], alone["day_index"]);
}

#[sqlx::test]
async fn an_unknown_route_is_a_404(pool: PgPool) {
    let state = state(pool);
    let response = get(&state, "/routes/nonexistent", Some(TOKEN)).await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(response.json()["error"], "not_found");
}

#[sqlx::test]
async fn quota_filters_independently_on_route_and_group(pool: PgPool) {
    let state = state(pool);

    let all = get(&state, "/quota", Some(TOKEN)).await.json();
    assert_eq!(all["windows"].as_array().unwrap().len(), 4);

    let by_route = get(&state, "/quota?route=warming", Some(TOKEN))
        .await
        .json();
    assert_eq!(by_route["windows"].as_array().unwrap().len(), 2);

    // "What is every route doing for Google today", which is how a
    // deliverability problem usually starts.
    let by_group = get(&state, "/quota?group=google", Some(TOKEN)).await.json();
    assert_eq!(by_group["windows"].as_array().unwrap().len(), 2);

    let both = get(&state, "/quota?route=warming&group=google", Some(TOKEN))
        .await
        .json();
    let windows = both["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["route"], "warming");
    assert_eq!(windows[0]["domain_group"], "google");
    assert!(
        windows[0]["day_ends_at"].is_string(),
        "§9.3's override expiry"
    );
}

#[sqlx::test]
async fn quota_404s_on_a_filter_that_names_nothing(pool: PgPool) {
    // An empty result and a typo are different answers, and defaulting to the
    // first would let `?group=gogle` read as "nothing configured for Google".
    let state = state(pool);
    assert_eq!(
        get(&state, "/quota?group=gogle", Some(TOKEN)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&state, "/quota?route=warmingg", Some(TOKEN))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

// ---------------------------------------------------------------------------
// §9.3 write
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn pause_and_resume_round_trip_through_storage(pool: PgPool) {
    let state = state(pool);

    let paused = post(&state, "/routes/warming/pause", Some(TOKEN), "").await;
    assert_eq!(paused.status, StatusCode::OK);
    assert_eq!(paused.json()["paused"], true);
    assert_eq!(paused.json()["previous"], false);

    let doc = get(&state, "/routes/warming", Some(TOKEN)).await.json();
    assert_eq!(doc["paused"], true);
    assert_eq!(doc["status"], "paused");

    let resumed = post(&state, "/routes/warming/resume", Some(TOKEN), "").await;
    assert_eq!(resumed.json()["paused"], false);
    assert_eq!(resumed.json()["previous"], true);

    let doc = get(&state, "/routes/warming", Some(TOKEN)).await.json();
    assert_eq!(doc["status"], "active");
}

#[sqlx::test]
async fn a_paused_route_is_skipped_by_the_real_chain_walk(pool: PgPool) {
    // The mutation is only worth anything if the message path sees it. §9.3's
    // "without a restart" is this assertion.
    let state = state(pool);
    post(&state, "/routes/warming/pause", Some(TOKEN), "")
        .await
        .json();

    let mut evaluation = Vec::new();
    let walk = simmer::routing::chain::walk_and_reserve(
        state.engine.config.default_ramp(),
        &state.engine.groups,
        &state.engine.config.dot_insensitive_domains,
        &store(&state),
        &state.engine.frequency,
        &state.engine.preflight,
        &["warming".to_string(), "overflow".to_string()],
        None,
        &["someone@gmail.com".to_string()],
        "test",
        &mut evaluation,
    )
    .await
    .expect("walk");

    assert!(matches!(
        walk,
        simmer::routing::chain::Walk::Selected(_) // it steered, rather than failing
    ));
    assert_eq!(
        simmer::routing::chain::render(&evaluation),
        "warming=paused,overflow=selected"
    );
}

#[sqlx::test]
async fn graduate_pins_the_route_and_can_be_reversed(pool: PgPool) {
    let state = state(pool);

    let response = post(&state, "/routes/warming/graduate", Some(TOKEN), "").await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.json()["graduated"],
        true,
        "an absent body means graduate"
    );

    assert_eq!(
        get(&state, "/routes/warming", Some(TOKEN)).await.json()["graduated"],
        true
    );

    let response = post(
        &state,
        "/routes/warming/graduate",
        Some(TOKEN),
        r#"{"graduated": false}"#,
    )
    .await;
    assert_eq!(response.json()["graduated"], false);
    assert_eq!(response.json()["previous"], true);
}

#[sqlx::test]
async fn an_overflow_route_cannot_be_graduated(pool: PgPool) {
    // §3.1: it carries no warm-up schedule, so there is no final value to pin
    // it to. Storing the flag would report success for a no-op.
    let state = state(pool);
    let response = post(&state, "/routes/overflow/graduate", Some(TOKEN), "").await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("overflow"));
}

#[sqlx::test]
async fn an_allowance_override_wins_over_the_schedule_and_is_visible_immediately(pool: PgPool) {
    let state = state(pool);

    let response = post(
        &state,
        "/routes/warming/allowance",
        Some(ONCALL),
        r#"{"domain_group":"google","allowance":50}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["allowance"], 50);
    assert!(response.json()["previous"].is_null());
    assert!(
        response.json()["expires_at"].is_string(),
        "§9.3: expires at the next day boundary"
    );

    let group = group_of(
        &get(&state, "/routes/warming", Some(TOKEN)).await.json(),
        "google",
    );
    assert_eq!(group["scheduled"], 1, "the schedule is unchanged");
    assert_eq!(group["override"], 50);
    assert_eq!(group["headroom"], 50, "the override is what §7.4 enforces");
    assert_eq!(group["drift"], false, "a deliberate override is not drift");
}

#[sqlx::test]
async fn an_override_can_be_cleared_with_null(pool: PgPool) {
    let state = state(pool);

    post(
        &state,
        "/routes/warming/allowance",
        Some(TOKEN),
        r#"{"domain_group":"google","allowance":50}"#,
    )
    .await;
    let cleared = post(
        &state,
        "/routes/warming/allowance",
        Some(TOKEN),
        r#"{"domain_group":"google","allowance":null}"#,
    )
    .await;

    assert_eq!(cleared.status, StatusCode::OK);
    assert_eq!(cleared.json()["previous"], 50);

    let group = group_of(
        &get(&state, "/routes/warming", Some(TOKEN)).await.json(),
        "google",
    );
    assert!(group["override"].is_null());
    assert_eq!(group["headroom"], 1, "back to the schedule's own ceiling");
}

#[sqlx::test]
async fn an_override_takes_effect_on_the_next_reservation(pool: PgPool) {
    // The point of the endpoint. The schedule says 1/day for Google; after the
    // override three reservations must all succeed.
    let state = state(pool);
    let day = today(&state.engine.config, "warming");

    post(
        &state,
        "/routes/warming/allowance",
        Some(TOKEN),
        r#"{"domain_group":"google","allowance":3}"#,
    )
    .await;

    for i in 0..3 {
        let taken = store(&state)
            .reserve(&ReserveRequest {
                ramp: "main".into(),
                route: "warming".into(),
                domain_group: "google".into(),
                day_index: day,
                allowance: Some(1),
                count: 1,
                correlation_id: format!("test-{i}"),
                expires_at: Utc::now() + chrono::Duration::minutes(10),
                over_cap: false,
            })
            .await
            .expect("store");
        assert!(
            matches!(taken, simmer::quota::Reserved::Taken(_)),
            "reservation {i} should have been granted under the override"
        );
    }
}

#[sqlx::test]
async fn a_negative_allowance_is_refused(pool: PgPool) {
    let state = state(pool);
    let response = post(
        &state,
        "/routes/warming/allowance",
        Some(TOKEN),
        r#"{"domain_group":"google","allowance":-1}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn an_unknown_domain_group_is_a_404(pool: PgPool) {
    let state = state(pool);
    let response = post(
        &state,
        "/routes/warming/allowance",
        Some(TOKEN),
        r#"{"domain_group":"nope","allowance":1}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// §14.1 — a mutation that makes every message 451
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn an_allowance_of_zero_is_allowed_and_warned_about(pool: PgPool) {
    // Both chains end in the overflow route, which is uncapped — so zeroing the
    // warming route alone exhausts nothing, and the warning has to survive that.
    // Zero the group on both routes and every chain has nothing left for Google.
    let state = state(pool);

    let response = post(
        &state,
        "/routes/overflow/allowance",
        Some(TOKEN),
        r#"{"domain_group":"google","allowance":0}"#,
    )
    .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "zero is a legitimate value"
    );

    let response = post(
        &state,
        "/routes/warming/allowance",
        Some(ONCALL),
        r#"{"domain_group":"google","allowance":0}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);

    let warnings = response.json()["warnings"]
        .as_array()
        .expect("warnings array")
        .clone();
    assert!(
        !warnings.is_empty(),
        "zeroing every route for a group must be reported: {}",
        response.body
    );
    let text = warnings
        .iter()
        .map(|w| w.as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(text.contains("google"), "{text}");
    assert!(
        text.contains("451"),
        "the warning must say what clients will see: {text}"
    );
}

#[sqlx::test]
async fn pausing_every_route_in_a_chain_is_warned_about(pool: PgPool) {
    let state = state(pool);

    let first = post(&state, "/routes/warming/pause", Some(TOKEN), "").await;
    assert!(
        first.json()["warnings"].as_array().unwrap().is_empty(),
        "the overflow route is still eligible, so nothing is exhausted yet"
    );

    let second = post(&state, "/routes/overflow/pause", Some(TOKEN), "").await;
    let warnings = second.json()["warnings"].as_array().unwrap().clone();
    assert!(
        !warnings.is_empty(),
        "every route in every chain is now paused: {}",
        second.body
    );
}

#[sqlx::test]
async fn an_ordinary_mutation_produces_no_warnings(pool: PgPool) {
    // The warning has to be rare to be read.
    let state = state(pool);
    let response = post(
        &state,
        "/routes/warming/allowance",
        Some(TOKEN),
        r#"{"domain_group":"google","allowance":10}"#,
    )
    .await;
    assert!(response.json()["warnings"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// §9.3 quota reset
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn a_reset_without_the_confirmation_is_refused(pool: PgPool) {
    let state = state(pool);
    let response = post(
        &state,
        "/quota/reset",
        Some(TOKEN),
        r#"{"route":"warming","domain_group":"google","confirm":"yes"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("confirm"));
}

#[sqlx::test]
async fn a_reset_zeroes_committed_and_keeps_live_reservations(pool: PgPool) {
    // The arithmetic that matters: an in-flight send still owns its headroom.
    // Zeroing `reserved` would hand it out twice and overshoot the ceiling this
    // whole service exists to hold.
    let state = state(pool.clone());
    let day = today(&state.engine.config, "warming");

    // One committed message...
    let first = store(&state)
        .reserve(&ReserveRequest {
            ramp: "main".into(),
            route: "warming".into(),
            domain_group: "catchall".into(),
            day_index: day,
            allowance: Some(3),
            count: 1,
            correlation_id: "committed".into(),
            expires_at: Utc::now() + chrono::Duration::minutes(10),
            over_cap: false,
        })
        .await
        .expect("reserve");
    let simmer::quota::Reserved::Taken(first) = first else {
        panic!("expected a reservation");
    };
    store(&state).commit(&first, &[]).await.expect("commit");

    // ...and one still in flight.
    store(&state)
        .reserve(&ReserveRequest {
            ramp: "main".into(),
            route: "warming".into(),
            domain_group: "catchall".into(),
            day_index: day,
            allowance: Some(3),
            count: 1,
            correlation_id: "in-flight".into(),
            expires_at: Utc::now() + chrono::Duration::minutes(10),
            over_cap: false,
        })
        .await
        .expect("reserve");

    let response = post(
        &state,
        "/quota/reset",
        Some(ONCALL),
        r#"{"route":"warming","domain_group":"catchall","confirm":"reset"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["committed_before"], 1);
    assert_eq!(response.json()["reserved_before"], 1);
    assert_eq!(
        response.json()["reserved_after"],
        1,
        "the in-flight reservation still owns its slot"
    );

    let usage = store(&state)
        .usage("main", "warming", "catchall", day)
        .await
        .expect("usage");
    assert_eq!(usage.committed, 0);
    assert_eq!(usage.reserved, 1);
}

#[sqlx::test]
async fn resetting_a_row_that_does_not_exist_is_a_no_op_rather_than_a_404(pool: PgPool) {
    // The operator's intent — "this counter should be zero" — is already true.
    let state = state(pool);
    let response = post(
        &state,
        "/quota/reset",
        Some(TOKEN),
        r#"{"route":"warming","domain_group":"google","confirm":"reset"}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["reset"], false);
}

// ---------------------------------------------------------------------------
// §9.4 dry run
// ---------------------------------------------------------------------------

const DRYRUN: &str = r#"{
    "envelope_from": "app@oldbrand.com",
    "from_header": "Old Brand <app@oldbrand.com>",
    "recipients": ["someone@gmail.com"],
    "body": "hello, see https://oldbrand.com/offer"
}"#;

#[sqlx::test]
async fn a_dry_run_reports_the_matched_rule_the_chain_and_the_rewrite(pool: PgPool) {
    let state = state(pool);
    let doc = post(&state, "/dryrun", Some(TOKEN), DRYRUN).await.json();

    assert_eq!(doc["matched_rule"]["match"], "oldbrand.com");
    assert_eq!(doc["matched_rule"]["match_on"], "envelope");
    assert_eq!(doc["chain"], serde_json::json!(["warming", "overflow"]));

    let outcome = &doc["recipients"][0];
    assert_eq!(outcome["domain_group"], "google");
    assert_eq!(outcome["selected"], "warming");
    assert!(outcome["would_reply"].as_str().unwrap().starts_with("250"));

    let outbound = &outcome["outbound"];
    assert_eq!(outbound["envelope_from"], "bounce@newbrand.com");

    let from = outbound["headers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "From")
        .expect("a From header");
    assert_eq!(from["value"], "New Brand <hello@newbrand.com>");

    // §6.4 against the sample body.
    assert_eq!(outbound["body_rewrites"][0]["count"], 1);
    assert_eq!(outbound["body_changed"], true);
}

#[sqlx::test]
async fn a_from_header_with_a_display_name_still_matches_its_rule(pool: PgPool) {
    // §5.4 matches on the address; the relay only ever sees one, because the
    // session parses the header block before building `Senders`. §9.4 takes "a
    // `From:` header value", which is the display-name form an operator pastes.
    // Passing it through raw made every domain rule miss and reported a
    // fall-through to `default_chain` that would not happen — found by driving
    // this endpoint against the shipped configuration.
    let state = state_from(
        pool,
        simmer::config::from_str(
            &CFG.replace("match_on: envelope", "match_on: from_header"),
            "test",
        )
        .expect("fixture is valid"),
    );

    for from in [
        "Old Brand <app@oldbrand.com>",
        "app@oldbrand.com",
        "<app@oldbrand.com>",
        "\"Brand, Old\" <app@oldbrand.com>",
    ] {
        let body = format!(
            r#"{{"envelope_from":null,"from_header":{},"recipients":["someone@gmail.com"]}}"#,
            serde_json::to_string(from).unwrap()
        );
        let doc = post(&state, "/dryrun", Some(TOKEN), &body).await.json();
        assert_eq!(
            doc["matched_rule"]["match"], "oldbrand.com",
            "'{from}' should match the domain rule, got {doc}"
        );
    }
}

#[sqlx::test]
async fn a_dry_run_reports_a_pattern_that_matched_nothing(pool: PgPool) {
    // The most useful thing this endpoint can say: "your rewrite is not firing".
    let state = state(pool);
    let doc = post(
        &state,
        "/dryrun",
        Some(TOKEN),
        r#"{"envelope_from":"app@oldbrand.com","recipients":["someone@gmail.com"],
            "body":"nothing to rewrite here"}"#,
    )
    .await
    .json();

    let rewrites = &doc["recipients"][0]["outbound"]["body_rewrites"];
    assert_eq!(
        rewrites.as_array().unwrap().len(),
        1,
        "every rule is reported"
    );
    assert_eq!(rewrites[0]["count"], 0);
    assert_eq!(doc["recipients"][0]["outbound"]["body_changed"], false);
}

#[sqlx::test]
async fn a_dry_run_explains_each_skip(pool: PgPool) {
    let state = state(pool);
    post(&state, "/routes/warming/pause", Some(TOKEN), "").await;

    let doc = post(&state, "/dryrun", Some(TOKEN), DRYRUN).await.json();
    let evaluation = &doc["recipients"][0]["evaluation"];

    assert_eq!(evaluation[0]["route"], "warming");
    assert_eq!(evaluation[0]["outcome"], "skipped");
    assert_eq!(evaluation[0]["reason"], "paused");
    assert!(
        evaluation[0]["explanation"]
            .as_str()
            .unwrap()
            .contains("paused"),
        "the reason is the product"
    );
    assert_eq!(evaluation[1]["outcome"], "selected");
    assert_eq!(doc["recipients"][0]["selected"], "overflow");
}

#[sqlx::test]
async fn a_dry_run_reserves_nothing(pool: PgPool) {
    // §9.4: "It sends nothing and takes no reservation."
    let state = state(pool.clone());

    for _ in 0..5 {
        post(&state, "/dryrun", Some(TOKEN), DRYRUN).await;
    }

    let reservations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quota_reservation")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(reservations, 0);
}

#[sqlx::test]
async fn a_dry_run_creates_no_quota_row(pool: PgPool) {
    // Stronger than "no reservation": a dry run that inserted a row would fix
    // today's ceiling at whatever the schedule said when somebody tested a
    // configuration, because the row is authoritative once written (D-026).
    let state = state(pool.clone());
    post(&state, "/dryrun", Some(TOKEN), DRYRUN).await;

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM quota_usage")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(rows, 0);
}

#[sqlx::test]
async fn a_dry_run_evaluates_each_recipient_independently(pool: PgPool) {
    let state = state(pool);
    let doc = post(
        &state,
        "/dryrun",
        Some(TOKEN),
        r#"{"envelope_from":"app@oldbrand.com",
            "recipients":["a@gmail.com","b@example.org"]}"#,
    )
    .await
    .json();

    let outcomes = doc["recipients"].as_array().unwrap();
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0]["domain_group"], "google");
    assert_eq!(outcomes[1]["domain_group"], "catchall");
    // D-047 — a real transaction carries one.
    assert!(doc["note"].as_str().unwrap().contains("one recipient"));
}

#[sqlx::test]
async fn an_exhausted_chain_reports_the_reply_the_client_would_get(pool: PgPool) {
    let state = state(pool);
    post(&state, "/routes/warming/pause", Some(TOKEN), "").await;
    post(&state, "/routes/overflow/pause", Some(TOKEN), "").await;

    let doc = post(&state, "/dryrun", Some(TOKEN), DRYRUN).await.json();
    let outcome = &doc["recipients"][0];

    assert!(outcome["selected"].is_null());
    // §10.3, and §14.1's whole argument: temporary, never 550.
    assert!(
        outcome["would_reply"].as_str().unwrap().starts_with("451"),
        "{}",
        outcome["would_reply"]
    );
    assert!(
        outcome["outbound"].is_null(),
        "no route, no identity to render"
    );
}

#[sqlx::test]
async fn a_sender_policy_refusal_is_an_answer_rather_than_an_http_error(pool: PgPool) {
    // The request was well formed; this is what would happen to it. Reporting it
    // as a 4xx would make a script treat a correct answer as its own bug.
    let state = state_from(pool, config("  strict_senders: true\n"));

    let response = post(
        &state,
        "/dryrun",
        Some(TOKEN),
        r#"{"envelope_from":"nobody@elsewhere.com","recipients":["a@gmail.com"]}"#,
    )
    .await;

    assert_eq!(response.status, StatusCode::OK);
    let doc = response.json();
    assert_eq!(doc["refused"]["reason"], "strict_senders");
    // §10.3's one permitted 550 — a statement about the sender, not the
    // recipient.
    assert!(
        doc["refused"]["would_reply"]
            .as_str()
            .unwrap()
            .starts_with("550"),
        "{doc}"
    );
    assert!(doc["recipients"].as_array().unwrap().is_empty());
    assert!(doc["chain"].as_array().unwrap().is_empty());
}

#[sqlx::test]
async fn an_ordinary_dry_run_carries_no_refusal(pool: PgPool) {
    let state = state(pool);
    let doc = post(&state, "/dryrun", Some(TOKEN), DRYRUN).await.json();
    assert!(doc.get("refused").is_none(), "{doc}");
}

#[sqlx::test]
async fn a_dry_run_needs_at_least_one_recipient(pool: PgPool) {
    let state = state(pool);
    let response = post(
        &state,
        "/dryrun",
        Some(TOKEN),
        r#"{"envelope_from":"a@oldbrand.com","recipients":[]}"#,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// §9.4 fidelity — the assertion the endpoint's value rests on
// ---------------------------------------------------------------------------

/// Walk the real chain and the dry-run chain over the same state, and compare.
async fn compare_walks(state: &AdminState, recipient: &str) -> (String, String) {
    compare_pinned_walks(state, recipient, None).await
}

/// [`compare_walks`] for a thread-affinity reply pinned to `pinned` (D-090):
/// both walks get §3.2 step 2a's order and the same pin.
async fn compare_pinned_walks(
    state: &AdminState,
    recipient: &str,
    pinned: Option<&str>,
) -> (String, String) {
    let cfg = &state.engine.config;
    let pin = match pinned {
        Some(r) => simmer::routing::thread::Pin::Route(r.to_string()),
        None => simmer::routing::thread::Pin::None,
    };
    let chain =
        simmer::routing::thread::order(&["warming".to_string(), "overflow".to_string()], &pin);

    let dry = simmer::routing::chain::dry_walk(
        cfg.default_ramp(),
        &state.engine.groups,
        &cfg.dot_insensitive_domains,
        &store(state),
        &state.engine.frequency,
        &state.engine.preflight,
        &chain,
        pinned,
        recipient,
        Utc::now(),
    )
    .await
    .expect("dry walk");

    let mut evaluation = Vec::new();
    simmer::routing::chain::walk_and_reserve(
        cfg.default_ramp(),
        &state.engine.groups,
        &cfg.dot_insensitive_domains,
        &store(state),
        &state.engine.frequency,
        &state.engine.preflight,
        &chain,
        pinned,
        &[recipient.to_string()],
        "compare",
        &mut evaluation,
    )
    .await
    .expect("real walk");

    (
        simmer::routing::chain::render(&dry),
        simmer::routing::chain::render(&evaluation),
    )
}

#[sqlx::test]
async fn dry_run_agrees_with_the_real_walk_when_everything_is_eligible(pool: PgPool) {
    let state = state(pool);
    let (dry, real) = compare_walks(&state, "someone@gmail.com").await;
    assert_eq!(dry, real);
    assert_eq!(dry, "warming=selected");
}

#[sqlx::test]
async fn dry_run_agrees_with_the_real_walk_on_a_paused_route(pool: PgPool) {
    let state = state(pool);
    post(&state, "/routes/warming/pause", Some(TOKEN), "").await;

    let (dry, real) = compare_walks(&state, "someone@gmail.com").await;
    assert_eq!(dry, real);
    assert_eq!(dry, "warming=paused,overflow=selected");
}

#[sqlx::test]
async fn dry_run_agrees_with_the_real_walk_on_an_exhausted_quota(pool: PgPool) {
    // Google's series is [1], so one committed message fills the day.
    let state = state(pool);
    let day = today(&state.engine.config, "warming");

    let taken = store(&state)
        .reserve(&ReserveRequest {
            ramp: "main".into(),
            route: "warming".into(),
            domain_group: "google".into(),
            day_index: day,
            allowance: Some(1),
            count: 1,
            correlation_id: "filler".into(),
            expires_at: Utc::now() + chrono::Duration::minutes(10),
            over_cap: false,
        })
        .await
        .expect("reserve");
    let simmer::quota::Reserved::Taken(taken) = taken else {
        panic!("expected a reservation");
    };
    store(&state).commit(&taken, &[]).await.expect("commit");

    let (dry, real) = compare_walks(&state, "someone@gmail.com").await;
    assert_eq!(dry, real);
    assert_eq!(dry, "warming=quota,overflow=selected");
}

#[sqlx::test]
async fn dry_run_agrees_with_the_real_walk_on_a_route_that_has_not_started(pool: PgPool) {
    // The ordering trap: `walk_and_reserve` checks §7.3 frequency *before* §7.2's
    // start instant, and a dry run that checked them the other way round would
    // report a different reason for the same route.
    let state = state_from(
        pool,
        simmer::config::from_str(
            &CFG.replace("2020-01-01T00:00:00Z", "2099-01-01T00:00:00Z"),
            "test",
        )
        .expect("fixture is valid"),
    );

    let (dry, real) = compare_walks(&state, "someone@gmail.com").await;
    assert_eq!(dry, real);
    assert_eq!(dry, "warming=not_started,overflow=selected");
}

#[sqlx::test]
async fn dry_run_agrees_with_the_real_walk_on_a_frequency_skip(pool: PgPool) {
    // §7.3, which is the check whose position in the order is easiest to get
    // wrong: it runs before the day-index test, not after it.
    let cfg = simmer::config::from_str(
        &CFG.replace(
            "    identity:\n      envelope_from: \"bounce@newbrand.com\"",
            "    recipient_frequency: { mode: to_address, window: { unit: daily, count: 1 }, \
             threshold: 1 }\n    identity:\n      envelope_from: \"bounce@newbrand.com\"",
        ),
        "test",
    )
    .expect("fixture is valid");
    let state = state_from(pool, cfg);

    // One recorded event puts the recipient at the threshold of 1.
    let keyer = state
        .engine
        .frequency
        .keyer(store(&state).as_ref())
        .await
        .expect("keyer");
    let key = keyer.key_for(
        "someone@gmail.com",
        simmer::config::FrequencyMode::ToAddress,
        &[],
    );
    let day = today(&state.engine.config, "warming");
    let taken = store(&state)
        .reserve(&ReserveRequest {
            ramp: "main".into(),
            route: "warming".into(),
            domain_group: "google".into(),
            day_index: day,
            allowance: Some(1),
            count: 1,
            correlation_id: "filler".into(),
            expires_at: Utc::now() + chrono::Duration::minutes(10),
            over_cap: false,
        })
        .await
        .expect("reserve");
    let simmer::quota::Reserved::Taken(taken) = taken else {
        panic!("expected a reservation");
    };
    store(&state)
        .commit(&taken, std::slice::from_ref(&key))
        .await
        .expect("commit");

    let (dry, real) = compare_walks(&state, "someone@gmail.com").await;
    assert_eq!(dry, real);
    assert_eq!(
        dry, "warming=frequency,overflow=selected",
        "frequency, not quota — the two call for opposite responses"
    );
}

// ---------------------------------------------------------------------------
// §7.3 — what the control plane must never emit
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn no_endpoint_exposes_a_recipient(pool: PgPool) {
    // §7.3 hashes so the container does not accumulate a record of who was
    // mailed. Every read endpoint is checked, not just the ones that touch the
    // frequency tables, so a future addition has to think about it.
    let state = state(pool.clone());
    let day = today(&state.engine.config, "warming");

    // Put a real recipient through the system first, so there is something to
    // leak.
    let keyer = state
        .engine
        .frequency
        .keyer(store(&state).as_ref())
        .await
        .expect("keyer");
    let key = keyer.key_for(
        "victim@gmail.com",
        simmer::config::FrequencyMode::ToAddress,
        &[],
    );
    let taken = store(&state)
        .reserve(&ReserveRequest {
            ramp: "main".into(),
            route: "warming".into(),
            domain_group: "google".into(),
            day_index: day,
            allowance: Some(1),
            count: 1,
            correlation_id: "real".into(),
            expires_at: Utc::now() + chrono::Duration::minutes(10),
            over_cap: false,
        })
        .await
        .expect("reserve");
    let simmer::quota::Reserved::Taken(taken) = taken else {
        panic!("expected a reservation");
    };
    store(&state)
        .commit(&taken, std::slice::from_ref(&key))
        .await
        .expect("commit");

    for uri in ["/routes", "/routes/warming", "/quota", "/health"] {
        let body = get(&state, uri, Some(TOKEN)).await.body;
        assert!(!body.contains("victim"), "{uri} leaked a recipient: {body}");
        assert!(
            !body.contains("recipient_hash"),
            "{uri} leaked a recipient key: {body}"
        );
        assert!(
            !body.contains('@'),
            "{uri} contains an address-shaped value: {body}"
        );
    }
}

#[sqlx::test]
async fn a_dry_run_echoes_only_what_the_caller_supplied(pool: PgPool) {
    // The one endpoint that carries a plaintext address, because the operator
    // put it there. It must not reveal anything about anyone else.
    let state = state(pool.clone());
    let day = today(&state.engine.config, "warming");

    let keyer = state
        .engine
        .frequency
        .keyer(store(&state).as_ref())
        .await
        .expect("keyer");
    let other = keyer.key_for(
        "someone.else@gmail.com",
        simmer::config::FrequencyMode::ToAddress,
        &[],
    );
    let taken = store(&state)
        .reserve(&ReserveRequest {
            ramp: "main".into(),
            route: "warming".into(),
            domain_group: "google".into(),
            day_index: day,
            allowance: Some(1),
            count: 1,
            correlation_id: "other".into(),
            expires_at: Utc::now() + chrono::Duration::minutes(10),
            over_cap: false,
        })
        .await
        .expect("reserve");
    let simmer::quota::Reserved::Taken(taken) = taken else {
        panic!("expected a reservation");
    };
    store(&state)
        .commit(&taken, std::slice::from_ref(&other))
        .await
        .expect("commit");

    let body = post(&state, "/dryrun", Some(TOKEN), DRYRUN).await.body;
    assert!(body.contains("someone@gmail.com"), "the supplied recipient");
    assert!(
        !body.contains("someone.else@gmail.com"),
        "anybody else: {body}"
    );
}

// ---------------------------------------------------------------------------
// §7.5 — the store being unreachable
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn a_bad_request_body_is_a_400_rather_than_a_500(pool: PgPool) {
    let state = state(pool);
    for body in [
        "{",
        r#"{"domain_group":"google"}"#,
        r#"{"domain_group":"google","allowance":"lots"}"#,
        r#"{"domain_grp":"google","allowance":1}"#,
    ] {
        let response = post(&state, "/routes/warming/allowance", Some(TOKEN), body).await;
        assert!(
            response.status.is_client_error(),
            "body {body:?} produced {}",
            response.status
        );
    }
}

#[sqlx::test]
async fn a_body_free_post_is_accepted_without_an_empty_object(pool: PgPool) {
    // `curl -XPOST .../pause` with no body, which is what an operator will type.
    let state = state(pool);
    let response = send(
        &state,
        Request::post("/routes/warming/pause")
            .header("authorization", format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// §3.2 step 2a — thread affinity in the dry run (D-090)
// ---------------------------------------------------------------------------

/// CFG with `thread_affinity` on: each route gets its own `Message-ID:` domain,
/// and warming a one-per-day recipient_frequency constraint.
fn affinity_state(pool: PgPool) -> AdminState {
    let yaml = CFG
        .replace(
            "        From: \"New Brand <hello@newbrand.com>\"\n",
            "        From: \"New Brand <hello@newbrand.com>\"\n        Message-ID: \"<{{uuid}}@newbrand.com>\"\n",
        )
        .replace(
            "    warmup:\n      started: \"2020-01-01T00:00:00Z\"\n",
            "    recipient_frequency:\n      mode: to_address\n      window: { unit: daily, count: 1 }\n      threshold: 1\n    warmup:\n      started: \"2020-01-01T00:00:00Z\"\n",
        )
        .replace(
            "    identity: { envelope_from: \"bounce@established.com\" }\n",
            "    identity:\n      envelope_from: \"bounce@established.com\"\n      set_headers: { Message-ID: \"<{{uuid}}@established.com>\" }\n",
        )
        + "  thread_affinity: true\n";
    let cfg = simmer::config::from_str(&yaml, "test").expect("fixture is valid");
    assert!(cfg.default_ramp().thread_affinity);
    assert!(cfg
        .default_ramp()
        .route("warming")
        .unwrap()
        .recipient_frequency
        .is_some());
    state_from(pool, cfg)
}

/// Fill google's one warming slot for today.
async fn spend_google(state: &AdminState) {
    let day = today(&state.engine.config, "warming");
    let simmer::quota::Reserved::Taken(r) = store(state)
        .reserve(&ReserveRequest {
            ramp: "main".into(),
            route: "warming".into(),
            domain_group: "google".into(),
            day_index: day,
            allowance: Some(1),
            count: 1,
            correlation_id: "filler".into(),
            expires_at: Utc::now() + chrono::Duration::minutes(10),
            over_cap: false,
        })
        .await
        .expect("reserve")
    else {
        panic!("expected a reservation");
    };
    store(state).commit(&r, &[]).await.expect("commit");
}

#[sqlx::test]
async fn dry_run_agrees_with_the_real_walk_on_a_pinned_route_past_its_cap(pool: PgPool) {
    let state = affinity_state(pool);
    spend_google(&state).await;

    let (dry, real) = compare_walks(&state, "a@gmail.com").await;
    assert_eq!(dry, real);
    assert_eq!(
        dry, "warming=quota,overflow=selected",
        "unpinned, the ramp holds"
    );

    let (dry, real) = compare_pinned_walks(&state, "b@gmail.com", Some("warming")).await;
    assert_eq!(dry, real);
    assert_eq!(dry, "warming=selected_over_cap");
}

#[sqlx::test]
async fn dry_run_agrees_with_the_real_walk_on_a_pinned_route_over_its_frequency(pool: PgPool) {
    let state = affinity_state(pool);

    // One delivered message to bob puts him at warming's threshold of 1.
    let mut ev = Vec::new();
    let simmer::routing::chain::Walk::Selected(s) = simmer::routing::chain::walk_and_reserve(
        state.engine.config.default_ramp(),
        &state.engine.groups,
        &state.engine.config.dot_insensitive_domains,
        &store(&state),
        &state.engine.frequency,
        &state.engine.preflight,
        &["warming".to_string()],
        None,
        &["bob@example.com".to_string()],
        "first",
        &mut ev,
    )
    .await
    .expect("walk") else {
        panic!("expected warming");
    };
    store(&state)
        .commit(&s.reservation, &s.recipient_keys)
        .await
        .expect("commit");

    let (dry, real) = compare_walks(&state, "bob@example.com").await;
    assert_eq!(dry, real);
    assert_eq!(dry, "warming=frequency,overflow=selected");

    let (dry, real) = compare_pinned_walks(&state, "bob@example.com", Some("warming")).await;
    assert_eq!(dry, real);
    assert_eq!(dry, "warming=selected", "a reply is not over-mailing");
}

#[sqlx::test]
async fn dry_run_agrees_with_the_real_walk_on_a_paused_pinned_route(pool: PgPool) {
    let state = affinity_state(pool);
    post(&state, "/routes/warming/pause", Some(TOKEN), "").await;
    let (dry, real) = compare_pinned_walks(&state, "a@gmail.com", Some("warming")).await;
    assert_eq!(dry, real);
    assert_eq!(dry, "warming=paused,overflow=selected");
}

#[sqlx::test]
async fn dry_run_reports_the_pin_from_the_threading_headers(pool: PgPool) {
    let state = affinity_state(pool);
    spend_google(&state).await;

    let doc = post(
        &state,
        "/dryrun",
        Some(TOKEN),
        r#"{"envelope_from":"a@oldbrand.com","recipients":["x@gmail.com"],
            "in_reply_to":"<CAx9@mail.gmail.com>",
            "references":"<3f2a@newbrand.com> <CAx9@mail.gmail.com>"}"#,
    )
    .await
    .json();

    assert_eq!(doc["thread"]["outcome"], "pinned", "{doc}");
    assert_eq!(doc["thread"]["pinned"], "warming", "{doc}");
    let r = &doc["recipients"][0];
    assert_eq!(r["selected"], "warming", "{doc}");
    assert_eq!(r["evaluation"][0]["reason"], "over_cap", "{doc}");
    assert_eq!(
        doc["chain"],
        serde_json::json!(["warming", "overflow"]),
        "configured order"
    );
}

#[sqlx::test]
async fn dry_run_reads_the_pin_from_a_supplied_message_and_reports_unmatched(pool: PgPool) {
    let state = affinity_state(pool);

    let doc = post(
        &state,
        "/dryrun",
        Some(TOKEN),
        r#"{"envelope_from":"a@oldbrand.com","recipients":["x@example.com"],
            "message":"From: a@oldbrand.com\nReferences: <1@established.com>\n\nhi\n"}"#,
    )
    .await
    .json();
    assert_eq!(doc["thread"]["pinned"], "overflow", "{doc}");
    assert_eq!(
        doc["recipients"][0]["evaluation"][0]["route"], "overflow",
        "{doc}"
    );

    let doc = post(
        &state,
        "/dryrun",
        Some(TOKEN),
        r#"{"envelope_from":"a@oldbrand.com","recipients":["x@example.com"],
            "references":"<CAx9@mail.gmail.com>"}"#,
    )
    .await
    .json();
    assert_eq!(doc["thread"]["outcome"], "unmatched", "{doc}");
    assert_eq!(doc["recipients"][0]["selected"], "warming", "{doc}");

    let doc = post(
        &state,
        "/dryrun",
        Some(TOKEN),
        r#"{"envelope_from":"a@oldbrand.com","recipients":["x@example.com"]}"#,
    )
    .await
    .json();
    assert!(
        doc.get("thread").is_none(),
        "a first message has no thread: {doc}"
    );
}

#[sqlx::test]
async fn the_quota_view_reports_replies_past_the_cap_truthfully(pool: PgPool) {
    // The control plane must not lie (D-026's rule): past the cap is shown as
    // committed above the allowance, with no headroom — never as a raised
    // allowance, and never as negative headroom.
    let state = affinity_state(pool);
    spend_google(&state).await;
    for _ in 0..2 {
        let (_, real) = compare_pinned_walks(&state, "b@gmail.com", Some("warming")).await;
        assert_eq!(real, "warming=selected_over_cap");
    }
    // compare_pinned_walks leaves the real walk's reservation outstanding, so
    // the two replies show as reserved: in flight, past the cap.
    let day = today(&state.engine.config, "warming");
    let u = store(&state)
        .usage("main", "warming", "google", day)
        .await
        .unwrap();
    assert_eq!((u.committed, u.reserved), (1, 2));

    let warming = route_of(&get(&state, "/routes", Some(TOKEN)).await.json(), "warming");
    let group = group_of(&warming, "google");
    assert_eq!(group["allowance"], 1, "{group}");
    assert_eq!(group["reserved"], 2, "{group}");
    assert_eq!(group["headroom"], 0, "{group}");
}
