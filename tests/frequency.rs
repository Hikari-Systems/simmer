//! §7.3 against real Postgres, and through the whole relay path.
//!
//! `src/frequency/mod.rs`'s unit tests own the normalisation, the hashing and the
//! window arithmetic. What only a database can show is here: that the window is
//! *rolling* against stored timestamps, that events are recorded on a downstream
//! `2xx` and on nothing else, that the salt survives a second store, and that
//! being over threshold **steers** rather than drops (§7.3: "it is a steering
//! rule, not a suppression rule. Nothing is ever dropped by it").

// Postgres-backed: the storage layer under test is `PgQuotaStore`. The SQL
// Server build runs the backend-neutral suite instead (tests/store_mssql.rs,
// D-084).
#![cfg(feature = "postgres")]

mod support;

use std::sync::Arc;

use chrono::{Duration, Utc};
use simmer::config::FrequencyMode;
use simmer::frequency::{Frequency, Keyer};
use simmer::quota::store::QuotaStore;
use simmer::quota::PgQuotaStore;
use simmer::routing::chain::{self, SkipReason, Walk};
use sqlx::PgPool;
use support::{Act, FakeDownstream, Script, Simmer};

const BODY: &str = "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n";

/// A warming route with a `threshold: 2` daily window in front of an uncapped
/// overflow, each with its own downstream so a test can tell which one carried
/// the message. The ramp is set high enough that quota never interferes: this
/// suite is about §7.3, and a route skipped for the wrong reason would still
/// look like a pass.
fn config(warming_port: u16, overflow_port: u16, mode: &str) -> String {
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
  timeouts: {{ command: 5s, data: 5s, session: 60s }}
  auth: {{ allow_insecure_auth: true }}
database:
  url: "postgres://u:p@localhost/simmer"
  connect_timeout: 5s
  fail_closed: true
admin: {{ listen: "127.0.0.1:0", auth_token: "t" }}
logging: {{ level: warn, format: text }}
dot_insensitive_domains: ["gmail.com"]
domain_groups:
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
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
      timeouts: {{ connect: 2s, command: 2s, data: 2s }}
    identity: {{ envelope_from: "b@newbrand.com" }}
    warmup:
      started: "2020-01-01T00:00:00Z"
      schedule: {{ default: [100] }}
    recipient_frequency:
      mode: {mode}
      window: {{ unit: daily, count: 1 }}
      threshold: 2
  - name: overflow
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: {overflow_port}
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
      timeouts: {{ connect: 2s, command: 2s, data: 2s }}
    identity: {{ envelope_from: "b@established.com" }}
"#
    )
}

/// The same fixture with no overflow to fall through to, so chain exhaustion is
/// reachable. `strict_senders` rather than a `default_chain`, because §4.2 will
/// not accept a default chain that does not end in an overflow route — for the
/// same reason this test exists.
fn config_without_overflow(warming_port: u16) -> String {
    config(warming_port, warming_port, "to_address")
        .replace("chain: [warming, overflow] }", "chain: [warming] }")
        .replace("default_chain: [overflow]", "strict_senders: true")
}

fn store(pool: PgPool) -> Arc<dyn QuotaStore> {
    Arc::new(PgQuotaStore::new(pool))
}

/// The keyer the *instance* is using — read from `instance_config` rather than
/// pinned to a constant.
///
/// This matters: the relay resolves its salt from the same table, so a test that
/// invented its own salt would compute keys nothing in the database matches, and
/// every assertion about a count would quietly read zero. Sharing the instance's
/// salt is also what makes these tests exercise the real §7.3 path.
async fn keyer(pool: &PgPool) -> Keyer {
    Keyer::new(
        store(pool.clone())
            .recipient_hash_salt()
            .await
            .expect("salt"),
    )
}

fn dot_insensitive() -> Vec<String> {
    vec!["gmail.com".to_string()]
}

/// Write an event at a chosen instant, which is the only way to test a window
/// boundary without waiting a day for one.
async fn seed(pool: &PgPool, route: &str, address: &str, when: chrono::DateTime<Utc>) {
    let key = keyer(pool)
        .await
        .key_for(address, FrequencyMode::ToAddress, &dot_insensitive());
    let mut tx = pool.begin().await.expect("begin");
    simmer::models::recipient_event::record(&mut tx, route, &[key], when)
        .await
        .expect("record");
    tx.commit().await.expect("commit");
}

