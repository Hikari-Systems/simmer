//! Two Simmers against one database: what is safe, and what is not.
//!
//! This suite exists to settle a claim the repository used to make. `README.md`
//! and D-007 both said that running two instances against one database was
//! "precisely the window in which quota overshoot occurs", and that was the
//! stated reason simmer is kept off the spot fleet. Reading the code says
//! otherwise, and a claim of that weight should be evidenced rather than
//! asserted — hence D-061, and hence this file.
//!
//! ## Why not `tests/quota.rs`
//!
//! That suite's concurrency test races N tasks through **one** `PgQuotaStore` on
//! **one** pool. It proves the protocol is sound against concurrent tasks, which
//! is what §7.4 asks for, and a sceptic is entitled to say it proves nothing
//! about two processes: one pool could in principle be serialising the
//! contenders itself.
//!
//! Every test here therefore builds **two independent pools** against the same
//! database and wraps each in its own store. Two pools is as close to two
//! processes as an in-process test reaches — distinct connections, distinct pool
//! state, distinct `Frequency` salt caches, one Postgres. What it does not
//! reproduce is two *hosts*, and nothing in the mechanism under test is
//! sensitive to that: the serialisation is done by the server, on a row.
//!
//! ## What each half shows
//!
//! - **Quota is cross-instance safe**, by the row lock `INSERT … ON CONFLICT DO
//!   UPDATE` takes in `models::quota::lock_usage`. Two tests: one for the
//!   outcome, one for the mechanism.
//! - **§7.3's recipient-frequency window is not**, by D-049's deliberate choice
//!   to read outside the reservation transaction. Two tests, which state the
//!   bound and then show it is a bound on *concurrency* rather than a constant.
//!
//! These use `#[sqlx::test]`'s pool-options form rather than its ready-made
//! `PgPool`, which is the only way to get two pools onto one per-test database.

// Postgres-backed: the storage layer under test is `PgQuotaStore`. The SQL
// Server build runs the backend-neutral suite instead (tests/store_mssql.rs,
// D-084).
#![cfg(feature = "postgres")]

use std::sync::Arc;

use chrono::{Duration, Utc};
use simmer::config::{Config, FrequencyMode};
use simmer::frequency::{Frequency, Keyer};
use simmer::quota::store::{QuotaStore, ReserveRequest, Reserved};
use simmer::quota::PgQuotaStore;
use simmer::routing::chain::{self, Walk};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use tokio::sync::Barrier;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Two instances
// ---------------------------------------------------------------------------

/// One instance: its own pool, its own store, its own lazily-resolved §7.3 salt.
///
/// The `Frequency` is per-instance on purpose. A real second replica resolves the
/// salt from `instance_config` into its own `OnceCell` (D-050), and sharing one
/// here would quietly skip the part of §7.3 that has to agree across instances.
struct Instance {
    store: Arc<dyn QuotaStore>,
    frequency: Frequency,
    pool: PgPool,
}

impl Instance {
    async fn start(opts: &PgPoolOptions, conn: &PgConnectOptions) -> Self {
        let pool = opts
            .clone()
            .connect_with(conn.clone())
            .await
            .expect("a second pool against the same database");
        Self {
            store: Arc::new(PgQuotaStore::new(pool.clone())),
            frequency: Frequency::new(),
            pool,
        }
    }
}

/// Two instances, and a pool for the test itself to look at the result with.
///
/// `max_connections` is set high enough that a pool cannot become the thing that
/// serialises the contenders — which is the failure mode that would make this
/// whole file prove nothing.
async fn two_instances(
    opts: PgPoolOptions,
    conn: PgConnectOptions,
) -> (Arc<Instance>, Arc<Instance>) {
    let opts = opts.max_connections(16);
    (
        Arc::new(Instance::start(&opts, &conn).await),
        Arc::new(Instance::start(&opts, &conn).await),
    )
}

fn request(route: &str, allowance: Option<i64>, count: i64) -> ReserveRequest {
    ReserveRequest {
        route: route.into(),
        domain_group: "catchall".into(),
        day_index: 0,
        allowance,
        count,
        correlation_id: Uuid::new_v4().to_string(),
        expires_at: Utc::now() + Duration::minutes(10),
        over_cap: false,
    }
}

