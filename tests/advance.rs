//! D-115's volume-gated advancement through the real walk, against real
//! Postgres.
//!
//! `src/quota/advance.rs`'s unit tests own the rule. What only the walk and a
//! real store can show is here: that the step a day's row is created at is the
//! one the rule picks from the previous row, that the walk then enforces exactly
//! that step's allowance, that `advance: auto` is §7.2 as it always was, and
//! that exactly one reservation per row reports creating it.
//!
//! The author's example, as a fixture: four routes, A/B/C on [50, 100, 200, 400]
//! and D on [100, 200, 400, 800], each in its own chain ahead of an overflow.

#![cfg(feature = "postgres")]

use std::sync::Arc;

use chrono::Utc;
use simmer::config::Config;
use simmer::frequency::Frequency;
use simmer::quota::store::{QuotaStore, RouteState};
use simmer::quota::{self, PgQuotaStore};
use simmer::routing::chain::{self, Walk};
use sqlx::PgPool;

const ROUTES: [&str; 4] = ["a", "b", "c", "d"];

/// Started `day` and a half days ago, so the day index is `day`.
fn config(day: i64, advance: &str) -> Config {
    let started = (Utc::now() - chrono::Duration::hours(24 * day + 12)).to_rfc3339();
    let route = |name: &str, schedule: &str| {
        format!(
            r#"
  - name: {name}
    downstream:
      host: "127.0.0.1"
      port: 2525
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
    identity: {{ envelope_from: "b@{name}.newbrand.com" }}
    warmup:
      started: "{started}"
      schedule: {{ default: {schedule} }}"#
        )
    };
    let yaml = format!(
        r#"
server:
  listeners:
    - address: "127.0.0.1:0"
  hostname: "simmer.test"
  max_message_bytes: 100000
  max_recipients: 1
  max_concurrent_sessions: 16
  allowed_cidrs: ["127.0.0.0/8"]
  timeouts: {{ command: 5s, data: 5s, session: 60s }}
  auth: {{ allow_insecure_auth: true }}
database:
  url: "postgres://u:p@localhost/simmer"
  connect_timeout: 5s
  fail_closed: true
admin: {{ listen: "127.0.0.1:0", auth_token: "t" }}
logging: {{ level: warn, format: text }}
default_ramp: main
ramps:
 main:
  domain_groups:
  - {{ name: catchall, domains: ["*"] }}
  senders:
  - {{ match: "a.example", match_on: envelope, chain: [a, overflow] }}
  - {{ match: "b.example", match_on: envelope, chain: [b, overflow] }}
  - {{ match: "c.example", match_on: envelope, chain: [c, overflow] }}
  - {{ match: "d.example", match_on: envelope, chain: [d, overflow] }}
  default_chain: [overflow]
  warmup_advance: {advance}
  routes:{a}{b}{c}{d}
  - name: overflow
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: 2526
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
    identity: {{ envelope_from: "b@established.com" }}
"#,
        a = route("a", "[50, 100, 200, 400]"),
        b = route("b", "[50, 100, 200, 400]"),
        c = route("c", "[50, 100, 200, 400]"),
        d = route("d", "[100, 200, 400, 800]"),
    );
    simmer::config::from_str(&yaml, "test").expect("fixture is valid")
}

const THRESHOLD: &str = "{ mode: threshold, threshold: 0.9 }";
const AUTO: &str = "auto";

fn store(pool: PgPool) -> Arc<dyn QuotaStore> {
    Arc::new(PgQuotaStore::new(pool))
}

/// What one walk did.
struct Sent {
    on_warming: i64,
    on_overflow: i64,
    /// How many of the warming reservations reported creating the day's row.
    created_rows: usize,
    steps: Vec<i64>,
}

/// Send `n` messages down `route`'s chain on the config's day, committing every
/// one as a downstream `2xx` would.
async fn send(cfg: &Config, store: &Arc<dyn QuotaStore>, route: &str, n: i64) -> Sent {
    let mut sent = Sent {
        on_warming: 0,
        on_overflow: 0,
        created_rows: 0,
        steps: Vec::new(),
    };
    let chain = vec![route.to_string(), "overflow".to_string()];
    for i in 0..n {
        let mut evaluation = Vec::new();
        let walked = chain::walk_and_reserve(
            cfg.default_ramp(),
            &simmer::routing::domain_group::Grouper::literal(),
            &cfg.dot_insensitive_domains,
            store,
            &Frequency::new(),
            &simmer::preflight::Registry::new(),
            &chain,
            None,
            &[format!("reader{i}@example.com")],
            "advance-test",
            &mut evaluation,
            Utc::now(),
        )
        .await
        .expect("walk");
        let Walk::Selected(s) = walked else {
            panic!("an overflow route is always eligible");
        };
        if s.route.name == route {
            sent.on_warming += 1;
            sent.steps.push(s.position.step);
            if s.reservation.created_row {
                sent.created_rows += 1;
            }
        } else {
            sent.on_overflow += 1;
        }
        store.commit(&s.reservation, &[]).await.expect("commit");
    }
    sent
}

