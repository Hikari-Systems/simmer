//! D-111's per-segment rate limits through the real walk and the real relay.
//!
//! `src/quota/rate.rs` owns the arithmetic and `tests/store_conformance/` the
//! stores. What only the walk and the relay can show is here: that a route out
//! of slots **steers** to the next link, that a booked future slot holds the
//! client and the downstream receives the message at it, that the schedule
//! follows the day index, that a slot is given back on every outcome that does
//! not commit — except §10.2's ambiguous final dot — that a pinned reply books
//! past the limit, and that §9.4's dry run gives the real walk's answer.
//!
//! The waits are real time, a second or so: Simmer and the fake downstream
//! talk over TCP, and a paused tokio clock auto-advances whenever every task is
//! waiting on I/O, which fires the stage timeouts. The walk-level tests need no
//! clock at all — they pass `now`.

#![cfg(feature = "postgres")]

mod support;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use simmer::frequency::Frequency;
use simmer::quota::store::QuotaStore;
use simmer::quota::PgQuotaStore;
use simmer::routing::chain::{self, SkipReason, Walk};
use sqlx::PgPool;
use support::{Act, FakeDownstream, GrantAllQuota, Script, Simmer, Turn};

const BODY: &str = "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n";
const STARTED: &str = "2026-01-01T00:00:00Z";

/// A warming route carrying `rate` (route-key indentation, one key per line)
/// in front of an overflow, with google, yahoo and a catch-all.
fn config(warming_port: u16, overflow_port: u16, rate: &str, cap: i64) -> String {
    format!(
        r#"
server:
  listeners:
    - address: "127.0.0.1:0"
  hostname: "simmer.test"
  max_message_bytes: 100000
  max_recipients: 1
  max_concurrent_sessions: 16
  allowed_cidrs: ["127.0.0.0/8"]
  timeouts: {{ command: 5s, data: 60s, session: 120s }}
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
  - {{ name: google, domains: ["gmail.com"] }}
  - {{ name: yahoo, domains: ["yahoo.com"] }}
  - {{ name: catchall, domains: ["*"] }}
  senders:
  - {{ match: "oldbrand.com", match_on: envelope, chain: [warming, overflow] }}
  default_chain: [overflow]
  routes:
  - name: warming
    downstream:
      host: "127.0.0.1"
      port: {warming_port}
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 100 }}
      timeouts: {{ connect: 2s, command: 2s, data: 2s }}
    identity: {{ envelope_from: "b@newbrand.com" }}
    rate:
{rate}    warmup:
      started: "{STARTED}"
      schedule: {{ default: [{cap}] }}
  - name: overflow
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: {overflow_port}
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 100 }}
      timeouts: {{ connect: 2s, command: 2s, data: 2s }}
    identity: {{ envelope_from: "b@established.com" }}
"#
    )
}

fn store(pool: PgPool) -> Arc<dyn QuotaStore> {
    Arc::new(PgQuotaStore::new(pool))
}

fn at(offset: chrono::Duration) -> DateTime<Utc> {
    STARTED.parse::<DateTime<Utc>>().unwrap() + offset
}

fn hours(h: i64) -> chrono::Duration {
    chrono::Duration::hours(h)
}

fn minutes(m: i64) -> chrono::Duration {
    chrono::Duration::minutes(m)
}

/// One real walk at `now`: the selected route and the evaluation.
async fn walk(
    cfg: &simmer::config::Config,
    store: &Arc<dyn QuotaStore>,
    pinned: Option<&str>,
    recipient: &str,
    now: DateTime<Utc>,
) -> (Option<String>, Vec<chain::Step>) {
    let mut evaluation = Vec::new();
    let chain = vec!["warming".to_string(), "overflow".to_string()];
    let walked = chain::walk_and_reserve(
        cfg.default_ramp(),
        &simmer::routing::domain_group::Grouper::literal(),
        &cfg.dot_insensitive_domains,
        store,
        &Frequency::new(),
        &simmer::preflight::Registry::new(),
        &chain,
        pinned,
        &[recipient.to_string()],
        "rate-test",
        &mut evaluation,
        now,
    )
    .await
    .expect("walk");
    let selected = match walked {
        Walk::Selected(s) => Some(s.route.name.clone()),
        Walk::Exhausted => None,
    };
    (selected, evaluation)
}