// ---------------------------------------------------------------------------
// Quota — safe across instances, by the row lock
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn n_reservations_split_across_two_pools_never_overshoot(
    opts: PgPoolOptions,
    conn: PgConnectOptions,
) {
    // The claim, tested: N concurrent reservations against a route with N-1
    // headroom grant exactly N-1, *whichever instance* they arrive at.
    //
    // `tests/quota.rs` runs this against one store. The only difference here is
    // that half the contenders reach Postgres over a different pool, and the
    // point is that it makes no difference at all.
    const N: i64 = 16;
    let (a, b) = two_instances(opts, conn).await;

    let start = Arc::new(Barrier::new(N as usize));
    let mut handles = Vec::new();
    for i in 0..N {
        let store = Arc::clone(if i % 2 == 0 { &a.store } else { &b.store });
        let start = Arc::clone(&start);
        handles.push(tokio::spawn(async move {
            // Every contender is at the door before any of them opens it.
            start.wait().await;
            match store.reserve(&request("warming", Some(N - 1), 1)).await {
                Ok(Reserved::Taken(r)) => {
                    store.commit(&r, &[]).await.expect("commit");
                    true
                }
                Ok(Reserved::NoHeadroom { .. }) => false,
                Err(e) => panic!("reserve failed: {e}"),
            }
        }));
    }

    let mut delivered = 0;
    for h in handles {
        if h.await.expect("task") {
            delivered += 1;
        }
    }

    assert_eq!(
        delivered,
        N - 1,
        "exactly N-1 must be granted across the two instances"
    );

    let usage = a.store.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!(usage.committed, N - 1, "and no overshoot in the row");
    assert_eq!(usage.reserved, 0, "with nothing left outstanding");

    // And the two instances agree about it, which is the other half of "one
    // instance owns its quota state" being unnecessary: there is no per-instance
    // view to diverge.
    let seen_by_b = b.store.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!(seen_by_b.committed, usage.committed);
}

#[sqlx::test]
async fn one_instances_open_reservation_blocks_the_others(
    opts: PgPoolOptions,
    conn: PgConnectOptions,
) {
    // The mechanism, not just the outcome. The test above would pass if the two
    // instances were serialised by luck; this one pins *why* they cannot race.
    //
    // Instance A opens the §7.4 transaction by hand and holds it. Instance B then
    // calls the real `reserve`. Two things must be true: B does not finish while A
    // holds the row, and when it does finish it sees what A wrote.
    //
    // **The row must already exist before either of them touches it.** That is
    // not incidental setup, it is the whole case: against a row that does not
    // exist yet, two `INSERT`s collide on the unique index and Postgres
    // serialises them whatever the conflict clause says — so a broken
    // `lock_usage` would still pass. `models::quota::lock_usage` says `ON
    // CONFLICT DO UPDATE` rather than `DO NOTHING` precisely because only the
    // former takes a lock on a row that is already there, and this test is the
    // one that holds it to that.
    let (a, b) = two_instances(opts, conn).await;

    let warm = a
        .store
        .reserve(&request("warming", Some(2), 1))
        .await
        .expect("reserve");
    let warm = match warm {
        Reserved::Taken(r) => r,
        Reserved::NoHeadroom { .. } => panic!("the row starts empty"),
    };
    a.store.commit(&warm, &[]).await.expect("commit");
    // Allowance 2, one committed. One slot left, and two instances about to want it.

    let mut tx = a.pool.begin().await.expect("begin");
    let usage = simmer::models::quota::lock_usage(&mut tx, "warming", "catchall", 0, Some(2))
        .await
        .expect("lock the existing row");
    assert!(usage.has_headroom_for(1), "one slot left");
    simmer::models::quota::insert_reservation(
        &mut tx,
        Uuid::new_v4(),
        &request("warming", Some(2), 1),
    )
    .await
    .expect("reserve inside the held transaction");
    // Not committed. A is mid-transaction, holding the row lock and the last slot.

    let store = Arc::clone(&b.store);
    let mut contender =
        tokio::spawn(async move { store.reserve(&request("warming", Some(2), 1)).await });

    // B must still be waiting. This is a wall-clock assertion, but only in the
    // safe direction: it can fail only if B got the row while A holds it, which
    // is the bug it is looking for. A slow machine makes it *more* likely to
    // pass, not less, so it is not a flaky test — it is a one-sided one.
    //
    // It is also *not* the assertion with the teeth, which is worth knowing
    // before trusting it. Replacing `lock_usage` with an unlocked read leaves
    // this one passing: B still blocks, just later and for a weaker reason — on
    // the `UPDATE` inside `insert_reservation`, having already made its headroom
    // decision against a stale read. Blocking is necessary and not sufficient.
    // The `NoHeadroom` assertion below is what fails in that case.
    let too_early =
        tokio::time::timeout(std::time::Duration::from_millis(500), &mut contender).await;
    assert!(
        too_early.is_err(),
        "the second instance reserved while the first held the row lock — \
         `lock_usage` is not serialising contenders and §7.4 is not cross-instance safe"
    );

    // Release it, and B goes through — seeing what A wrote, not what it read
    // before A wrote it. That second half is the one that matters: a lock that
    // merely delayed B without making it decide against fresh state would be no
    // lock at all, and is exactly what an unlocked `lock_usage` degrades to.
    tx.commit().await.expect("commit A");

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), contender)
        .await
        .expect("the contender unblocks once the row lock is released")
        .expect("task")
        .expect("reserve");
    assert!(
        matches!(outcome, Reserved::NoHeadroom { .. }),
        "the second instance must observe the first instance's reservation, \
         not the empty row it would have read had the check not been serialised"
    );
}