/// Each route's allowance today, from the one entry point everything uses.
async fn allowances(cfg: &Config, store: &Arc<dyn QuotaStore>) -> Vec<i64> {
    let mut out = Vec::new();
    for name in ROUTES {
        let route = cfg.default_ramp().route(name).expect("route");
        let day = quota::day::for_route(route, Utc::now());
        let p = quota::position(
            store.as_ref(),
            route,
            "catchall",
            day,
            RouteState::default(),
        )
        .await
        .expect("position");
        out.push(p.allowance.as_column().expect("a warming route is capped"));
    }
    out
}

async fn send_each(cfg: &Config, store: &Arc<dyn QuotaStore>, counts: [i64; 4]) {
    for (route, n) in ROUTES.iter().zip(counts) {
        let sent = send(cfg, store, route, n).await;
        assert_eq!(sent.on_warming, n, "{route}: all under the cap");
    }
}

// -- the author's example ---------------------------------------------------

#[sqlx::test]
async fn day_one_holds_only_the_route_that_fell_short(pool: PgPool) {
    let store = store(pool);
    let day0 = config(0, THRESHOLD);
    assert_eq!(allowances(&day0, &store).await, vec![50, 50, 50, 100]);

    // 45/40/45/98 against bars of 45/45/45/90: B alone falls short.
    send_each(&day0, &store, [45, 40, 45, 98]).await;

    let day1 = config(1, THRESHOLD);
    assert_eq!(allowances(&day1, &store).await, vec![100, 50, 100, 200]);
}

#[sqlx::test]
async fn the_walk_enforces_the_held_allowance_exactly(pool: PgPool) {
    // Not just reported: on day 1 B carries 50 and spills the rest, while A
    // carries 100.
    let store = store(pool);
    send_each(&config(0, THRESHOLD), &store, [45, 40, 45, 98]).await;

    let day1 = config(1, THRESHOLD);
    let b = send(&day1, &store, "b", 120).await;
    assert_eq!((b.on_warming, b.on_overflow), (50, 70));
    assert!(b.steps.iter().all(|s| *s == 0), "held at step 0");

    let a = send(&day1, &store, "a", 120).await;
    assert_eq!((a.on_warming, a.on_overflow), (100, 20));
    assert!(a.steps.iter().all(|s| *s == 1));
}

#[sqlx::test]
async fn the_example_continued_into_day_two(pool: PgPool) {
    let store = store(pool);
    send_each(&config(0, THRESHOLD), &store, [45, 40, 45, 98]).await;
    // Day 1 against 100/50/100/200: A, B, D meet; C sends 80 of the 90 it needs.
    send_each(&config(1, THRESHOLD), &store, [100, 50, 80, 200]).await;
    assert_eq!(
        allowances(&config(2, THRESHOLD), &store).await,
        vec![200, 100, 100, 400]
    );
}

#[sqlx::test]
async fn auto_mode_advances_on_the_calendar_whatever_was_sent(pool: PgPool) {
    let store = store(pool);
    send_each(&config(0, AUTO), &store, [45, 40, 45, 98]).await;
    assert_eq!(
        allowances(&config(1, AUTO), &store).await,
        vec![100, 100, 100, 200]
    );
    // Nothing at all on day 1 changes nothing either.
    assert_eq!(
        allowances(&config(2, AUTO), &store).await,
        vec![200, 200, 200, 400]
    );
    let b = send(&config(1, AUTO), &store, "b", 120).await;
    assert_eq!(b.on_warming, 100, "B is not held under auto");
}

// -- the rule's edges, through the store ------------------------------------