async fn dry(
    cfg: &simmer::config::Config,
    store: &Arc<dyn QuotaStore>,
    pinned: Option<&str>,
    recipient: &str,
    now: DateTime<Utc>,
) -> Vec<chain::Step> {
    chain::dry_walk(
        cfg.default_ramp(),
        &simmer::routing::domain_group::Grouper::literal(),
        &cfg.dot_insensitive_domains,
        store,
        &Frequency::new(),
        &simmer::preflight::Registry::new(),
        &["warming".to_string(), "overflow".to_string()],
        pinned,
        recipient,
        now,
    )
    .await
    .expect("dry walk")
}

fn cfg(rate: &str, cap: i64) -> simmer::config::Config {
    simmer::config::from_str(&config(2525, 2526, rate, cap), "test").expect("fixture is valid")
}

async fn tat(store: &Arc<dyn QuotaStore>, group: &str) -> Option<DateTime<Utc>> {
    store
        .rate_tats("main")
        .await
        .unwrap()
        .get(&("warming".to_string(), group.to_string()))
        .copied()
}

// ---------------------------------------------------------------------------
// (a) through the relay: burst, then steer
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn a_slow_rate_sends_its_burst_and_steers_the_rest_to_overflow(pool: PgPool) {
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let store = store(pool);
    let simmer = Simmer::start_with_quota(
        &config(
            warm.addr.port(),
            over.addr.port(),
            "      per_hour: 1\n      burst: 2\n",
            1000,
        ),
        Arc::clone(&store),
    )
    .await;

    for i in 0..5 {
        let mut c = simmer.connect().await;
        c.hello().await;
        let r = c
            .deliver("jane@oldbrand.com", &format!("r{i}@gmail.com"), BODY)
            .await;
        assert_eq!(r.code, 250, "message {i}: {r:?}");
    }

    assert_eq!(warm.messages().len(), 2, "the burst, and no more");
    assert_eq!(over.messages().len(), 3, "steered, never refused");
    // The quota row counted exactly what the warming route sent.
    let day = simmer::quota::day::index(STARTED.parse().unwrap(), Utc::now());
    let u = store.usage("main", "warming", "google", day).await.unwrap();
    assert_eq!((u.committed, u.reserved), (2, 0));
}

// ---------------------------------------------------------------------------
// (b) a domain-group override paces differently on the same route
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn a_yahoo_override_paces_differently_from_google(pool: PgPool) {
    // Default 3600/h (one a second); yahoo 1/h. Burst 1, max_wait 3s: google's
    // second message waits about a second for its slot and still goes warming;
    // yahoo's has no slot for an hour and steers.
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start_with_quota(
        &config(
            warm.addr.port(),
            over.addr.port(),
            "      schedule:\n        default: [3600]\n        overrides:\n          yahoo: [1]\n      max_wait: 3s\n",
            1000,
        ),
        store(pool),
    )
    .await;

    for to in ["a@gmail.com", "b@gmail.com", "a@yahoo.com", "b@yahoo.com"] {
        let mut c = simmer.connect().await;
        c.hello().await;
        let r = c.deliver("jane@oldbrand.com", to, BODY).await;
        assert_eq!(r.code, 250, "{to}: {r:?}");
    }

    let to = |d: &FakeDownstream| -> Vec<String> {
        d.messages()
            .into_iter()
            .flat_map(|m| m.recipients)
            .collect()
    };
    assert_eq!(to(&warm), ["a@gmail.com", "b@gmail.com", "a@yahoo.com"]);
    assert_eq!(to(&over), ["b@yahoo.com"]);
}

