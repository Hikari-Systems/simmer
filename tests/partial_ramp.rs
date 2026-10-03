//! D-091's partial ramp through the real walk, against real Postgres.
//!
//! `src/routing/partial.rs`'s unit tests own the hash: determinism, the offered
//! fraction, and what changes the decision. What only the walk can show is here:
//! that a message outside the share **steers** to the next link without touching
//! the warming route's row, that one inside it reserves as normal, that the
//! day's entry is the one applied, that the gate lifts past the end of `share`
//! and on graduation, that a pinned reply is exempt, and that §9.4's dry run
//! gives the real walk's answer for every recipient.

// Postgres-backed, as tests/frequency.rs is. The walk is backend-neutral; the
// store underneath is what the fixture needs.
#![cfg(feature = "postgres")]

use std::sync::Arc;

use chrono::Utc;
use simmer::frequency::{Frequency, Keyer};
use simmer::quota::store::QuotaStore;
use simmer::quota::PgQuotaStore;
use simmer::routing::chain::{self, SkipReason, Walk};
use simmer::routing::partial;
use sqlx::PgPool;

/// A warming route with a partial ramp in front of an overflow, started
/// `day` and a half days ago. The cap is high enough that quota never refuses
/// — a route skipped for the wrong reason would still look like a pass.
fn config(day: i64, share: &[f64]) -> simmer::config::Config {
    let started = (Utc::now() - chrono::Duration::hours(24 * day + 12)).to_rfc3339();
    let share = format!("{share:?}");
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
  - {{ match: "oldbrand.com", match_on: envelope, chain: [warming, overflow] }}
  default_chain: [overflow]
  routes:
  - name: warming
    downstream:
      host: "127.0.0.1"
      port: 2525
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
    identity: {{ envelope_from: "b@newbrand.com" }}
    warmup:
      started: "{started}"
      schedule: {{ default: [100000], share: {share} }}
  - name: overflow
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: 2526
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
    identity: {{ envelope_from: "b@established.com" }}
"#
    );
    simmer::config::from_str(&yaml, "test").expect("fixture is valid")
}

fn store(pool: PgPool) -> Arc<dyn QuotaStore> {
    Arc::new(PgQuotaStore::new(pool))
}

/// The instance's keyer, so that the expected answers are computed with the
/// salt the walk itself resolves (tests/frequency.rs's `keyer`, for the same
/// reason).
async fn keyer(store: &Arc<dyn QuotaStore>) -> Keyer {
    Keyer::new(store.recipient_hash_salt().await.expect("salt"))
}

fn day_index(cfg: &simmer::config::Config) -> i64 {
    simmer::quota::day::for_route(
        cfg.default_ramp().route("warming").expect("route"),
        Utc::now(),
    )
}

fn recipients(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("reader{i}@example.com")).collect()
}

async fn walk(
    cfg: &simmer::config::Config,
    store: &Arc<dyn QuotaStore>,
    pinned: Option<&str>,
    recipient: &str,
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
        "test-correlation",
        &mut evaluation,
        chrono::Utc::now(),
    )
    .await
    .expect("walk");
    let selected = match walked {
        Walk::Selected(s) => Some(s.route.name.clone()),
        Walk::Exhausted => None,
    };
    (selected, evaluation)
}

async fn warming_reserved(store: &Arc<dyn QuotaStore>, cfg: &simmer::config::Config) -> i64 {
    store
        .usage("main", "warming", "catchall", day_index(cfg))
        .await
        .expect("usage")
        .reserved
}

#[sqlx::test]
async fn a_message_outside_the_share_steers_and_one_inside_reserves(pool: PgPool) {
    let cfg = config(0, &[0.4]);
    let store = store(pool);
    let keyer = keyer(&store).await;
    let day = day_index(&cfg);
    assert_eq!(day, 0);

    let (mut steered, mut offered) = (0, 0);
    for r in recipients(60) {
        let before = warming_reserved(&store, &cfg).await;
        let (selected, evaluation) = walk(&cfg, &store, None, &r).await;
        let expected = partial::offered(&keyer, "main", "warming", &r, day, 0.4, &[]);

        if expected {
            offered += 1;
            assert_eq!(selected.as_deref(), Some("warming"), "{r}");
            assert_eq!(warming_reserved(&store, &cfg).await, before + 1);
            assert_eq!(chain::render(&evaluation), "warming=selected");
        } else {
            steered += 1;
            assert_eq!(selected.as_deref(), Some("overflow"), "{r}");
            assert_eq!(
                evaluation[0].outcome,
                Err(SkipReason::PartialRamp),
                "steered by the partial ramp and not by anything else"
            );
            assert_eq!(
                chain::render(&evaluation),
                "warming=partial_ramp,overflow=selected"
            );
            // Turned away before the reservation: the row is untouched.
            assert_eq!(warming_reserved(&store, &cfg).await, before);
        }
    }
    // Both branches exercised, or the assertions above prove half of nothing.
    assert!(
        steered > 0 && offered > 0,
        "steered {steered}, offered {offered}"
    );
}

