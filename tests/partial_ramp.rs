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
///
/// Under `advance: auto`, so the day index is the step and the share list is
/// indexed by the calendar; [`threshold_config`] is D-115's variant.
fn config(day: i64, share: &[f64]) -> simmer::config::Config {
    config_with(day, share, "auto")
}

/// [`config`] under D-115's threshold rule at 90%.
fn threshold_config(day: i64, share: &[f64]) -> simmer::config::Config {
    config_with(day, share, "{ mode: threshold, threshold: 0.9 }")
}

fn config_with(day: i64, share: &[f64], advance: &str) -> simmer::config::Config {
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
  warmup_advance: {advance}
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
        let expected = partial::offered(&keyer, "main", "warming", &r, 0.4, &[]);

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
    // Day 1 of [0.01, 1.0] offers everything; day 0 of it almost nothing.
    // Reading the wrong index would invert the two. (D-117 refuses a list that
    // decreases, so the inversion is told apart by day rather than by list.)
    let store = store(pool);
    assert_eq!(on_warming(&config(1, &[0.01, 1.0]), &store, 40).await, 40);
    assert!(on_warming(&config(0, &[0.01, 1.0]), &store, 40).await <= 2);
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

    let gated = recipients(100)
        .into_iter()
        .find(|r| !partial::offered(&keyer, "main", "warming", r, 0.2, &[]))
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
            ..Default::default()
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
                day,
                state,
                Utc::now(),
                simmer::quota::allowance_for(route, group, day, day, state),
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

// -- D-117: a recipient ramped onto the warming route stays ramped ----------

/// The recipients of `pool` that the real walk puts on the warming route on
/// the config's day.
async fn ramped_set(
    cfg: &simmer::config::Config,
    store: &Arc<dyn QuotaStore>,
    pool: &[String],
) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for r in pool {
        if walk(cfg, store, None, r).await.0.as_deref() == Some("warming") {
            out.insert(r.clone());
        }
    }
    out
}

#[sqlx::test]
async fn every_recipient_ramped_on_a_prior_day_stays_ramped_for_the_rest_of_the_ramp(pool: PgPool) {
    // Walk the same recipients through every day of a rising share list and
    // past its end. Nobody ever leaves; each day only adds.
    let store = store(pool);
    let shares = [0.1, 0.2, 0.2, 0.45, 0.7, 0.9];
    let people = recipients(300);
    let mut before: Option<std::collections::BTreeSet<String>> = None;
    for day in 0..=(shares.len() as i64 + 1) {
        let today = ramped_set(&config(day, &shares), &store, &people).await;
        if let Some(prev) = &before {
            let dropped: Vec<_> = prev.difference(&today).collect();
            assert!(dropped.is_empty(), "day {day} dropped {dropped:?}");
        }
        before = Some(today);
    }
    assert_eq!(
        before.unwrap().len(),
        people.len(),
        "past the list everyone is on"
    );
}

#[sqlx::test]
async fn a_day_with_the_same_share_ramps_exactly_the_same_recipients(pool: PgPool) {
    // [0.2, 0.2]: days 0 and 1 offer the same entry, so the same people —
    // not a fresh 20% drawn for the new day, as before D-117.
    let store = store(pool);
    let people = recipients(300);
    let day0 = ramped_set(&config(0, &[0.2, 0.2]), &store, &people).await;
    let day1 = ramped_set(&config(1, &[0.2, 0.2]), &store, &people).await;
    assert!(!day0.is_empty());
    assert_eq!(day0, day1);
}

#[sqlx::test]
async fn the_ramped_set_is_the_one_the_hash_predicts_on_every_day(pool: PgPool) {
    let store = store(pool);
    let keyer = keyer(&store).await;
    let people = recipients(200);
    for (day, share) in [(0, 0.15), (1, 0.4), (2, 0.4), (3, 0.8)] {
        let cfg = config(day, &[0.15, 0.4, 0.4, 0.8]);
        let walked = ramped_set(&cfg, &store, &people).await;
        let predicted: std::collections::BTreeSet<String> = people
            .iter()
            .filter(|r| partial::offered(&keyer, "main", "warming", r, share, &[]))
            .cloned()
            .collect();
        assert_eq!(walked, predicted, "day {day}");
    }
}

#[sqlx::test]
async fn a_held_step_keeps_the_ramped_set_unchanged(pool: PgPool) {
    // D-115: day 0 sends nothing that commits, so day 1 is held at step 0 and
    // offers share[0] again — the same recipients, nobody new, nobody lost.
    let store = store(pool);
    let people = recipients(300);
    let shares = [0.2, 0.6];
    let day0 = ramped_set(&threshold_config(0, &shares), &store, &people).await;
    let day1 = ramped_set(&threshold_config(1, &shares), &store, &people).await;
    assert_eq!(day0, day1, "held at step 0, so the same 20%");

    // Under auto, the same history moves to share[1]: everybody from day 0 is
    // still there, and more besides.
    let auto1 = ramped_set(&config(1, &shares), &store, &people).await;
    assert!(day0.is_subset(&auto1));
    assert!(auto1.len() > day0.len());
}

