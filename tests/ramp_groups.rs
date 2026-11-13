//! D-116's ramp groups through the real walk, against real Postgres.
//!
//! `tests/store_conformance/` owns the storage contract on both backends: both
//! rows counted, the group row locked first, the sweeper. What only the walk
//! can show is here: that a full group **steers** a warming route's message to
//! overflow with `group_quota`, that a route's own cap is still reported as
//! `quota`, that overflow never touches the group, that the group's ceiling
//! moves by D-115's rule on its own committed total, that a pinned reply passes
//! it as D-090 allows, and that a ramp outside every group behaves as before.
//!
//! The fixture: ramps `m` and `p`, each with a warming route `warm` and an
//! overflow, sharing group `g`; ramp `solo`, the same shape in no group; and
//! ramp `strict`, in `g` with no overflow route (D-113). Every route and the
//! group start at [`START`]; each test picks the instant it walks at, so a day
//! is a `now` rather than a fresh configuration.

#![cfg(feature = "postgres")]

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use simmer::config::Config;
use simmer::frequency::Frequency;
use simmer::quota::store::QuotaStore;
use simmer::quota::{self, PgQuotaStore};
use simmer::routing::chain::{self, SkipReason, Walk};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;

const START: &str = "2026-01-01T09:00:00Z";

fn start() -> DateTime<Utc> {
    START.parse().unwrap()
}

/// Midday of `day`, by the group's (and every route's) clock.
fn day(n: i64) -> DateTime<Utc> {
    start() + Duration::hours(24 * n + 12)
}

/// `group_schedule` and `group_advance` configure `g`; `m` and `p` carry route
/// schedules `m_schedule` / `p_schedule` under `advance: auto`, so a route's
/// own allowance is never in question.
fn config_with(group: &str) -> Config {
    let ramp = |name: &str, schedule: &str, overflow: bool| {
        let (no_overflow, chain, overflow_route) = if overflow {
            (
                "",
                "[warm, overflow]",
                r#"
  - name: overflow
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: 2526
      tls: off
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@established.com" }"#,
            )
        } else {
            ("\n  allow_no_overflow_route: true", "[warm]", "")
        };
        format!(
            r#"
 {name}:
  domain_groups:
  - {{ name: catchall, domains: ["*"] }}
  senders:
  - {{ match: "app.example", match_on: envelope, chain: {chain} }}
  default_chain: {chain}{no_overflow}
  warmup_advance: auto
  routes:
  - name: warm
    downstream:
      host: "127.0.0.1"
      port: 2525
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
    identity: {{ envelope_from: "b@{name}.newbrand.com" }}
    warmup:
      started: "{START}"
      schedule: {{ default: {schedule} }}{overflow_route}"#
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
default_ramp: m
ramps:{m}{p}{solo}{strict}
ramp_groups:
  g:
    ramps: [m, p, strict]
    started: "{START}"
{group}"#,
        m = ramp("m", "[80, 160, 320]", true),
        p = ramp("p", "[50, 100, 200]", true),
        solo = ramp("solo", "[80, 160, 320]", true),
        strict = ramp("strict", "[1000]", false),
    );
    simmer::config::from_str(&yaml, "test").expect("fixture is valid")
}

/// The group of the plan's simulations: `[100, 200, 400]` at threshold 0.9.
fn config() -> Config {
    config_with("    schedule: [100, 200, 400]\n")
}

fn store(pool: PgPool) -> Arc<dyn QuotaStore> {
    Arc::new(PgQuotaStore::new(pool))
}

/// One walk's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sent {
    route: Option<String>,
    /// The warming route's skip reason, if it was skipped.
    skipped: Option<SkipReason>,
    over_cap: bool,
    ramp_group: Option<String>,
}