async fn count(pool: &PgPool, route: &str, address: &str, since_hours: i64) -> i64 {
    let key = keyer(pool)
        .await
        .key_for(address, FrequencyMode::ToAddress, &dot_insensitive());
    store(pool.clone())
        .recipient_event_count(route, &key, Utc::now() - Duration::hours(since_hours))
        .await
        .expect("count")
}

// ---------------------------------------------------------------------------
// The salt (§7.3: "generated once and persisted")
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn the_salt_survives_a_second_store(pool: PgPool) {
    // The property that matters on restart. A salt minted fresh each time would
    // silently reset every window rather than failing visibly, because every key
    // would simply stop matching the rows already there.
    let first = store(pool.clone())
        .recipient_hash_salt()
        .await
        .expect("salt");
    let second = store(pool.clone())
        .recipient_hash_salt()
        .await
        .expect("salt");

    assert_eq!(first, second);
    assert_eq!(first.len(), 32);
}

#[sqlx::test]
async fn two_stores_racing_for_the_salt_agree_on_one(pool: PgPool) {
    // Two replicas starting together. `ON CONFLICT DO NOTHING` plus a read means
    // whoever inserts first wins and both use that one — the alternative is two
    // instances with different views of the same recipient.
    let a = store(pool.clone());
    let b = store(pool.clone());
    let (x, y) = tokio::join!(a.recipient_hash_salt(), b.recipient_hash_salt());
    assert_eq!(x.expect("a"), y.expect("b"));
}

#[sqlx::test]
async fn the_stored_salt_is_not_the_plaintext_of_anything(pool: PgPool) {
    // §7.3's data-protection claim starts here: the only thing in
    // `instance_config` is the key material.
    store(pool.clone())
        .recipient_hash_salt()
        .await
        .expect("salt");
    let stored = simmer::models::instance_config::get(
        &pool,
        simmer::models::instance_config::RECIPIENT_HASH_SALT,
    )
    .await
    .expect("get")
    .expect("present");

    assert!(!stored.is_empty());
    assert!(!stored.contains('@'), "{stored}");
}

// ---------------------------------------------------------------------------
// The rolling window, against stored timestamps
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn an_event_inside_the_window_counts_and_one_outside_does_not(pool: PgPool) {
    seed(
        &pool,
        "warming",
        "bob@example.com",
        Utc::now() - Duration::hours(1),
    )
    .await;
    seed(
        &pool,
        "warming",
        "bob@example.com",
        Utc::now() - Duration::hours(30),
    )
    .await;

    // A 24-hour window sees only the recent one...
    assert_eq!(count(&pool, "warming", "bob@example.com", 24).await, 1);
    // ...and a 48-hour window sees both, which is what makes it *rolling* rather
    // than a bucket that empties.
    assert_eq!(count(&pool, "warming", "bob@example.com", 48).await, 2);
}

#[sqlx::test]
async fn the_window_is_counted_per_route(pool: PgPool) {
    // §7.3's constraint is declared on a route and counts that route's own sends.
    // §11's row carries `route` for exactly this reason.
    seed(&pool, "warming", "bob@example.com", Utc::now()).await;
    seed(&pool, "overflow", "bob@example.com", Utc::now()).await;
    seed(&pool, "overflow", "bob@example.com", Utc::now()).await;

    assert_eq!(count(&pool, "warming", "bob@example.com", 24).await, 1);
    assert_eq!(count(&pool, "overflow", "bob@example.com", 24).await, 2);
}

#[sqlx::test]
async fn normalised_spellings_of_one_inbox_share_a_window(pool: PgPool) {
    // §7.3's headline case, end to end through the storage layer: the two
    // spellings are one inbox, so they must be one row's worth of key.
    seed(&pool, "warming", "Bob.Smith+news@gmail.com", Utc::now()).await;

    assert_eq!(count(&pool, "warming", "bobsmith@gmail.com", 24).await, 1);
    // ...and a genuinely different address does not.
    assert_eq!(count(&pool, "warming", "alice@gmail.com", 24).await, 0);
}