#[sqlx::test]
async fn an_advanced_step_widens_the_set_without_dropping_anyone(pool: PgPool) {
    // Day 0's row met 90% of its plan, so day 1 is step 1 and share[1].
    use simmer::quota::store::ReserveRequest;
    let store = store(pool);
    let people = recipients(300);
    let shares = [0.2, 0.6];
    let day0 = ramped_set(&threshold_config(0, &shares), &store, &people).await;

    let filler = store
        .reserve(&ReserveRequest {
            ramp: "main".into(),
            route: "warming".into(),
            domain_group: "catchall".into(),
            day_index: 0,
            allowance: Some(100_000),
            step: Some(0),
            count: 90_000,
            correlation_id: "fill".into(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            ..Default::default()
        })
        .await
        .expect("reserve");
    let simmer::quota::store::Reserved::Taken(r) = filler else {
        panic!("headroom for the filler");
    };
    store.commit(&r, &[]).await.expect("commit");

    let day1 = ramped_set(&threshold_config(1, &shares), &store, &people).await;
    assert!(day0.is_subset(&day1), "nobody ramped on day 0 is dropped");
    assert!(day1.len() > day0.len(), "step 1's wider share admits more");
}

#[sqlx::test]
async fn a_recipient_keeps_its_place_under_any_spelling_of_its_address(pool: PgPool) {
    let store = store(pool);
    let cfg = config(0, &[0.3]);
    let people = recipients(200);
    let plain = ramped_set(&cfg, &store, &people).await;
    let shouted: Vec<String> = people.iter().map(|r| r.to_uppercase()).collect();
    let loud = ramped_set(&cfg, &store, &shouted).await;
    assert_eq!(
        plain.len(),
        loud.len(),
        "normalisation keeps a ramped inbox ramped however it is written"
    );
}

// -- D-118: an auto partial ramp keeps the recipients it mailed (O-21) -------

/// Walk, and on a selection **commit** the way §7.4 phase 3 does after a
/// downstream `2xx` — member key and all. Returns the route and whether
/// membership is what let the recipient onto it.
async fn send(
    cfg: &simmer::config::Config,
    store: &Arc<dyn QuotaStore>,
    pinned: Option<&str>,
    recipient: &str,
) -> (Option<String>, bool) {
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
        Utc::now(),
    )
    .await
    .expect("walk");
    match walked {
        Walk::Selected(s) => {
            store
                .commit_recording(&s.reservation, &s.recipient_keys, s.member_key.as_ref())
                .await
                .expect("commit");
            (Some(s.route.name.clone()), s.partial_member)
        }
        Walk::Exhausted => (None, false),
    }
}

/// Fill a day's cap so `remaining` is left, counting what is already used.
async fn leave(
    store: &Arc<dyn QuotaStore>,
    cfg: &simmer::config::Config,
    cap: i64,
    remaining: i64,
) {
    let u = store
        .usage("main", "warming", "catchall", day_index(cfg))
        .await
        .expect("usage");
    let to_fill = cap - remaining - u.committed - u.reserved;
    assert!(to_fill > 0, "the fixture must leave {remaining}: {u:?}");
    fill(store, cfg, "catchall", to_fill).await;
}

/// The auto params every D-118 test uses: a high ceiling while the cap is
/// untouched, a floor of 1% once it is nearly full, and no tail release.
const STICKY: &str = ", ceiling: 0.9, floor: 0.01, tail: {below: 0}";

/// Pass one, at a high share: who the route took. Every one of them is now a
/// member.
async fn mail_at_a_high_share(
    cfg: &simmer::config::Config,
    store: &Arc<dyn QuotaStore>,
    people: &[String],
) -> std::collections::BTreeSet<String> {
    let mut on = std::collections::BTreeSet::new();
    for r in people {
        let (route, member) = send(cfg, store, None, r).await;
        assert!(!member, "nobody is a member before their first commit: {r}");
        if route.as_deref() == Some("warming") {
            on.insert(r.clone());
        }
    }
    assert!(
        on.len() > people.len() / 2,
        "the fixture's high share must take most of them: {} of {}",
        on.len(),
        people.len()
    );
    on
}