/// Walk `ramp`'s chain once at `now` and, if anything was selected, commit it
/// as a downstream `2xx` would (or release it, with `deliver: false`).
async fn walk_once(
    cfg: &Config,
    store: &Arc<dyn QuotaStore>,
    ramp: &str,
    pinned: Option<&str>,
    now: DateTime<Utc>,
    deliver: bool,
) -> Sent {
    let ramp = cfg.ramp(ramp).expect("ramp");
    let chain = ramp.default_chain.clone().expect("a default chain");
    let mut evaluation = Vec::new();
    let walked = chain::walk_and_reserve(
        ramp,
        &simmer::routing::domain_group::Grouper::literal(),
        &cfg.dot_insensitive_domains,
        store,
        &Frequency::new(),
        &simmer::preflight::Registry::new(),
        &chain,
        pinned,
        &["reader@example.com".to_string()],
        "ramp-groups-test",
        &mut evaluation,
        now,
    )
    .await
    .expect("walk");
    let skipped = evaluation
        .iter()
        .find(|s| s.route == "warm")
        .and_then(|s| s.outcome.err());
    match walked {
        Walk::Selected(s) => {
            let out = Sent {
                route: Some(s.route.name.clone()),
                skipped,
                over_cap: s.over_cap,
                ramp_group: s.ramp_group.as_ref().map(|g| g.name.clone()),
            };
            if deliver {
                store.commit(&s.reservation, &[]).await.expect("commit");
            } else {
                store.release(&s.reservation).await.expect("release");
            }
            out
        }
        Walk::Exhausted => Sent {
            route: None,
            skipped,
            over_cap: false,
            ramp_group: None,
        },
    }
}

async fn send(cfg: &Config, store: &Arc<dyn QuotaStore>, ramp: &str, now: DateTime<Utc>) -> Sent {
    walk_once(cfg, store, ramp, None, now, true).await
}

async fn group_position(cfg: &Config, store: &Arc<dyn QuotaStore>, n: i64) -> quota::GroupPosition {
    quota::group_position(store.as_ref(), cfg.ramp_groups.get("g").unwrap(), day(n))
        .await
        .expect("group position")
}

async fn committed(store: &Arc<dyn QuotaStore>, ramp: &str, n: i64) -> i64 {
    store
        .usage(ramp, "warm", "catchall", n)
        .await
        .expect("usage")
        .committed
}

// -- simulation 1: the shared cap steers the excess to overflow ---------------

#[sqlx::test]
async fn a_full_group_steers_every_further_warming_message_to_overflow(pool: PgPool) {
    let (cfg, store) = (config(), store(pool));
    let mut warm = 0;
    let mut group_quota = 0;
    for i in 0..130 {
        let ramp = if i % 2 == 0 { "m" } else { "p" };
        let s = send(&cfg, &store, ramp, day(0)).await;
        match s.route.as_deref() {
            Some("warm") => {
                warm += 1;
                assert_eq!(s.ramp_group.as_deref(), Some("g"));
            }
            Some("overflow") => {
                // Overflow never counts against the group.
                assert_eq!(s.ramp_group, None);
                if s.skipped == Some(SkipReason::GroupQuota) {
                    group_quota += 1;
                }
            }
            other => panic!("an overflow route is always eligible: {other:?}"),
        }
    }
    assert_eq!(warm, 100, "the group's day-0 ceiling, exactly");
    assert_eq!(
        group_quota, 30,
        "every message past it, with the group's reason"
    );

    let g = store.group_usage("g", 0).await.unwrap();
    assert_eq!((g.allowance, g.committed, g.reserved), (Some(100), 100, 0));
    assert_eq!(
        committed(&store, "m", 0).await + committed(&store, "p", 0).await,
        100
    );

    // 100 of 100 met the 0.9 bar: day 1 is step 1, a ceiling of 200.
    let next = group_position(&cfg, &store, 1).await;
    assert_eq!((next.step, next.allowance), (1, Some(200)));
    assert_eq!(next.decision.outcome, quota::advance::Outcome::Advanced);
}

// -- simulation 2: a group that falls short is held -----------------------

#[sqlx::test]
async fn a_group_that_misses_its_threshold_is_held_while_routes_advance(pool: PgPool) {
    let (cfg, store) = (config(), store(pool));
    for _ in 0..100 {
        send(&cfg, &store, "m", day(0)).await; // m's route takes 80, then overflow
    }
    // m's 80 did not fill the group: top it up from p.
    for _ in 0..20 {
        send(&cfg, &store, "p", day(0)).await;
    }
    assert_eq!(store.group_usage("g", 0).await.unwrap().committed, 100);

    // Day 1, ceiling 200: commit only 150 (180 needed).
    for i in 0..150 {
        let ramp = if i % 3 == 0 { "p" } else { "m" };
        let s = send(&cfg, &store, ramp, day(1)).await;
        assert_eq!(
            s.route.as_deref(),
            Some("warm"),
            "message {i} is under every cap"
        );
    }
    let held = group_position(&cfg, &store, 2).await;
    assert_eq!((held.step, held.allowance), (1, Some(200)));
    assert_eq!(held.held_days(), 1);
    assert_eq!(held.decision.outcome, quota::advance::Outcome::Held);

    // The routes' own schedules (auto) advanced regardless.
    let m = cfg.ramp("m").unwrap().route("warm").unwrap();
    let p = quota::position(store.as_ref(), m, "catchall", 2, Default::default())
        .await
        .unwrap();
    assert_eq!(p.allowance.as_column(), Some(320));

    // And day 2 enforces the held 200, not the schedule's 400.
    let mut warm = 0;
    for i in 0..260 {
        let ramp = if i % 2 == 0 { "m" } else { "p" };
        if send(&cfg, &store, ramp, day(2)).await.route.as_deref() == Some("warm") {
            warm += 1;
        }
    }
    assert_eq!(warm, 200);
}

