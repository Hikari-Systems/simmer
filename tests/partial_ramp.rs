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
    simmer::quota::day::for_route(cfg.route("warming").expect("route"), Utc::now())
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
        cfg,
        store,
        &Frequency::new(),
        &simmer::preflight::Registry::new(),
        &chain,
        pinned,
        &[recipient.to_string()],
        "test-correlation",
        &mut evaluation,
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
        .usage("warming", "catchall", day_index(cfg))
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
        let expected = partial::offered(&keyer, "warming", &r, day, 0.4, &[]);

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
        .set_graduated("warming", true)
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
        .find(|r| !partial::offered(&keyer, "warming", r, day, 0.2, &[]))
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
            &cfg,
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