#[sqlx::test]
async fn a_recipient_mailed_at_a_high_share_stays_when_the_share_falls_the_same_day(pool: PgPool) {
    // O-21's 09:00/14:00 flip: the share is 0.9 on an empty cap and 0.01 on a
    // nearly full one, within one ramp day. A recipient mailed in the morning
    // must not be steered off in the afternoon.
    let cfg = auto_config(24.0 * 0.2, 1000, 1000, STICKY);
    let store = store(pool);
    let people = recipients(100);
    let members = mail_at_a_high_share(&cfg, &store, &people).await;

    leave(&store, &cfg, 1000, 150).await;
    // The share really did fall: a stranger is now almost always turned away.
    let strangers: Vec<String> = (0..100)
        .map(|i| format!("stranger{i}@example.com"))
        .collect();
    assert!(
        offered_count_of(&cfg, &store, &strangers).await < 10,
        "the fixture's share must have fallen to about its floor"
    );

    let mut kept_by_membership = 0;
    for r in &members {
        let (route, member) = send(&cfg, &store, None, r).await;
        assert_eq!(route.as_deref(), Some("warming"), "{r} was dropped");
        if member {
            kept_by_membership += 1;
        }
    }
    assert!(
        kept_by_membership > members.len() / 2,
        "most of them must have been kept by membership, not by the share: \
         {kept_by_membership} of {}",
        members.len()
    );
}

#[sqlx::test]
async fn a_recipient_mailed_on_one_day_stays_on_the_next(pool: PgPool) {
    let store = store(pool);
    let people = recipients(100);
    let day0 = auto_config(24.0 * 0.2, 1000, 1000, STICKY);
    let members = mail_at_a_high_share(&day0, &store, &people).await;

    // Day 1, already nearly full: the share is at its floor.
    let day1 = auto_config(24.0 * 1.2, 1000, 1000, STICKY);
    assert_eq!(day_index(&day1), day_index(&day0) + 1);
    leave(&store, &day1, 1000, 150).await;

    for r in &members {
        let (route, _) = send(&day1, &store, None, r).await;
        assert_eq!(
            route.as_deref(),
            Some("warming"),
            "{r} was dropped on day 1"
        );
    }
    // And the people the route never took are still judged on the share alone.
    let never: Vec<String> = people
        .iter()
        .filter(|r| !members.contains(*r))
        .cloned()
        .collect();
    let back_on = offered_count_of(&day1, &store, &never).await;
    assert!(back_on <= never.len() / 4, "{back_on} of {}", never.len());
}

#[sqlx::test]
async fn membership_does_not_lift_the_cap(pool: PgPool) {
    // The author's "yes" to D-117: a full cap still sends a ramped recipient to
    // overflow. Membership answers the share, never the quota.
    let cfg = auto_config(24.0 * 0.2, 1000, 1000, STICKY);
    let store = store(pool);
    let members = mail_at_a_high_share(&cfg, &store, &recipients(50)).await;
    let u = store
        .usage("main", "warming", "catchall", day_index(&cfg))
        .await
        .unwrap();
    fill(&store, &cfg, "catchall", 1000 - u.committed - u.reserved).await;

    for r in &members {
        let (_, eval) = walk(&cfg, &store, None, r).await;
        let warming = eval.iter().find(|s| s.route == "warming").expect("step");
        assert_eq!(warming.outcome, Err(SkipReason::Quota), "{r}");
    }
}