// -- simulation 3: the route's own cap is still `quota` ---------------------

#[sqlx::test]
async fn a_full_route_in_a_group_with_room_is_skipped_for_quota(pool: PgPool) {
    let (cfg, store) = (config(), store(pool));
    for _ in 0..50 {
        assert_eq!(
            send(&cfg, &store, "p", day(0)).await.route.as_deref(),
            Some("warm")
        );
    }
    let s = send(&cfg, &store, "p", day(0)).await;
    assert_eq!(s.route.as_deref(), Some("overflow"));
    assert_eq!(s.skipped, Some(SkipReason::Quota), "the group still has 50");
    assert_eq!(store.group_usage("g", 0).await.unwrap().committed, 50);
}

// -- simulation 4: two instances, two ramps, the last slots ----------------

#[sqlx::test]
async fn racing_instances_across_ramps_never_overshoot_the_group(
    opts: PgPoolOptions,
    conn: PgConnectOptions,
) {
    // sqlx's test pools cap a child at the parent's 16.
    const N: usize = 16;
    let opts = opts.max_connections(N as u32);
    let a = store(opts.clone().connect_with(conn.clone()).await.unwrap());
    let b = store(opts.connect_with(conn).await.unwrap());
    let cfg = Arc::new(config());

    // Spend 95 of the 100 first, so the race is for the last 5.
    for i in 0..95 {
        let ramp = if i % 2 == 0 { "m" } else { "p" };
        send(&cfg, &a, ramp, day(0)).await;
    }

    // Warm both pools so every contender has its own connection (D-084).
    let gate = Arc::new(tokio::sync::Barrier::new(N));
    let mut warmers = Vec::new();
    for i in 0..N {
        let s = Arc::clone(if i % 2 == 0 { &a } else { &b });
        let gate = Arc::clone(&gate);
        warmers.push(tokio::spawn(async move {
            let _ = s.route_states("m").await;
            gate.wait().await;
        }));
    }
    for w in warmers {
        w.await.unwrap();
    }

    let gate = Arc::new(tokio::sync::Barrier::new(N));
    let mut handles = Vec::new();
    for i in 0..N {
        let s = Arc::clone(if i % 2 == 0 { &a } else { &b });
        let cfg = Arc::clone(&cfg);
        let gate = Arc::clone(&gate);
        handles.push(tokio::spawn(async move {
            let ramp = if i % 3 == 0 { "p" } else { "m" };
            gate.wait().await;
            send(&cfg, &s, ramp, day(0)).await
        }));
    }
    let mut warm = 0;
    for h in handles {
        let s = h.await.unwrap();
        match s.route.as_deref() {
            Some("warm") => warm += 1,
            Some("overflow") => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(warm, 5);
    let g = a.group_usage("g", 0).await.unwrap();
    assert_eq!((g.committed, g.reserved), (100, 0));
}

// -- simulation 5: a pinned reply passes a full group, counted on both -----

#[sqlx::test]
async fn a_pinned_reply_passes_a_full_group_and_counts_on_both_rows(pool: PgPool) {
    let (cfg, store) = (config(), store(pool));
    for i in 0..100 {
        send(&cfg, &store, if i % 2 == 0 { "m" } else { "p" }, day(0)).await;
    }
    let before_m = committed(&store, "m", 0).await;

    let reply = walk_once(&cfg, &store, "m", Some("warm"), day(0), true).await;
    assert_eq!(reply.route.as_deref(), Some("warm"));
    assert!(reply.over_cap, "reserved past the group's cap (D-090)");
    assert_eq!(reply.ramp_group.as_deref(), Some("g"));

    let g = store.group_usage("g", 0).await.unwrap();
    assert_eq!((g.committed, g.allowance), (101, Some(100)));
    assert_eq!(committed(&store, "m", 0).await, before_m + 1);

    // Nothing else is let through.
    let next = send(&cfg, &store, "m", day(0)).await;
    assert_eq!(next.route.as_deref(), Some("overflow"));
    assert_eq!(next.skipped, Some(SkipReason::GroupQuota));
}

// -- simulation 6: a failed send gives both rows back -----------------------

#[sqlx::test]
async fn a_failed_send_releases_the_group_row_too(pool: PgPool) {
    let (cfg, store) = (config(), store(pool));
    let s = walk_once(&cfg, &store, "m", None, day(0), false).await;
    assert_eq!(s.route.as_deref(), Some("warm"));
    let g = store.group_usage("g", 0).await.unwrap();
    assert_eq!((g.committed, g.reserved), (0, 0));
    let u = store.usage("m", "warm", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved), (0, 0));
}

// -- simulation 7: a ramp outside every group is unchanged ------------------

#[sqlx::test]
async fn a_ramp_in_no_group_ignores_a_full_group(pool: PgPool) {
    let (cfg, store) = (config(), store(pool));
    assert!(cfg.ramp("solo").unwrap().ramp_group.is_none());
    for i in 0..100 {
        send(&cfg, &store, if i % 2 == 0 { "m" } else { "p" }, day(0)).await;
    }
    assert_eq!(store.group_usage("g", 0).await.unwrap().committed, 100);

    // solo's route takes its full 80 and then spills for `quota`, exactly as
    // with no `ramp_groups` block at all, and never names a group.
    let plain = {
        let mut c = config();
        c.ramp_groups = Default::default();
        c.link_ramp_groups();
        c
    };
    for i in 0..80 {
        let s = send(&cfg, &store, "solo", day(0)).await;
        assert_eq!(s.route.as_deref(), Some("warm"), "message {i}");
        assert_eq!(s.ramp_group, None);
    }
    let s = send(&cfg, &store, "solo", day(0)).await;
    assert_eq!(
        (s.route.as_deref(), s.skipped),
        (Some("overflow"), Some(SkipReason::Quota))
    );
    // The group row did not move.
    assert_eq!(store.group_usage("g", 0).await.unwrap().committed, 100);

    // And the same walk against a configuration with no groups at all agrees,
    // step for step, on the next day.
    let mut with_groups = Vec::new();
    let mut without = Vec::new();
    for _ in 0..3 {
        with_groups.push(send(&cfg, &store, "solo", day(1)).await);
    }
    for _ in 0..3 {
        without.push(send(&plain, &store, "solo", day(1)).await);
    }
    assert_eq!(with_groups, without);
}

// -- simulation 8: `advance: auto` on the group ----------------------------

#[sqlx::test]
async fn an_auto_group_follows_its_day_whatever_it_sent(pool: PgPool) {
    let cfg = config_with("    schedule: [100, 200, 400]\n    advance: auto\n");
    let store = store(pool);
    send(&cfg, &store, "m", day(0)).await; // 1 of 100
    for n in 0..4 {
        let p = group_position(&cfg, &store, n).await;
        assert_eq!(p.step, n);
        assert_eq!(p.allowance, Some([100, 200, 400, 400][n as usize]));
    }

    // And the threshold rule, from the same single message, holds at step 0.
    let thr = config();
    let held = group_position(&thr, &store, 3).await;
    assert_eq!((held.step, held.allowance), (0, Some(100)));
}

// -- the edges ----------------------------------------------------------

#[sqlx::test]
async fn a_group_not_yet_started_caps_nothing_and_writes_no_row(pool: PgPool) {
    let (cfg, store) = (config(), store(pool));
    let before = start() - Duration::hours(1);
    // The routes have not started either at `before`, so move the routes back
    // and leave the group in the future.
    let yaml_cfg = {
        let mut c = cfg.clone();
        for ramp in &mut c.ramps.0 {
            for r in &mut ramp.routes {
                if let Some(w) = r.warmup.as_mut() {
                    w.started = start() - Duration::days(10);
                }
            }
        }
        c.link_ramp_groups();
        c
    };
    // Day 9 for the routes, whose own cap (the final 320) is all that binds:
    // the group's 100 would have refused the rest had it been in force.
    for i in 0..200 {
        let s = send(&yaml_cfg, &store, "m", before).await;
        assert_eq!(s.route.as_deref(), Some("warm"), "message {i}");
        assert_eq!(s.ramp_group, None);
    }
    assert_eq!(committed(&store, "m", 9).await, 200);
    assert_eq!(
        store.group_usage("g", -1).await.unwrap(),
        Default::default()
    );
    let p = quota::group_position(store.as_ref(), cfg.ramp_groups.get("g").unwrap(), before)
        .await
        .unwrap();
    assert_eq!(p.allowance, None);
}

#[sqlx::test]
async fn a_full_group_on_a_no_overflow_ramp_gets_its_usual_policy(pool: PgPool) {
    // D-113: a full group leaves `strict` with nothing eligible, exactly as its
    // own cap would — the ramp's no-overflow policy answers, not a drop.
    let (cfg, store) = (config(), store(pool));
    for i in 0..100 {
        send(&cfg, &store, if i % 2 == 0 { "m" } else { "p" }, day(0)).await;
    }
    let s = send(&cfg, &store, "strict", day(0)).await;
    assert_eq!(s.route, None);
    assert_eq!(s.skipped, Some(SkipReason::GroupQuota));

    let ramp = cfg.ramp("strict").unwrap();
    let eligible = chain::any_eligible(
        ramp,
        &simmer::routing::domain_group::Grouper::literal(),
        &store,
        &["warm".to_string()],
        "reader@example.com",
        day(0),
    )
    .await
    .unwrap();
    assert!(!eligible, "the early check sees the full group too");
}

#[sqlx::test]
async fn the_dry_run_reports_the_group_and_agrees_with_the_walk(pool: PgPool) {
    let (cfg, store) = (config(), store(pool));
    let dry = |cfg: Config, store: Arc<dyn QuotaStore>| async move {
        chain::dry_walk(
            cfg.ramp("m").unwrap(),
            &simmer::routing::domain_group::Grouper::literal(),
            &cfg.dot_insensitive_domains,
            &store,
            &Frequency::new(),
            &simmer::preflight::Registry::new(),
            &["warm".to_string(), "overflow".to_string()],
            None,
            "reader@example.com",
            day(0),
        )
        .await
        .unwrap()
    };

    let steps = dry(cfg.clone(), Arc::clone(&store)).await;
    assert_eq!(steps[0].outcome, Ok(()));
    let g = steps[0].ramp_group.as_ref().expect("the group is reported");
    assert_eq!(
        (g.name.as_str(), g.step, g.allowance, g.headroom),
        ("g", 0, 100, Some(100))
    );

    for i in 0..100 {
        send(&cfg, &store, if i % 2 == 0 { "m" } else { "p" }, day(0)).await;
    }
    let steps = dry(cfg.clone(), Arc::clone(&store)).await;
    assert_eq!(steps[0].outcome, Err(SkipReason::GroupQuota));
    assert_eq!(steps[0].ramp_group.as_ref().unwrap().headroom, Some(0));
    assert_eq!(steps[1].outcome, Ok(()));
    assert_eq!(steps[1].ramp_group, None, "overflow never counts");

    // The real walk, at the same instant, says the same.
    let s = send(&cfg, &store, "m", day(0)).await;
    assert_eq!(
        (s.route.as_deref(), s.skipped),
        (Some("overflow"), Some(SkipReason::GroupQuota))
    );
    // And it counted nothing in the dry run.
    assert_eq!(store.group_usage("g", 0).await.unwrap().committed, 100);
}

#[sqlx::test]
async fn the_group_row_keeps_the_ceiling_it_was_created_with(pool: PgPool) {
    // D-026 for the group: an edited schedule does not move a day already
    // being spent.
    let (cfg, store) = (config(), store(pool));
    send(&cfg, &store, "m", day(0)).await;
    let raised = config_with("    schedule: [5000]\n");
    assert_eq!(
        group_position(&raised, &store, 0).await.allowance,
        Some(100)
    );
    let mut warm = 1;
    for i in 0..150 {
        if send(&raised, &store, if i % 2 == 0 { "m" } else { "p" }, day(0))
            .await
            .route
            .as_deref()
            == Some("warm")
        {
            warm += 1;
        }
    }
    assert_eq!(warm, 100);
}