/// How many of `n` recipients the walk put on the warming route.
async fn on_warming(cfg: &simmer::config::Config, store: &Arc<dyn QuotaStore>, n: usize) -> usize {
    let mut hits = 0;
    for r in recipients(n) {
        if walk(cfg, store, None, &r).await.0.as_deref() == Some("warming") {
            hits += 1;
        }
    }
    hits
}

#[sqlx::test]
async fn the_days_entry_is_the_one_applied(pool: PgPool) {
    // Day 1 of [0.01, 1.0] offers everything; day 1 of [1.0, 0.01] almost
    // nothing. Reading the wrong index would invert the two.
    let store = store(pool);
    assert_eq!(on_warming(&config(1, &[0.01, 1.0]), &store, 40).await, 40);
    assert!(on_warming(&config(1, &[1.0, 0.01]), &store, 40).await <= 2);
}

#[sqlx::test]
async fn the_gate_lifts_past_the_end_of_the_share_list(pool: PgPool) {
    // Day 2 of a two-entry list: every message is offered, which is NOT §7.2's
    // "final value repeats" — that would throttle the route forever.
    let store = store(pool);
    assert_eq!(on_warming(&config(2, &[0.01, 0.01]), &store, 40).await, 40);
}

#[sqlx::test]
async fn a_graduated_route_is_offered_everything(pool: PgPool) {
    let cfg = config(0, &[0.01]);
    let store = store(pool);
    store
        .set_graduated("main", "warming", true)
        .await
        .expect("graduate");
    for r in recipients(40) {
        let (selected, _) = walk(&cfg, &store, None, &r).await;
        assert_eq!(selected.as_deref(), Some("warming"), "{r}");
    }
}

#[sqlx::test]
async fn a_pinned_reply_is_exempt(pool: PgPool) {
    let cfg = config(0, &[0.2]);
    let store = store(pool);
    let keyer = keyer(&store).await;
    let day = day_index(&cfg);

    let gated = recipients(100)
        .into_iter()
        .find(|r| !partial::offered(&keyer, "main", "warming", r, day, 0.2, &[]))
        .expect("at 20%, some recipient is outside the share");

    let (unpinned, _) = walk(&cfg, &store, None, &gated).await;
    assert_eq!(unpinned.as_deref(), Some("overflow"));

    // D-090: the reply stays on the route that started the thread.
    let (pinned, evaluation) = walk(&cfg, &store, Some("warming"), &gated).await;
    assert_eq!(pinned.as_deref(), Some("warming"));
    assert_eq!(chain::render(&evaluation), "warming=selected");
}

#[sqlx::test]
async fn dry_run_gives_the_real_walks_answer(pool: PgPool) {
    let cfg = config(0, &[0.5]);
    let store = store(pool);
    let chain = vec!["warming".to_string(), "overflow".to_string()];

    for r in recipients(40) {
        let dry = chain::dry_walk(
            cfg.default_ramp(),
            &simmer::routing::domain_group::Grouper::literal(),
            &cfg.dot_insensitive_domains,
            &store,
            &Frequency::new(),
            &simmer::preflight::Registry::new(),
            &chain,
            None,
            &r,
            Utc::now(),
        )
        .await
        .expect("dry walk");
        let (_, real) = walk(&cfg, &store, None, &r).await;
        assert_eq!(chain::render(&dry), chain::render(&real), "{r}");
    }
}

// -- D-097: share: {mode: auto} ---------------------------------------------