// ---------------------------------------------------------------------------
// §7.3 — not safe across instances, deliberately, and by how much (D-049)
// ---------------------------------------------------------------------------

/// A warming route with a `threshold: 2` daily window and an allowance far above
/// anything these tests send, so a route skipped for quota can never be mistaken
/// for a route skipped for frequency.
fn config() -> Config {
    let yaml = r#"
server:
  listeners:
    - address: "127.0.0.1:0"
  hostname: "simmer.test"
  max_message_bytes: 100000
  max_recipients: 1
  max_concurrent_sessions: 16
  allowed_cidrs: ["127.0.0.0/8"]
  timeouts: { command: 5s, data: 5s, session: 60s }
  auth: { allow_insecure_auth: true }
database: { url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }
admin: { listen: "127.0.0.1:0", auth_token: "t" }
logging: { level: warn, format: text }
domain_groups:
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
    identity: { envelope_from: "b@newbrand.com" }
    warmup:
      started: "2020-01-01T00:00:00Z"
      schedule: { default: [1000] }
    recipient_frequency:
      mode: to_address
      window: { unit: daily, count: 1 }
      threshold: 2
  - name: overflow
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: 2526
      tls: off
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
      timeouts: { connect: 2s, command: 2s, data: 2s }
    identity: { envelope_from: "b@established.com" }
"#;
    simmer::config::from_str(yaml, "test").expect("fixture is valid")
}

/// Count what §7.3 would count, under the salt the *instances* are using.
async fn events(pool: &PgPool, route: &str, address: &str) -> i64 {
    let salt = PgQuotaStore::new(pool.clone())
        .recipient_hash_salt()
        .await
        .expect("salt");
    let key = Keyer::new(salt).key_for(address, FrequencyMode::ToAddress, &[]);
    PgQuotaStore::new(pool.clone())
        .recipient_event_count(route, &key, Utc::now() - Duration::hours(24))
        .await
        .expect("count")
}

async fn seed_one_event(pool: &PgPool, route: &str, address: &str) {
    let salt = PgQuotaStore::new(pool.clone())
        .recipient_hash_salt()
        .await
        .expect("salt");
    let key = Keyer::new(salt).key_for(address, FrequencyMode::ToAddress, &[]);
    let mut tx = pool.begin().await.expect("begin");
    simmer::models::recipient_event::record(&mut tx, route, &[key], Utc::now())
        .await
        .expect("record");
    tx.commit().await.expect("commit");
}

/// One send, driven the way the relay drives it: walk (which reads the window and
/// reserves quota), then commit (which records the event). The barrier sits
/// between the two, which is the interleaving D-049 permits.
async fn send_through(
    instance: &Instance,
    cfg: &Config,
    recipient: &str,
    read_barrier: &Barrier,
) -> Option<String> {
    let mut evaluation = Vec::new();
    let chain = vec!["warming".to_string(), "overflow".to_string()];
    let walked = chain::walk_and_reserve(
        cfg,
        &simmer::routing::domain_group::Grouper::literal(),
        &instance.store,
        &instance.frequency,
        &simmer::preflight::Registry::new(),
        &chain,
        None,
        &[recipient.to_string()],
        "multi-instance-test",
        &mut evaluation,
    )
    .await
    .expect("walk");

    let selected = match walked {
        Walk::Exhausted => return None,
        Walk::Selected(s) => s,
    };

    // Every concurrent send has now finished reading the window and none has
    // written to it. This is not a contrived state: it is exactly what two
    // instances look like between the §7.3 read and the §7.4 commit, and forcing
    // it is the only way to make a race deterministic enough to assert on.
    read_barrier.wait().await;

    instance
        .store
        .commit(&selected.reservation, &selected.recipient_keys)
        .await
        .expect("commit");
    Some(selected.route.name.clone())
}