#[sqlx::test]
async fn a_day_with_no_traffic_neither_advances_nor_loses_ground(pool: PgPool) {
    // A meets on day 0, sends nothing on days 1 and 2: day 3 is step 1, not 3.
    let store = store(pool);
    send(&config(0, THRESHOLD), &store, "a", 50).await;
    assert_eq!(allowances(&config(3, THRESHOLD), &store).await[0], 100);
}

#[sqlx::test]
async fn a_route_that_never_sent_stays_on_its_first_entry(pool: PgPool) {
    let store = store(pool);
    assert_eq!(
        allowances(&config(9, THRESHOLD), &store).await,
        vec![50, 50, 50, 100]
    );
}

#[sqlx::test]
async fn an_element_that_keeps_meeting_climbs_and_then_repeats_its_last_entry(pool: PgPool) {
    let store = store(pool);
    let mut seen = Vec::new();
    for day in 0..6 {
        let cfg = config(day, THRESHOLD);
        let cap = allowances(&cfg, &store).await[0];
        seen.push(cap);
        send(&cfg, &store, "a", cap).await;
    }
    assert_eq!(seen, vec![50, 100, 200, 400, 400, 400]);
}

#[sqlx::test]
async fn todays_step_is_fixed_once_the_row_exists(pool: PgPool) {
    // Day 1's row is created at step 0 (day 0 fell short). A late commit that
    // lands on day 0 afterwards does not re-decide day 1.
    use simmer::quota::store::{ReserveRequest, Reserved};
    let store = store(pool);
    send(&config(0, THRESHOLD), &store, "a", 10).await;
    let day1 = config(1, THRESHOLD);
    send(&day1, &store, "a", 1).await;
    assert_eq!(allowances(&day1, &store).await[0], 50);

    let late = store
        .reserve(&ReserveRequest {
            ramp: "main".into(),
            route: "a".into(),
            domain_group: "catchall".into(),
            day_index: 0,
            allowance: Some(50),
            step: Some(0),
            count: 40,
            correlation_id: "late".into(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            ..Default::default()
        })
        .await
        .expect("reserve");
    let Reserved::Taken(r) = late else {
        panic!("headroom");
    };
    store.commit(&r, &[]).await.expect("commit");

    assert_eq!(
        allowances(&day1, &store).await[0],
        50,
        "decided when created"
    );
    // ...and tomorrow is decided from day 1's own row, not day 0's.
    assert_eq!(allowances(&config(2, THRESHOLD), &store).await[0], 50);
}

#[sqlx::test]
async fn exactly_one_reservation_reports_creating_the_days_row(pool: PgPool) {
    let store = store(pool);
    let sent = send(&config(0, THRESHOLD), &store, "a", 30).await;
    assert_eq!(sent.created_rows, 1);
    let sent = send(&config(0, THRESHOLD), &store, "a", 5).await;
    assert_eq!(sent.created_rows, 0, "the row already exists");
    let sent = send(&config(1, THRESHOLD), &store, "a", 5).await;
    assert_eq!(sent.created_rows, 1, "a new day is a new row");
}

#[sqlx::test]
async fn switching_auto_to_threshold_continues_from_the_last_rows_step(pool: PgPool) {
    // Auto through day 2 writes step 2; under the threshold rule on day 3 the
    // route is at step 2 + (day 2 met?).
    let store = store(pool);
    for day in 0..3 {
        send(&config(day, AUTO), &store, "a", 10).await;
    }
    assert_eq!(
        allowances(&config(3, THRESHOLD), &store).await[0],
        200,
        "step 2 held: day 2 sent 10 of 200"
    );
    assert_eq!(allowances(&config(3, AUTO), &store).await[0], 400);
}

#[sqlx::test]
async fn the_dry_run_and_the_walk_agree_on_a_held_route(pool: PgPool) {
    let store = store(pool);
    send_each(&config(0, THRESHOLD), &store, [45, 40, 45, 98]).await;
    let day1 = config(1, THRESHOLD);
    let evaluation = simmer::routing::chain::dry_walk(
        day1.default_ramp(),
        &simmer::routing::domain_group::Grouper::literal(),
        &day1.dot_insensitive_domains,
        &store,
        &Frequency::new(),
        &simmer::preflight::Registry::new(),
        &["b".to_string(), "overflow".to_string()],
        None,
        "reader1@example.com",
        Utc::now(),
    )
    .await
    .expect("dry run");
    assert_eq!(evaluation[0].warmup_step, Some(0), "B is held at step 0");

    let walked = send(&day1, &store, "b", 1).await;
    assert_eq!(walked.steps, vec![0]);
}