/// A warming route whose share is computed, `hours` into day 0 of its ramp,
/// with an explicit cap so the controller has something to pace against.
///
/// `google` gets its own, smaller cap, so that "the share is per domain group"
/// is a thing the fixture can show rather than a thing the code asserts.
fn auto_config(hours: f64, cap: i64, google_cap: i64, params: &str) -> simmer::config::Config {
    let started =
        (Utc::now() - chrono::Duration::milliseconds((hours * 3_600_000.0) as i64)).to_rfc3339();
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
  - {{ name: google, domains: ["gmail.com"] }}
  - {{ name: catchall, domains: ["*"] }}
  senders:
  - {{ match: "oldbrand.com", match_on: envelope, chain: [warming, overflow] }}
  default_chain: [overflow]
  routes:
  - name: warming
    downstream:
      host: "127.0.0.1"
      port: 2525
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
    identity: {{ envelope_from: "b@newbrand.com" }}
    warmup:
      started: "{started}"
      schedule:
        default: [{cap}]
        overrides: {{ google: [{google_cap}] }}
        share: {{ mode: auto{params} }}
  - name: overflow
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: 2526
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
    identity: {{ envelope_from: "b@established.com" }}
"#
    );
    simmer::config::from_str(&yaml, "test").expect("fixture is valid")
}

/// Push a group's row up to `count` used, the way a day's traffic would, without
/// walking anything: the reservation protocol's own phase 1, which is what the
/// controller reads.
async fn fill(store: &Arc<dyn QuotaStore>, cfg: &simmer::config::Config, group: &str, count: i64) {
    use simmer::quota::store::{ReserveRequest, Reserved};
    let taken = store
        .reserve(&ReserveRequest {
            ramp: "main".into(),
            route: "warming".to_string(),
            domain_group: group.to_string(),
            day_index: day_index(cfg),
            allowance: None,
            count,
            correlation_id: "fill".to_string(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            over_cap: false,
        })
        .await
        .expect("reserve");
    assert!(
        matches!(taken, Reserved::Taken(_)),
        "the fixture must be able to take {count}"
    );
}

/// How many of `n` recipients the **dry run** says would be offered to the
/// warming route. The dry run, so that counting does not itself fill the cap and
/// move the number being counted.
async fn offered_count(
    cfg: &simmer::config::Config,
    store: &Arc<dyn QuotaStore>,
    n: usize,
) -> usize {
    let chain = vec!["warming".to_string(), "overflow".to_string()];
    let mut offered = 0;
    for r in recipients(n) {
        let dry = chain::dry_walk(
            cfg.default_ramp(),
            &simmer::routing::domain_group::Grouper::literal(),
            &cfg.dot_insensitive_domains,
            store,
            &Frequency::new(),
            &simmer::preflight::Registry::new(),
            &chain,
            None,
            &r,
            Utc::now(),
        )
        .await
        .expect("dry walk");
        let steered = dry
            .iter()
            .any(|s| s.route == "warming" && s.outcome == Err(SkipReason::PartialRamp));
        if !steered {
            offered += 1;
        }
    }
    offered
}

#[sqlx::test]
async fn the_auto_share_falls_as_the_cap_fills(pool: PgPool) {
    // A fifth of the way into the day, against a fill window ending at 60% of
    // it: on pace would be a third of the cap gone.
    let cfg = auto_config(
        24.0 * 0.2,
        200,
        200,
        ", ceiling: 0.8, floor: 0.01, tail: {below: 0}",
    );
    let store = store(pool);

    let empty = offered_count(&cfg, &store, 200).await;
    fill(&store, &cfg, "catchall", 120).await;
    let filled = offered_count(&cfg, &store, 200).await;

    assert!(
        filled < empty,
        "the share must fall as the cap fills: {empty} offered empty, {filled} at 120/200"
    );

    // And the walk agrees with what the dry run just counted: a message the
    // share turns away steers without touching the row.
    let before = warming_reserved(&store, &cfg).await;
    let mut steered = 0;
    for r in recipients(200) {
        let (_, eval) = walk(&cfg, &store, None, &r).await;
        if eval
            .iter()
            .any(|s| s.route == "warming" && s.outcome == Err(SkipReason::PartialRamp))
        {
            steered += 1;
        }
    }
    assert!(steered > 0, "some messages must have been turned away");
    let after = warming_reserved(&store, &cfg).await;
    assert_eq!(
        after - before,
        200 - steered,
        "every message not turned away, and only those, reserved"
    );
}

#[sqlx::test]
async fn the_tail_release_closes_the_cap(pool: PgPool) {
    // Early in the day and far ahead of pace, so the controller alone would sit
    // at its floor and the last of the cap would take all day to go out.
    let cfg = auto_config(
        1.0,
        200,
        200,
        ", floor: 0.01, tail: {below: 0.1, ceiling: 1.0}",
    );
    let store = store(pool);

    // 21 left of 200 is above a tenth: still throttled.
    fill(&store, &cfg, "catchall", 179).await;
    let throttled = offered_count(&cfg, &store, 100).await;
    assert!(
        throttled < 100,
        "above the threshold the route is still being throttled, got {throttled}/100"
    );

    // 20 left is the threshold, and the release is unconditional from there.
    fill(&store, &cfg, "catchall", 1).await;
    assert_eq!(
        offered_count(&cfg, &store, 100).await,
        100,
        "under the tail threshold every message must be offered, or the ramp \
         never finishes its cap"
    );
}

#[sqlx::test]
async fn past_the_fill_window_every_message_is_offered(pool: PgPool) {
    // 80% through the day, with the window closing at 60% and half the cap
    // still to go. The share opens to its ceiling, which here is 1.
    let cfg = auto_config(24.0 * 0.8, 200, 200, ", fill_by: 0.6, tail: {below: 0}");
    let store = store(pool);
    fill(&store, &cfg, "catchall", 100).await;

    assert_eq!(offered_count(&cfg, &store, 100).await, 100);
}

#[sqlx::test]
async fn the_auto_share_is_computed_per_domain_group(pool: PgPool) {
    // The one behaviour that differs from D-091's list, which applies one value
    // to every group: the cap the controller paces against is per group, so the
    // share is too.
    let cfg = auto_config(
        24.0 * 0.2,
        200,
        200,
        ", ceiling: 0.8, floor: 0.01, tail: {below: 0}",
    );
    let store = store(pool);
    let day = day_index(&cfg);
    let route = cfg.default_ramp().route("warming").expect("route");
    let state = simmer::quota::store::RouteState::default();

    fill(&store, &cfg, "catchall", 150).await;

    let share_of = |group: &'static str| {
        let store = store.clone();
        async move {
            let usage = store
                .usage("main", "warming", group, day)
                .await
                .expect("usage");
            partial::share_for_group(
                route,
                day,
                state,
                Utc::now(),
                simmer::quota::allowance_for(route, group, day, state),
                Some(&usage),
            )
        }
    };

    let catchall = share_of("catchall").await.expect("throttled");
    let google = share_of("google").await;
    assert!(
        google.is_none() || google.expect("share") > catchall,
        "google's cap is untouched, so its share must be the looser one: \
         google {google:?} vs catchall {catchall}"
    );
}