// ---------------------------------------------------------------------------
// The sweeper (§7.3: "evicts rows older than the longest configured window plus
// a margin, on an interval")
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn the_sweeper_evicts_only_what_is_past_the_cutoff(pool: PgPool) {
    let store = store(pool.clone());
    seed(
        &pool,
        "warming",
        "old@example.com",
        Utc::now() - Duration::hours(30),
    )
    .await;
    seed(
        &pool,
        "warming",
        "new@example.com",
        Utc::now() - Duration::hours(1),
    )
    .await;

    let evicted = store
        .sweep_recipient_events(Utc::now() - Duration::hours(26))
        .await
        .expect("sweep");

    assert_eq!(evicted, 1);
    assert_eq!(count(&pool, "warming", "old@example.com", 48).await, 0);
    assert_eq!(
        count(&pool, "warming", "new@example.com", 48).await,
        1,
        "a row still inside a window must survive the sweep"
    );
}

#[sqlx::test]
async fn a_sweep_with_nothing_to_do_is_not_an_error(pool: PgPool) {
    let store = store(pool.clone());
    let evicted = store
        .sweep_recipient_events(Utc::now() - Duration::hours(26))
        .await
        .expect("sweep");
    assert_eq!(evicted, 0);
}

#[sqlx::test]
async fn the_sweeper_task_runs_a_pass_and_stops_on_shutdown(pool: PgPool) {
    // `sweep_once` is what the interval loop calls; this drives the loop itself,
    // which is the part `quota::sweeper` still has no coverage for.
    let store = store(pool.clone());
    seed(
        &pool,
        "warming",
        "old@example.com",
        Utc::now() - Duration::hours(30),
    )
    .await;

    let shutdown = simmer::smtp::Shutdown::new();
    let task = tokio::spawn(simmer::frequency::sweeper::run(
        Arc::clone(&store),
        std::time::Duration::from_secs(26 * 3_600),
        shutdown.clone(),
    ));

    // The first tick fires immediately, so the pass has happened by the time the
    // row is gone. Poll rather than sleep a fixed interval.
    for _ in 0..100 {
        if count(&pool, "warming", "old@example.com", 48).await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(count(&pool, "warming", "old@example.com", 48).await, 0);

    shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the sweeper stops when the shutdown token fires")
        .expect("no panic");
}

// ---------------------------------------------------------------------------
// §3.2 step 3b — the chain walk
// ---------------------------------------------------------------------------

async fn walk(
    cfg: &simmer::config::Config,
    store: &Arc<dyn QuotaStore>,
    recipient: &str,
) -> (Option<String>, Vec<chain::Step>) {
    let mut evaluation = Vec::new();
    let chain = vec!["warming".to_string(), "overflow".to_string()];
    let walked = chain::walk_and_reserve(
        cfg,
        &simmer::routing::domain_group::Grouper::literal(),
        store,
        &Frequency::new(),
        &simmer::preflight::Registry::new(),
        &chain,
        None,
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

fn parse(yaml: &str) -> simmer::config::Config {
    simmer::config::from_str(yaml, "test").expect("fixture is valid")
}

#[sqlx::test]
async fn under_threshold_the_constrained_route_is_selected(pool: PgPool) {
    let store = store(pool.clone());
    let cfg = parse(&config(2525, 2526, "to_address"));

    seed(&pool, "warming", "bob@example.com", Utc::now()).await;

    let (selected, _) = walk(&cfg, &store, "bob@example.com").await;
    assert_eq!(selected.as_deref(), Some("warming"), "1 < threshold 2");
}

#[sqlx::test]
async fn at_threshold_the_route_is_skipped_and_the_chain_falls_through(pool: PgPool) {
    // §7.3: "Over threshold makes the route **ineligible**, so the message falls
    // through to the next link." At or over — the spec says "at or over" in §3.2
    // 3b — so two events against a threshold of two is enough.
    let store = store(pool.clone());
    let cfg = parse(&config(2525, 2526, "to_address"));

    seed(&pool, "warming", "bob@example.com", Utc::now()).await;
    seed(&pool, "warming", "bob@example.com", Utc::now()).await;

    let (selected, evaluation) = walk(&cfg, &store, "bob@example.com").await;
    assert_eq!(selected.as_deref(), Some("overflow"));
    assert_eq!(
        evaluation[0].outcome,
        Err(SkipReason::Frequency),
        "the skip reason must be `frequency`, not `quota`: they call for opposite responses"
    );
    assert_eq!(
        chain::render(&evaluation),
        "warming=frequency,overflow=selected"
    );
}

#[sqlx::test]
async fn an_event_outside_the_window_does_not_keep_a_route_ineligible(pool: PgPool) {
    // The rolling window's whole point: yesterday's messages stop counting.
    let store = store(pool.clone());
    let cfg = parse(&config(2525, 2526, "to_address"));

    seed(
        &pool,
        "warming",
        "bob@example.com",
        Utc::now() - Duration::hours(25),
    )
    .await;
    seed(
        &pool,
        "warming",
        "bob@example.com",
        Utc::now() - Duration::hours(26),
    )
    .await;

    let (selected, _) = walk(&cfg, &store, "bob@example.com").await;
    assert_eq!(selected.as_deref(), Some("warming"));
}

#[sqlx::test]
async fn another_recipient_is_unaffected_by_this_ones_window(pool: PgPool) {
    let store = store(pool.clone());
    let cfg = parse(&config(2525, 2526, "to_address"));

    seed(&pool, "warming", "bob@example.com", Utc::now()).await;
    seed(&pool, "warming", "bob@example.com", Utc::now()).await;

    let (selected, _) = walk(&cfg, &store, "alice@example.com").await;
    assert_eq!(selected.as_deref(), Some("warming"));
}

#[sqlx::test]
async fn to_domain_mode_pools_every_recipient_at_one_provider(pool: PgPool) {
    // The other half of §7.3's two modes: `to_domain` keys on the domain, so two
    // different people at one provider share a window.
    let store = store(pool.clone());
    let cfg = parse(&config(2525, 2526, "to_domain"));

    let key = keyer(&pool)
        .await
        .key_for("anyone@example.com", FrequencyMode::ToDomain, &[]);
    let mut tx = pool.begin().await.expect("begin");
    simmer::models::recipient_event::record(&mut tx, "warming", &[key.clone(), key], Utc::now())
        .await
        .expect("record");
    tx.commit().await.expect("commit");

    let (selected, _) = walk(&cfg, &store, "someone-else@example.com").await;
    assert_eq!(
        selected.as_deref(),
        Some("overflow"),
        "two events for the domain put every recipient at it over threshold"
    );
}

#[sqlx::test]
async fn an_unconstrained_route_is_never_skipped_for_frequency(pool: PgPool) {
    // The overflow route declares no constraint, so no number of events can make
    // it ineligible — which is what keeps the chain from being exhausted by a
    // steering rule.
    let store = store(pool.clone());
    let cfg = parse(&config(2525, 2526, "to_address"));

    for _ in 0..10 {
        seed(&pool, "warming", "bob@example.com", Utc::now()).await;
        seed(&pool, "overflow", "bob@example.com", Utc::now()).await;
    }

    let (selected, _) = walk(&cfg, &store, "bob@example.com").await;
    assert_eq!(selected.as_deref(), Some("overflow"));
}

// ---------------------------------------------------------------------------
// Through the whole relay: §7.4 phase 3 records events, and §10.3 answers 451
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn a_delivered_message_records_one_event(pool: PgPool) {
    // §7.4 phase 3: "On downstream 2xx, move the count from reserved to committed
    // **and record recipient-frequency events**."
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let store = store(pool.clone());

    let simmer = Simmer::start_with_quota(
        &config(warm.addr.port(), over.addr.port(), "to_address"),
        Arc::clone(&store),
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 250, "{r:?}");

    assert_eq!(count(&pool, "warming", "bob@gmail.com", 24).await, 1);
}

#[sqlx::test]
async fn a_failed_send_records_no_event(pool: PgPool) {
    // The other half of the same sentence. A message the recipient never received
    // must not count against a window that exists to ask "how often has this
    // person heard from us".
    let warm = FakeDownstream::start(Script::with(|s| {
        s.final_dot = Act::Reply(451, "4.3.0 try later");
    }))
    .await;
    let over = FakeDownstream::start(Script::default()).await;
    let store = store(pool.clone());

    let simmer = Simmer::start_with_quota(
        &config(warm.addr.port(), over.addr.port(), "to_address"),
        Arc::clone(&store),
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 451, "{r:?}");

    assert_eq!(
        count(&pool, "warming", "bob@gmail.com", 24).await,
        0,
        "§3.3 releases the reservation and records nothing; the send did not happen"
    );
}

#[sqlx::test]
async fn the_third_message_to_one_recipient_leaves_by_the_overflow_route(pool: PgPool) {
    // The whole feature, end to end: two messages ride the warming route, and the
    // third — still perfectly deliverable — goes out under the *other* identity
    // rather than being refused.
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let store = store(pool.clone());

    let simmer = Simmer::start_with_quota(
        &config(warm.addr.port(), over.addr.port(), "to_address"),
        Arc::clone(&store),
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    for _ in 0..3 {
        let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
        assert_eq!(r.code, 250, "{r:?}");
    }

    assert_eq!(warm.messages().len(), 2, "the threshold is 2");
    assert_eq!(
        over.messages().len(),
        1,
        "and the third steers, it does not drop"
    );
    assert_eq!(
        over.last().expect("received").mail_from.as_deref(),
        Some("b@established.com"),
        "the overflow route's identity, because that is the route that carried it"
    );

    // ...and the third message's event is recorded against the route that
    // actually sent it, which declares no constraint, so it is not recorded at
    // all: nothing will ever read it.
    assert_eq!(count(&pool, "warming", "bob@gmail.com", 24).await, 2);
    assert_eq!(count(&pool, "overflow", "bob@gmail.com", 24).await, 0);
}

#[sqlx::test]
async fn one_inbox_spelled_two_ways_shares_the_threshold_through_the_relay(pool: PgPool) {
    // §7.3's reason for normalising, as a user would hit it: "a determined
    // recipient will complain about both".
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let store = store(pool.clone());

    let simmer = Simmer::start_with_quota(
        &config(warm.addr.port(), over.addr.port(), "to_address"),
        Arc::clone(&store),
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    for to in [
        "Bob.Smith@gmail.com",
        "bobsmith+news@gmail.com",
        "bob.smith+other@gmail.com",
    ] {
        assert_eq!(c.deliver("jane@oldbrand.com", to, BODY).await.code, 250);
    }

    assert_eq!(warm.messages().len(), 2);
    assert_eq!(
        over.messages().len(),
        1,
        "the third spelling is the same inbox and must count against the same window"
    );
}

#[sqlx::test]
async fn an_exhausted_chain_is_451_and_never_550(pool: PgPool) {
    // §10.3 and §14.1. Over threshold with nowhere to fall through to is a
    // *temporary* refusal: the recipient is perfectly deliverable, and a 550 here
    // would suppress them in systems that outlive Simmer by years.
    let warm = FakeDownstream::start(Script::default()).await;
    let store = store(pool.clone());

    let simmer = Simmer::start_with_quota(
        &config_without_overflow(warm.addr.port()),
        Arc::clone(&store),
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    for _ in 0..2 {
        assert_eq!(
            c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY)
                .await
                .code,
            250
        );
    }

    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 451, "{r:?}");
    assert!(r.contains("4.7.1"), "{r:?}");
    assert_eq!(
        warm.messages().len(),
        2,
        "nothing more reached the downstream"
    );
}

#[sqlx::test]
async fn no_plaintext_address_reaches_the_event_table(pool: PgPool) {
    // §7.3's actual requirement, asserted against the database rather than
    // against the function that writes it: "avoids the container accumulating a
    // plaintext record of every address mailed".
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let store = store(pool.clone());

    let simmer = Simmer::start_with_quota(
        &config(warm.addr.port(), over.addr.port(), "to_address"),
        Arc::clone(&store),
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(
        c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY)
            .await
            .code,
        250
    );

    let rows: Vec<Vec<u8>> = sqlx::query_scalar("SELECT recipient_hash FROM recipient_event")
        .fetch_all(&pool)
        .await
        .expect("select");

    assert_eq!(rows.len(), 1);
    for row in rows {
        assert_eq!(row.len(), 16, "§7.3 bounds row size");
        for needle in ["bob", "gmail", "bob@gmail.com"] {
            assert!(
                !row.windows(needle.len()).any(|w| w == needle.as_bytes()),
                "'{needle}' is in the stored key"
            );
        }
    }
}