// ---------------------------------------------------------------------------
// (c) max_wait holds the client; the downstream receives at the paced instant
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn max_wait_holds_the_client_until_the_booked_slot(pool: PgPool) {
    // 3600/h, burst 1: one a second. Three messages sent back to back, each on
    // its own connection, arrive at the downstream a second apart.
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let simmer = Arc::new(
        Simmer::start_with_quota(
            &config(
                warm.addr.port(),
                over.addr.port(),
                "      per_hour: 3600\n      max_wait: 5s\n",
                1000,
            ),
            store(pool),
        )
        .await,
    );

    let started = std::time::Instant::now();
    let mut handles = Vec::new();
    for i in 0..3 {
        let simmer = Arc::clone(&simmer);
        handles.push(tokio::spawn(async move {
            let mut c = simmer.connect().await;
            c.hello().await;
            let r = c
                .deliver("jane@oldbrand.com", &format!("r{i}@gmail.com"), BODY)
                .await;
            (r.code, std::time::Instant::now())
        }));
    }
    for h in handles {
        let (code, _) = h.await.unwrap();
        assert_eq!(code, 250);
    }
    assert!(
        over.messages().is_empty(),
        "nothing steered: every slot was within max_wait"
    );

    // When each MAIL FROM reached the downstream: the start of each relay.
    let mut mails: Vec<_> = warm
        .timed_commands()
        .into_iter()
        .filter(|c| c.line.to_ascii_uppercase().starts_with("MAIL FROM"))
        .map(|c| c.at.duration_since(started))
        .collect();
    mails.sort();
    assert_eq!(mails.len(), 3);
    // A second apart, give or take scheduling. The lower bound is the claim.
    for pair in mails.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            gap >= Duration::from_millis(900) && gap < Duration::from_millis(2500),
            "gaps {mails:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// (d) the schedule follows the day index and repeats its last value
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn the_rate_schedule_advances_with_the_day_and_repeats_its_last_value(pool: PgPool) {
    // [1, 2, 3]/h, burst 1, max_wait 0: intervals of 60, 30 and 20 minutes.
    let cfg = cfg("      schedule: { default: [1, 2, 3] }\n", 100_000);
    let store = store(pool);
    let r = "x@gmail.com";
    let route = |w: (Option<String>, Vec<chain::Step>)| w.0.unwrap();

    // Day 0: one an hour.
    assert_eq!(
        route(walk(&cfg, &store, None, r, at(hours(1))).await),
        "warming"
    );
    let (sel, ev) = walk(&cfg, &store, None, r, at(hours(1) + minutes(59))).await;
    assert_eq!(sel.as_deref(), Some("overflow"));
    assert_eq!(ev[0].outcome, Err(SkipReason::Rate));
    assert_eq!(
        ev[0].rate.unwrap().send_at,
        at(hours(2)),
        "the earliest slot"
    );

    // Day 1: one every 30 minutes.
    assert_eq!(
        route(walk(&cfg, &store, None, r, at(hours(25))).await),
        "warming"
    );
    assert_eq!(
        route(walk(&cfg, &store, None, r, at(hours(25) + minutes(30))).await),
        "warming"
    );
    assert_eq!(
        route(walk(&cfg, &store, None, r, at(hours(25) + minutes(45))).await),
        "overflow"
    );

    // Day 7, past the end: the last value, one every 20 minutes.
    let d7 = hours(7 * 24 + 1);
    assert_eq!(route(walk(&cfg, &store, None, r, at(d7)).await), "warming");
    assert_eq!(
        route(walk(&cfg, &store, None, r, at(d7 + minutes(20))).await),
        "warming"
    );
    assert_eq!(
        route(walk(&cfg, &store, None, r, at(d7 + minutes(30))).await),
        "overflow"
    );

    // Not started: the route is not_started, and the rate is never consulted.
    let before = tat(&store, "google").await;
    let (_, ev) = walk(&cfg, &store, None, r, at(-hours(1))).await;
    assert_eq!(ev[0].outcome, Err(SkipReason::NotStarted));
    assert_eq!(tat(&store, "google").await, before);
}

// ---------------------------------------------------------------------------
// (e) the dry run agrees with the real walk, step for step
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn the_dry_run_agrees_with_the_real_walk_step_for_step(pool: PgPool) {
    // 3600/h (a second apart), burst 2, max_wait 2s. At one instant: two go at
    // once, the third waits a second, the fourth two, the fifth is too late.
    let cfg = cfg(
        "      per_hour: 3600\n      burst: 2\n      max_wait: 2s\n",
        100_000,
    );
    let store = store(pool);
    let now = at(hours(3));
    let mut waits = Vec::new();
    for i in 0..5 {
        let r = format!("r{i}@gmail.com");
        let dry = dry(&cfg, &store, None, &r, now).await;
        assert!(tat(&store, "google").await <= Some(now + chrono::Duration::seconds(i)));
        let (_, real) = walk(&cfg, &store, None, &r, now).await;
        assert_eq!(chain::render(&dry), chain::render(&real), "message {i}");
        assert_eq!(
            dry.iter().map(|s| s.rate).collect::<Vec<_>>(),
            real.iter().map(|s| s.rate).collect::<Vec<_>>(),
            "message {i}: the same slot, the same wait"
        );
        waits.push(real[0].rate.unwrap().wait.num_seconds());
        if i >= 4 {
            assert_eq!(real[0].outcome, Err(SkipReason::Rate), "message {i}");
        }
    }
    assert_eq!(
        waits,
        vec![0, 0, 1, 2, 3],
        "two at once, then a second apart, then too late"
    );
}

#[sqlx::test]
async fn the_dry_run_books_nothing(pool: PgPool) {
    let cfg = cfg("      per_hour: 1\n", 100_000);
    let store = store(pool);
    for _ in 0..3 {
        let steps = dry(&cfg, &store, None, "x@gmail.com", at(hours(1))).await;
        assert_eq!(chain::render(&steps), "warming=selected");
    }
    assert_eq!(tat(&store, "google").await, None);
}

// ---------------------------------------------------------------------------
// The unbook paths
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn no_headroom_gives_the_slot_back(pool: PgPool) {
    // A cap of 1 and a burst of 5: the second message books a slot, finds no
    // headroom, and must give the slot back rather than spend it.
    let cfg = cfg("      per_hour: 1\n      burst: 5\n", 1);
    let store = store(pool);
    let now = at(hours(1));
    assert_eq!(
        walk(&cfg, &store, None, "a@gmail.com", now)
            .await
            .0
            .as_deref(),
        Some("warming")
    );
    let after_first = tat(&store, "google").await;
    let (sel, ev) = walk(&cfg, &store, None, "b@gmail.com", now).await;
    assert_eq!(sel.as_deref(), Some("overflow"));
    assert_eq!(ev[0].outcome, Err(SkipReason::Quota));
    assert_eq!(
        tat(&store, "google").await,
        after_first,
        "the slot was given back"
    );
}

#[sqlx::test]
async fn a_downstream_failure_gives_the_slot_back(pool: PgPool) {
    // Burst 1, max_wait 0. The first message is refused 451 at the final dot,
    // so its slot comes back, and the second goes out on the warming route at
    // once rather than steering to overflow.
    let warm = FakeDownstream::start(Script::with(|s| {
        s.transactions = vec![Turn {
            final_dot: Some(Act::Reply(451, "4.3.0 try later")),
            ..Turn::default()
        }];
    }))
    .await;
    let over = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start_with_quota(
        &config(
            warm.addr.port(),
            over.addr.port(),
            "      per_hour: 1\n",
            1000,
        ),
        store(pool),
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(
        c.deliver("jane@oldbrand.com", "a@gmail.com", BODY)
            .await
            .code,
        451
    );
    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(
        c.deliver("jane@oldbrand.com", "b@gmail.com", BODY)
            .await
            .code,
        250
    );
    // The fake records only what it accepted; both MAIL FROMs reached it.
    assert_eq!(warm.command_count("MAIL FROM"), 2, "both went to warming");
    assert_eq!(warm.messages().len(), 1);
    assert!(
        over.messages().is_empty(),
        "the slot was there for the second"
    );
}

#[sqlx::test]
async fn an_ambiguous_final_dot_keeps_the_slot_spent(pool: PgPool) {
    // §10.2: the downstream may have the message. The slot is not given back,
    // so the second message finds none and steers.
    let warm = FakeDownstream::start(Script::with(|s| s.final_dot = Act::Drop)).await;
    let over = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start_with_quota(
        &config(
            warm.addr.port(),
            over.addr.port(),
            "      per_hour: 1\n",
            1000,
        ),
        store(pool),
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(
        c.deliver("jane@oldbrand.com", "a@gmail.com", BODY)
            .await
            .code,
        451
    );
    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(
        c.deliver("jane@oldbrand.com", "b@gmail.com", BODY)
            .await
            .code,
        250
    );
    assert_eq!(over.messages().len(), 1, "steered: the slot stayed spent");
}

#[tokio::test]
async fn a_reservation_error_gives_the_slot_back() {
    let cfg = cfg("      per_hour: 1\n", 1000);
    let fake = Arc::new(GrantAllQuota::new());
    fake.fail_reserves();
    let store: Arc<dyn QuotaStore> = fake.clone();
    let mut evaluation = Vec::new();
    let chain = vec!["warming".to_string(), "overflow".to_string()];
    let walked = chain::walk_and_reserve(
        cfg.default_ramp(),
        &simmer::routing::domain_group::Grouper::literal(),
        &cfg.dot_insensitive_domains,
        &store,
        &Frequency::new(),
        &simmer::preflight::Registry::new(),
        &chain,
        None,
        &["a@gmail.com".to_string()],
        "rate-test",
        &mut evaluation,
        at(hours(1)),
    )
    .await;
    assert!(walked.is_err(), "§7.5 decides the reply");
    let unbooked = fake.unbooked();
    assert_eq!(unbooked.len(), 1);
    assert_eq!(
        (
            unbooked[0].route.as_str(),
            unbooked[0].domain_group.as_str()
        ),
        ("warming", "google")
    );
    assert!(
        fake.rate_tats("main")
            .await
            .unwrap()
            .values()
            .all(|t| *t == at(hours(1))),
        "back to a full bucket"
    );
}

// ---------------------------------------------------------------------------
// D-111's pinned reply
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn a_pinned_reply_books_past_the_limit_and_is_never_skipped_for_rate(pool: PgPool) {
    let cfg = cfg("      per_hour: 1\n", 100_000);
    let store = store(pool);
    let now = at(hours(1));
    walk(&cfg, &store, None, "a@gmail.com", now).await;
    let spent = tat(&store, "google").await.unwrap();

    // An ordinary message steers...
    assert_eq!(
        walk(&cfg, &store, None, "b@gmail.com", now)
            .await
            .0
            .as_deref(),
        Some("overflow")
    );
    // ...and the dry run says the pinned one would go, over the limit.
    let d = dry(&cfg, &store, Some("warming"), "c@gmail.com", now).await;
    assert_eq!(chain::render(&d), "warming=selected");
    assert!(d[0].rate.unwrap().over_limit);
    assert_eq!(
        tat(&store, "google").await,
        Some(spent),
        "the dry run booked nothing"
    );

    // A reply pinned to warming goes now, counted against the bucket.
    let (sel, ev) = walk(&cfg, &store, Some("warming"), "c@gmail.com", now).await;
    assert_eq!(sel.as_deref(), Some("warming"));
    let step = ev[0].rate.unwrap();
    assert!(step.over_limit);
    assert_eq!(step.send_at, now, "sent now, not held");
    assert_eq!(
        tat(&store, "google").await,
        Some(spent + hours(1)),
        "one more interval on the bucket"
    );
}