#[sqlx::test]
async fn a_graduated_route_is_offered_everything_under_auto(pool: PgPool) {
    let cfg = auto_config(1.0, 200, 200, ", floor: 0.01, tail: {below: 0}");
    let store = store(pool);
    fill(&store, &cfg, "catchall", 190).await;
    store
        .set_graduated("main", "warming", true)
        .await
        .expect("graduate");

    // §9.3's graduate jumps to the end of the ramp, and the ramp is part of it.
    assert_eq!(offered_count(&cfg, &store, 60).await, 60);
}

#[sqlx::test]
async fn a_pinned_reply_is_exempt_under_auto(pool: PgPool) {
    // D-090: a reply that changed identity because arithmetic said so is exactly
    // what thread affinity exists to prevent.
    let cfg = auto_config(
        1.0,
        200,
        200,
        ", floor: 0.01, ceiling: 0.02, tail: {below: 0}",
    );
    let store = store(pool);
    fill(&store, &cfg, "catchall", 150).await;

    for r in recipients(30) {
        let (selected, _) = walk(&cfg, &store, Some("warming"), &r).await;
        assert_eq!(selected.as_deref(), Some("warming"), "{r}");
    }
}

#[sqlx::test]
async fn dry_run_gives_the_real_walks_answer_under_auto(pool: PgPool) {
    let cfg = auto_config(
        24.0 * 0.25,
        200,
        200,
        ", ceiling: 0.9, floor: 0.01, tail: {below: 0}",
    );
    let store = store(pool);
    fill(&store, &cfg, "catchall", 100).await;
    let chain = vec!["warming".to_string(), "overflow".to_string()];

    // §9 says the control plane must not lie. It reads the same row and the same
    // clock as the walk, so its answer is the walk's — not a probability.
    for r in recipients(40) {
        let dry = chain::dry_walk(
            cfg.default_ramp(),
            &simmer::routing::domain_group::Grouper::literal(),
            &cfg.dot_insensitive_domains,
            &store,
            &Frequency::new(),
            &simmer::preflight::Registry::new(),
            &chain,
            None,
            &r,
            Utc::now(),
        )
        .await
        .expect("dry walk");
        let (_, real) = walk(&cfg, &store, None, &r).await;
        assert_eq!(chain::render(&dry), chain::render(&real), "{r}");
    }
}