#[sqlx::test]
async fn the_dry_run_agrees_with_the_walk_for_members(pool: PgPool) {
    let cfg = auto_config(24.0 * 0.2, 1000, 1000, STICKY);
    let store = store(pool);
    let people = recipients(60);
    mail_at_a_high_share(&cfg, &store, &people).await;
    leave(&store, &cfg, 1000, 200).await;
    let chain = vec!["warming".to_string(), "overflow".to_string()];

    let mut strangers: Vec<String> = (0..20)
        .map(|i| format!("stranger{i}@example.com"))
        .collect();
    strangers.extend(people);
    for r in strangers {
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

#[sqlx::test]
async fn a_pinned_reply_makes_its_recipient_a_member(pool: PgPool) {
    // D-090 put them on the route against the share; they were mailed on it,
    // so it keeps them.
    let cfg = auto_config(24.0 * 0.2, 1000, 1000, STICKY);
    let store = store(pool);
    leave(&store, &cfg, 1000, 200).await;

    let r = "pinned@example.com";
    let (route, _) = send(&cfg, &store, Some("warming"), r).await;
    assert_eq!(route.as_deref(), Some("warming"));
    let (route, member) = send(&cfg, &store, None, r).await;
    assert_eq!(route.as_deref(), Some("warming"));
    // Unless their draw happens to fall under the 1% floor, it was membership.
    let keyer = keyer(&store).await;
    let draw = partial::draw(&keyer, "main", "warming", r, &[]);
    assert_eq!(member, draw >= 0.01, "draw {draw}");
}

#[sqlx::test]
async fn any_spelling_of_a_member_is_the_member(pool: PgPool) {
    let cfg = auto_config(24.0 * 0.2, 1000, 1000, STICKY);
    let store = store(pool);
    leave(&store, &cfg, 1000, 200).await;
    let (route, _) = send(&cfg, &store, Some("warming"), "Reader@Example.COM").await;
    assert_eq!(route.as_deref(), Some("warming"));
    let (route, _) = send(&cfg, &store, None, "reader@example.com").await;
    assert_eq!(route.as_deref(), Some("warming"));
}

#[sqlx::test]
async fn a_list_share_records_no_members(pool: PgPool) {
    // D-117's nested draw already keeps everyone a list ramped; a row per
    // recipient there would be a record of every address mailed for nothing.
    let cfg = config(0, &[0.4]);
    let store = store(pool);
    let mut on = 0;
    for r in recipients(50) {
        if send(&cfg, &store, None, &r).await.0.as_deref() == Some("warming") {
            on += 1;
        }
    }
    assert!(on > 0);
    assert!(store.partial_member_routes().await.unwrap().is_empty());
}

#[sqlx::test]
async fn an_overflow_commit_makes_no_member(pool: PgPool) {
    let cfg = auto_config(24.0 * 0.2, 1000, 1000, STICKY);
    let store = store(pool);
    leave(&store, &cfg, 1000, 200).await;
    let keyer = keyer(&store).await;
    // Find someone the share turns away, and send them.
    let stranger = (0..1000)
        .map(|i| format!("stranger{i}@example.com"))
        .find(|r| partial::draw(&keyer, "main", "warming", r, &[]) >= 0.5)
        .expect("someone draws high");
    let (route, _) = send(&cfg, &store, None, &stranger).await;
    assert_eq!(route.as_deref(), Some("overflow"));
    assert!(store.partial_member_routes().await.unwrap().is_empty());
}

#[sqlx::test]
async fn the_sweeper_drops_an_idle_routes_members_and_keeps_an_active_ones(pool: PgPool) {
    use simmer::routing::partial_members::{retention_days, sweep_once};
    let cfg = auto_config(24.0 * 0.2, 1000, 1000, STICKY);
    let store = store(pool);
    mail_at_a_high_share(&cfg, &store, &recipients(10)).await;
    assert_eq!(retention_days(&cfg, "main", "warming"), 30);
    let members = store.partial_member_routes().await.unwrap()[0].members;

    // 29 days idle: kept, every one of them.
    let now = Utc::now();
    assert_eq!(
        sweep_once(store.as_ref(), &cfg, now + chrono::Duration::days(29))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store.partial_member_routes().await.unwrap()[0].members,
        members
    );

    // 31: the route has finished; its members go together.
    assert_eq!(
        sweep_once(store.as_ref(), &cfg, now + chrono::Duration::days(31))
            .await
            .unwrap(),
        members as u64
    );
    assert!(store.partial_member_routes().await.unwrap().is_empty());
}

#[sqlx::test]
async fn member_retention_days_is_the_routes_own(pool: PgPool) {
    use simmer::routing::partial_members::{retention_days, sweep_once};
    let cfg = auto_config(
        24.0 * 0.2,
        1000,
        1000,
        &format!("{STICKY}, member_retention_days: 2"),
    );
    assert_eq!(retention_days(&cfg, "main", "warming"), 2);
    // A route nobody configures, or one no longer auto, gets the default.
    assert_eq!(retention_days(&cfg, "main", "gone"), 30);
    assert_eq!(retention_days(&cfg, "main", "overflow"), 30);
    assert_eq!(retention_days(&cfg, "elsewhere", "warming"), 30);

    let store = store(pool);
    mail_at_a_high_share(&cfg, &store, &recipients(10)).await;
    let now = Utc::now();
    assert_eq!(
        sweep_once(store.as_ref(), &cfg, now + chrono::Duration::days(1))
            .await
            .unwrap(),
        0
    );
    assert!(
        sweep_once(store.as_ref(), &cfg, now + chrono::Duration::days(3))
            .await
            .unwrap()
            > 0
    );
}

/// [`offered_count`], over named recipients.
async fn offered_count_of(
    cfg: &simmer::config::Config,
    store: &Arc<dyn QuotaStore>,
    people: &[String],
) -> usize {
    let chain = vec!["warming".to_string(), "overflow".to_string()];
    let mut offered = 0;
    for r in people {
        let dry = chain::dry_walk(
            cfg.default_ramp(),
            &simmer::routing::domain_group::Grouper::literal(),
            &cfg.dot_insensitive_domains,
            store,
            &Frequency::new(),
            &simmer::preflight::Registry::new(),
            &chain,
            None,
            r,
            Utc::now(),
        )
        .await
        .expect("dry walk");
        if !dry
            .iter()
            .any(|s| s.route == "warming" && s.outcome == Err(SkipReason::PartialRamp))
        {
            offered += 1;
        }
    }
    offered
}