#[sqlx::test]
async fn two_instances_can_exceed_one_frequency_window_by_one(
    opts: PgPoolOptions,
    conn: PgConnectOptions,
) {
    // D-049, demonstrated rather than argued. The §7.3 count is read outside the
    // reservation transaction, so two sends that both read before either wrote
    // both see a window with room and both take it.
    //
    // Settled 2026-08-10: this stays. §7.3 is a reputation-shaping heuristic, not
    // an accounting invariant like quota, and holding the ramp's hot row lock
    // across a high-cardinality index read on every message costs more than the
    // one message it would save.
    let (a, b) = two_instances(opts, conn).await;
    let cfg = config();

    // One event against a threshold of two: one slot left, and two takers.
    seed_one_event(&a.pool, "warming", "bob@example.com").await;

    let barrier = Arc::new(Barrier::new(2));
    let (first, second) = tokio::join!(
        send_through(&a, &cfg, "bob@example.com", &barrier),
        send_through(&b, &cfg, "bob@example.com", &barrier),
    );

    assert_eq!(
        (first.as_deref(), second.as_deref()),
        (Some("warming"), Some("warming")),
        "both instances found the window under threshold, which is the race"
    );
    assert_eq!(
        events(&a.pool, "warming", "bob@example.com").await,
        3,
        "threshold 2, three delivered: over by exactly one"
    );
}

#[sqlx::test]
async fn the_frequency_overshoot_is_bounded_by_concurrency_not_by_a_constant(
    opts: PgPoolOptions,
    conn: PgConnectOptions,
) {
    // The honest statement of the bound, which is easy to get wrong in a
    // document: the window can be exceeded by one *per concurrent send*, not by
    // one overall. C sends that all read before any writes all deliver, so the
    // window reaches `threshold + (C - 1)`.
    //
    // This is what `docs/MULTI_INSTANCE.md` has to say, and it is the reason the
    // §7.3 threshold is not a guarantee at any level of concurrency — including
    // within a single instance, which is where the race already lived.
    const C: usize = 6;
    let (a, b) = two_instances(opts, conn).await;
    let cfg = Arc::new(config());

    seed_one_event(&a.pool, "warming", "bob@example.com").await;

    let barrier = Arc::new(Barrier::new(C));
    let mut handles = Vec::new();
    for i in 0..C {
        let instance = Arc::clone(if i % 2 == 0 { &a } else { &b });
        let cfg = Arc::clone(&cfg);
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            send_through(&instance, &cfg, "bob@example.com", &barrier).await
        }));
    }
    let mut selected = Vec::new();
    for h in handles {
        selected.push(h.await.expect("task"));
    }

    assert!(
        selected.iter().all(|s| s.as_deref() == Some("warming")),
        "every send in the race found the window under threshold"
    );

    let threshold = 2;
    assert_eq!(
        events(&a.pool, "warming", "bob@example.com").await,
        threshold + C as i64 - 1,
        "the window is exceeded by one per concurrent send, not by one in total"
    );
}

#[sqlx::test]
async fn sequential_sends_across_two_instances_respect_the_window(
    opts: PgPoolOptions,
    conn: PgConnectOptions,
) {
    // The other side of the same coin, and the reason the race above is bounded
    // rather than unbounded: with no overlap there is no over-delivery, whichever
    // instance the message arrives at. The second instance reads the first
    // instance's events through the same salt (D-050), so the window is genuinely
    // shared — it is only the read-then-write gap that is not serialised.
    let (a, b) = two_instances(opts, conn).await;
    let cfg = config();

    let solo = Barrier::new(1);
    assert_eq!(
        send_through(&a, &cfg, "bob@example.com", &solo)
            .await
            .as_deref(),
        Some("warming"),
    );
    assert_eq!(
        send_through(&b, &cfg, "bob@example.com", &solo)
            .await
            .as_deref(),
        Some("warming"),
        "second of two: still under threshold"
    );
    assert_eq!(
        send_through(&a, &cfg, "bob@example.com", &solo)
            .await
            .as_deref(),
        Some("overflow"),
        "at threshold the route is ineligible on the other instance too, and the \
         message steers rather than failing (§7.3)"
    );

    assert_eq!(
        events(&a.pool, "warming", "bob@example.com").await,
        2,
        "the window held exactly"
    );
}
