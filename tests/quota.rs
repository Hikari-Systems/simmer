//! §7 against real Postgres.
//!
//! These use `#[sqlx::test]`, which creates a fresh database per test and applies
//! `migrations/` to it. They need `DATABASE_URL` — see the README. Faking the
//! database here would prove nothing: the whole argument for §7.4's three-phase
//! protocol is about what two transactions do to one row at the same time, and
//! that is a property of Postgres, not of Rust.

// Postgres-backed: the storage layer under test is `PgQuotaStore`. The SQL
// Server build runs the backend-neutral suite instead (tests/store_mssql.rs,
// D-084).
#![cfg(feature = "postgres")]

use std::sync::Arc;

use chrono::{Duration, Utc};
use simmer::config::Config;
use simmer::quota::store::{QuotaStore, ReserveRequest, Reserved};
use simmer::quota::{self, PgQuotaStore};
use simmer::routing::chain::{self, Walk};
use sqlx::PgPool;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A warming route capped at 3/day, then an uncapped overflow.
const CFG: &str = r#"
server:
  listeners:
    - address: "127.0.0.1:0"
  hostname: simmer.test
  max_message_bytes: 100000
  max_recipients: 10
  max_concurrent_sessions: 16
  allowed_cidrs: ["127.0.0.0/8"]
  timeouts: { command: 5s, data: 5s, session: 60s }
  auth: { allow_insecure_auth: true }
database: { url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }
admin: { listen: "127.0.0.1:0", auth_token: "t" }
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
      timeouts: { connect: 2s, command: 2s, data: 2s }
    identity: { envelope_from: "b@newbrand.com" }
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
    identity: { envelope_from: "b@established.com" }
"#;

fn config() -> Config {
    simmer::config::from_str(CFG, "test").expect("fixture is valid")
}

fn store(pool: PgPool) -> Arc<dyn QuotaStore> {
    Arc::new(PgQuotaStore::new(pool))
}

/// A §7.3 keyer with a fixed salt, so a test never depends on what the database
/// minted. Routes without a `recipient_frequency` never ask it for anything.
fn frequency() -> simmer::frequency::Frequency {
    simmer::frequency::Frequency::with_salt(b"a fixed salt for tests".to_vec())
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

async fn taken(store: &Arc<dyn QuotaStore>, req: &ReserveRequest) -> quota::Reservation {
    match store.reserve(req).await.expect("reserve") {
        Reserved::Taken(r) => r,
        Reserved::NoHeadroom { usage } => panic!("expected headroom, got {usage:?}"),
    }
}

// ---------------------------------------------------------------------------
// §7.4 — the three phases
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn reserving_then_committing_moves_the_count(pool: PgPool) {
    let store = store(pool);
    let r = taken(&store, &request("warming", Some(3), 1)).await;

    let mid = store.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!(mid.reserved, 1, "reserve holds the slot");
    assert_eq!(mid.committed, 0, "but does not spend it");

    store.commit(&r, &[]).await.expect("commit");

    let after = store.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!(after.reserved, 0);
    assert_eq!(after.committed, 1);
}

#[sqlx::test]
async fn reserving_then_releasing_gives_the_slot_back(pool: PgPool) {
    // §7.4: "Counters increment on downstream success only. A failed send must
    // not consume allowance."
    let store = store(pool);
    let r = taken(&store, &request("warming", Some(3), 1)).await;
    store.release(&r).await.expect("release");

    let after = store.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!(after.reserved, 0);
    assert_eq!(
        after.committed, 0,
        "a failed send leaves committed unchanged"
    );
}

#[sqlx::test]
async fn a_reservation_counts_against_headroom_before_it_commits(pool: PgPool) {
    // The race §7.4 exists to close, stated directly: an outstanding reservation
    // must occupy the slot even though nothing has been committed yet.
    let store = store(pool);
    let _first = taken(&store, &request("warming", Some(1), 1)).await;

    let second = store
        .reserve(&request("warming", Some(1), 1))
        .await
        .unwrap();
    assert!(
        matches!(second, Reserved::NoHeadroom { .. }),
        "an uncommitted reservation must still hold the slot"
    );
}

#[sqlx::test]
async fn the_allowance_is_exhausted_exactly_and_not_one_more(pool: PgPool) {
    let store = store(pool);
    for i in 0..3 {
        let r = taken(&store, &request("warming", Some(3), 1)).await;
        store.commit(&r, &[]).await.unwrap();
        assert_eq!(
            store
                .usage("warming", "catchall", 0)
                .await
                .unwrap()
                .committed,
            i + 1
        );
    }

    assert!(matches!(
        store
            .reserve(&request("warming", Some(3), 1))
            .await
            .unwrap(),
        Reserved::NoHeadroom { .. }
    ));
}

#[sqlx::test]
async fn a_multi_recipient_reservation_takes_its_whole_magnitude(pool: PgPool) {
    // O-7: "decremented per message, by the recipient count" — one reservation
    // of magnitude N, not N reservations.
    let store = store(pool);
    let r = taken(&store, &request("warming", Some(3), 3)).await;
    assert_eq!(
        store
            .usage("warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        3
    );

    // ...and it does not fit twice.
    assert!(matches!(
        store
            .reserve(&request("warming", Some(3), 3))
            .await
            .unwrap(),
        Reserved::NoHeadroom { .. }
    ));

    store.commit(&r, &[]).await.unwrap();
    assert_eq!(
        store
            .usage("warming", "catchall", 0)
            .await
            .unwrap()
            .committed,
        3
    );
}

#[sqlx::test]
async fn a_request_larger_than_the_whole_allowance_never_fits(pool: PgPool) {
    let store = store(pool);
    assert!(matches!(
        store
            .reserve(&request("warming", Some(3), 4))
            .await
            .unwrap(),
        Reserved::NoHeadroom { .. }
    ));
    assert_eq!(
        store
            .usage("warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        0
    );
}

// ---------------------------------------------------------------------------
// O-2 / D-024 — overflow routes account but are never limited
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn an_overflow_route_never_runs_out_but_still_counts(pool: PgPool) {
    // §3.1 says an overflow route is "never quota-limited"; the accounting is
    // what makes "how much is spilling to overflow" answerable, which is the
    // number that matters most during a warm-up.
    let store = store(pool);
    for _ in 0..50 {
        let r = taken(&store, &request("overflow", None, 1)).await;
        store.commit(&r, &[]).await.unwrap();
    }

    let usage = store.usage("overflow", "catchall", 0).await.unwrap();
    assert_eq!(usage.allowance, None, "no ceiling");
    assert_eq!(usage.committed, 50, "but a full count");
    assert!(usage.has_headroom_for(1_000_000));
}

// ---------------------------------------------------------------------------
// O-4 / D-026 — the allowance is authoritative once written
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn a_later_config_change_does_not_raise_todays_ceiling(pool: PgPool) {
    // The failure this prevents: edit the schedule at noon, restart, and the
    // restart authorises a burst on a day that was already half spent.
    let store = store(pool);
    let r = taken(&store, &request("warming", Some(3), 1)).await;
    store.commit(&r, &[]).await.unwrap();

    // Now pretend the config was edited to 500 and the process restarted.
    for _ in 0..2 {
        let r = taken(&store, &request("warming", Some(500), 1)).await;
        store.commit(&r, &[]).await.unwrap();
    }

    let usage = store.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!(
        usage.allowance,
        Some(3),
        "the row keeps the ceiling it was created with"
    );
    assert!(
        matches!(
            store
                .reserve(&request("warming", Some(500), 1))
                .await
                .unwrap(),
            Reserved::NoHeadroom { .. }
        ),
        "today stays capped at 3 despite the new schedule"
    );
}

#[sqlx::test]
async fn tomorrow_picks_up_the_new_ceiling(pool: PgPool) {
    // The other half of D-026: the change is not ignored, it is deferred to the
    // next day boundary.
    let store = store(pool);
    let mut today = request("warming", Some(3), 1);
    today.day_index = 0;
    store
        .commit(&taken(&store, &today).await, &[])
        .await
        .unwrap();

    let mut tomorrow = request("warming", Some(500), 1);
    tomorrow.day_index = 1;
    store
        .commit(&taken(&store, &tomorrow).await, &[])
        .await
        .unwrap();

    assert_eq!(
        store
            .usage("warming", "catchall", 1)
            .await
            .unwrap()
            .allowance,
        Some(500)
    );
}

#[sqlx::test]
async fn each_day_index_is_an_independent_bucket(pool: PgPool) {
    // §7.2: "An unused allowance does not carry over."
    let store = store(pool);
    for day in 0..3 {
        let mut req = request("warming", Some(3), 3);
        req.day_index = day;
        store.commit(&taken(&store, &req).await, &[]).await.unwrap();
    }

    for day in 0..3 {
        assert_eq!(
            store
                .usage("warming", "catchall", day)
                .await
                .unwrap()
                .committed,
            3,
            "day {day}"
        );
    }
}

#[sqlx::test]
async fn domain_groups_are_independent_buckets(pool: PgPool) {
    // §7.1: "mailbox providers throttle independently of one another."
    let store = store(pool);
    let mut google = request("warming", Some(1), 1);
    google.domain_group = "google".into();
    store
        .commit(&taken(&store, &google).await, &[])
        .await
        .unwrap();

    // google is now full...
    assert!(matches!(
        store.reserve(&google).await.unwrap(),
        Reserved::NoHeadroom { .. }
    ));
    // ...but catchall is untouched.
    let catchall = request("warming", Some(3), 1);
    assert!(matches!(
        store.reserve(&catchall).await.unwrap(),
        Reserved::Taken(_)
    ));
}

// ---------------------------------------------------------------------------
// O-3 / D-025 — the per-group allowance override
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn an_allowance_override_raises_todays_ceiling_for_one_group(pool: PgPool) {
    let store = store(pool.clone());
    for _ in 0..3 {
        store
            .commit(&taken(&store, &request("warming", Some(3), 1)).await, &[])
            .await
            .unwrap();
    }
    assert!(matches!(
        store
            .reserve(&request("warming", Some(3), 1))
            .await
            .unwrap(),
        Reserved::NoHeadroom { .. }
    ));

    simmer::models::route_state::set_allowance_override(
        &pool,
        "warming",
        "catchall",
        0,
        Some(10),
        Some(3),
    )
    .await
    .expect("override");

    assert!(matches!(
        store
            .reserve(&request("warming", Some(3), 1))
            .await
            .unwrap(),
        Reserved::Taken(_)
    ));

    // The scheduled value is still visible alongside it, so the mutation is
    // auditable rather than destructive.
    let usage = store.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!(usage.allowance, Some(3));
    assert_eq!(usage.allowance_override, Some(10));
    assert_eq!(usage.effective_allowance(), Some(10));
}

#[sqlx::test]
async fn an_override_expires_with_the_day_it_was_set_for(pool: PgPool) {
    // D-025: expiry needs no job. The override is a column on one day's row.
    let store = store(pool.clone());
    simmer::models::route_state::set_allowance_override(
        &pool,
        "warming",
        "catchall",
        0,
        Some(99),
        Some(3),
    )
    .await
    .unwrap();

    let mut tomorrow = request("warming", Some(3), 1);
    tomorrow.day_index = 1;
    taken(&store, &tomorrow).await;

    let usage = store.usage("warming", "catchall", 1).await.unwrap();
    assert_eq!(
        usage.allowance_override, None,
        "yesterday's override is gone"
    );
    assert_eq!(usage.effective_allowance(), Some(3));
}

#[sqlx::test]
async fn an_override_can_lower_a_ceiling_below_what_is_already_committed(pool: PgPool) {
    let store = store(pool.clone());
    store
        .commit(&taken(&store, &request("warming", Some(3), 3)).await, &[])
        .await
        .unwrap();

    simmer::models::route_state::set_allowance_override(
        &pool,
        "warming",
        "catchall",
        0,
        Some(1),
        Some(3),
    )
    .await
    .unwrap();

    // Must refuse rather than underflowing into "unlimited".
    assert!(matches!(
        store
            .reserve(&request("warming", Some(3), 1))
            .await
            .unwrap(),
        Reserved::NoHeadroom { .. }
    ));
}

// ---------------------------------------------------------------------------
// §7.4 — the sweeper
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn the_sweeper_releases_an_expired_reservation(pool: PgPool) {
    let store = store(pool);
    let mut req = request("warming", Some(3), 2);
    req.expires_at = Utc::now() - Duration::seconds(1);
    taken(&store, &req).await;

    assert_eq!(
        store
            .usage("warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        2
    );

    let expired = store.sweep_expired().await.expect("sweep");
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].route, "warming");
    assert_eq!(expired[0].count, 2);

    let after = store.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!(after.reserved, 0, "the headroom comes back");
    assert_eq!(after.committed, 0, "and nothing was spent");
}

#[sqlx::test]
async fn the_sweeper_leaves_live_reservations_alone(pool: PgPool) {
    let store = store(pool);
    taken(&store, &request("warming", Some(3), 1)).await;

    assert!(store.sweep_expired().await.unwrap().is_empty());
    assert_eq!(
        store
            .usage("warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        1
    );
}

#[sqlx::test]
async fn committing_after_the_sweeper_still_counts_the_delivery(pool: PgPool) {
    // The edge case: the send took longer than `expires_at`, the sweeper gave
    // the headroom back, and *then* the downstream said 250. The message was
    // delivered, so the ramp must count it — but `reserved` must not be
    // decremented twice, because that would steal from a different message's
    // live reservation.
    let store = store(pool);
    let mut slow = request("warming", Some(3), 1);
    slow.expires_at = Utc::now() - Duration::seconds(1);
    let r = taken(&store, &slow).await;

    // Another message reserves legitimately in the meantime.
    let other = taken(&store, &request("warming", Some(3), 1)).await;

    store.sweep_expired().await.unwrap();
    store.commit(&r, &[]).await.expect("commit after sweep");

    let usage = store.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!(usage.committed, 1, "the delivery is counted");
    assert_eq!(
        usage.reserved, 1,
        "the other message's reservation is untouched"
    );

    store.commit(&other, &[]).await.unwrap();
    assert_eq!(
        store
            .usage("warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        0
    );
}

#[sqlx::test]
async fn releasing_after_the_sweeper_is_a_no_op(pool: PgPool) {
    let store = store(pool);
    let mut slow = request("warming", Some(3), 1);
    slow.expires_at = Utc::now() - Duration::seconds(1);
    let r = taken(&store, &slow).await;
    let other = taken(&store, &request("warming", Some(3), 1)).await;

    store.sweep_expired().await.unwrap();
    store.release(&r).await.expect("release after sweep");

    assert_eq!(
        store
            .usage("warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        1,
        "must not double-release into another message's reservation"
    );
    let _ = other;
}

// ---------------------------------------------------------------------------
// §12.3 — the concurrency test
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn n_concurrent_reservations_against_n_minus_one_slots_never_overshoot(pool: PgPool) {
    // §12.3: "N concurrent sessions against a route with N-1 remaining
    // allowance; assert exactly N-1 delivered and no overshoot."
    //
    // This is the test the whole three-phase protocol exists for. A post-hoc
    // increment passes every other test in this file and fails this one.
    const N: i64 = 16;
    let store = store(pool.clone());

    let mut handles = Vec::new();
    for _ in 0..N {
        let store = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
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

    assert_eq!(delivered, N - 1, "exactly N-1 must be granted");

    let usage = store.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!(usage.committed, N - 1, "and no overshoot in the row");
    assert_eq!(usage.reserved, 0, "with nothing left outstanding");
}

#[sqlx::test]
async fn concurrent_reservations_that_all_fail_leave_committed_at_zero(pool: PgPool) {
    // The same race, with every send failing. §7.4: "A failed send must not
    // consume allowance."
    const N: usize = 12;
    let store = store(pool);

    let mut handles = Vec::new();
    for _ in 0..N {
        let store = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            if let Ok(Reserved::Taken(r)) = store.reserve(&request("warming", Some(3), 1)).await {
                store.release(&r).await.expect("release");
            }
        }));
    }
    for h in handles {
        h.await.expect("task");
    }

    let usage = store.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!(usage.committed, 0);
    assert_eq!(usage.reserved, 0, "every reservation was resolved");
}

// ---------------------------------------------------------------------------
// §10.4 — shutdown release
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn shutdown_releases_only_this_processs_reservations(pool: PgPool) {
    // D-007: "§10.4 shutdown releases only the reservations belonging to its own
    // in-flight sessions rather than truncating the table."
    let store = store(pool.clone());
    let mine = taken(&store, &request("warming", Some(3), 1)).await;
    let someone_elses = taken(&store, &request("warming", Some(3), 1)).await;

    let released = simmer::models::quota::release_by_ids(&pool, &[mine.id])
        .await
        .expect("release");
    assert_eq!(released, 1);

    let usage = store.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!(usage.reserved, 1, "the other reservation survives");

    store.commit(&someone_elses, &[]).await.unwrap();
    assert_eq!(
        store
            .usage("warming", "catchall", 0)
            .await
            .unwrap()
            .committed,
        1
    );
}

// ---------------------------------------------------------------------------
// §3.2 step 3 — the chain walk
// ---------------------------------------------------------------------------

async fn walk(
    cfg: &Config,
    store: &Arc<dyn QuotaStore>,
    recipient: &str,
) -> (Option<String>, String) {
    let mut evaluation = Vec::new();
    let chain = vec!["warming".to_string(), "overflow".to_string()];
    let result = chain::walk_and_reserve(
        cfg,
        &simmer::routing::domain_group::Grouper::literal(),
        store,
        &frequency(),
        &simmer::preflight::Registry::new(),
        &chain,
        None,
        &[recipient.to_string()],
        "test-correlation",
        &mut evaluation,
    )
    .await
    .expect("walk");

    let selected = match result {
        Walk::Selected(s) => {
            store.commit(&s.reservation, &[]).await.expect("commit");
            Some(s.route.name.clone())
        }
        Walk::Exhausted => None,
    };
    (selected, chain::render(&evaluation))
}

#[sqlx::test]
async fn the_walk_prefers_the_warming_route_until_it_is_full(pool: PgPool) {
    let cfg = config();
    let store = store(pool);

    // The default series is [3], so three messages ride the warming route.
    for i in 0..3 {
        let (route, _) = walk(&cfg, &store, "bob@example.com").await;
        assert_eq!(route.as_deref(), Some("warming"), "message {i}");
    }

    // The fourth falls through to overflow — §3.2 step 3, and the reason the
    // chain exists at all.
    let (route, trace) = walk(&cfg, &store, "bob@example.com").await;
    assert_eq!(route.as_deref(), Some("overflow"));
    assert_eq!(trace, "warming=quota,overflow=selected");
}

#[sqlx::test]
async fn the_walk_uses_the_recipients_own_domain_group(pool: PgPool) {
    // §3.2 step 2 and §7.1's second axis. The google override is [1], so a
    // single gmail message fills it while catchall still has room.
    let cfg = config();
    let store = store(pool);

    let (route, _) = walk(&cfg, &store, "bob@gmail.com").await;
    assert_eq!(route.as_deref(), Some("warming"));

    let (route, trace) = walk(&cfg, &store, "bob@gmail.com").await;
    assert_eq!(
        route.as_deref(),
        Some("overflow"),
        "google is full: {trace}"
    );

    // ...and a non-google recipient is unaffected.
    let (route, _) = walk(&cfg, &store, "bob@example.com").await;
    assert_eq!(route.as_deref(), Some("warming"));
}

#[sqlx::test]
async fn a_paused_route_is_skipped(pool: PgPool) {
    // §3.2 step 3a.
    let cfg = config();
    let store = store(pool.clone());
    simmer::models::route_state::set_paused(&pool, "warming", true)
        .await
        .expect("pause");

    let (route, trace) = walk(&cfg, &store, "bob@example.com").await;
    assert_eq!(route.as_deref(), Some("overflow"));
    assert_eq!(trace, "warming=paused,overflow=selected");

    // Resuming brings it straight back — no restart, and no cache to expire.
    simmer::models::route_state::set_paused(&pool, "warming", false)
        .await
        .unwrap();
    let (route, _) = walk(&cfg, &store, "bob@example.com").await;
    assert_eq!(route.as_deref(), Some("warming"));
}

#[sqlx::test]
async fn a_route_whose_warm_up_has_not_started_is_skipped(pool: PgPool) {
    // §7.2: "warmup.started in the future makes the route ineligible until it
    // arrives."
    let mut cfg = config();
    let future = Utc::now() + Duration::days(3);
    cfg.routes[0].warmup.as_mut().unwrap().started = future;

    let store = store(pool);
    let (route, trace) = walk(&cfg, &store, "bob@example.com").await;
    assert_eq!(route.as_deref(), Some("overflow"));
    assert_eq!(trace, "warming=not_started,overflow=selected");
}

#[sqlx::test]
async fn a_graduated_route_jumps_to_its_final_allowance(pool: PgPool) {
    // §9.3's graduate, against a longer schedule than the shared fixture's.
    let cfg =
        simmer::config::from_str(&CFG.replace("default: [3]", "default: [1, 2, 500]"), "test")
            .expect("valid");

    let store = store(pool.clone());
    simmer::models::route_state::set_graduated(&pool, "warming", true)
        .await
        .expect("graduate");

    // Day 0 would normally allow 1. Graduated, it allows the final 500.
    for _ in 0..5 {
        let (route, _) = walk(&cfg, &store, "bob@example.com").await;
        assert_eq!(route.as_deref(), Some("warming"));
    }
}

#[sqlx::test]
async fn an_exhausted_chain_with_no_overflow_selects_nothing(pool: PgPool) {
    // §3.2 step 4 — the condition §10.3 answers with 451.
    let cfg = config();
    let store = store(pool);

    let mut evaluation = Vec::new();
    let chain = vec!["warming".to_string()];
    for _ in 0..3 {
        let r = chain::walk_and_reserve(
            &cfg,
            &simmer::routing::domain_group::Grouper::literal(),
            &store,
            &frequency(),
            &simmer::preflight::Registry::new(),
            &chain,
            None,
            &["bob@example.com".to_string()],
            "c",
            &mut Vec::new(),
        )
        .await
        .unwrap();
        match r {
            Walk::Selected(s) => store.commit(&s.reservation, &[]).await.unwrap(),
            Walk::Exhausted => panic!("should still have headroom"),
        }
    }

    let r = chain::walk_and_reserve(
        &cfg,
        &simmer::routing::domain_group::Grouper::literal(),
        &store,
        &frequency(),
        &simmer::preflight::Registry::new(),
        &chain,
        None,
        &["bob@example.com".to_string()],
        "c",
        &mut evaluation,
    )
    .await
    .unwrap();
    assert!(matches!(r, Walk::Exhausted));
    assert_eq!(chain::render(&evaluation), "warming=quota");
}

#[sqlx::test]
async fn the_walk_leaves_no_reservation_behind_when_it_falls_through(pool: PgPool) {
    // A route that is skipped must not hold anything: only the selected route's
    // reservation exists when the walk returns.
    let cfg = config();
    let store = store(pool);
    for _ in 0..3 {
        walk(&cfg, &store, "bob@example.com").await;
    }
    walk(&cfg, &store, "bob@example.com").await;

    assert_eq!(
        store
            .usage("warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        0,
        "the skipped route holds nothing"
    );
}

// ---------------------------------------------------------------------------
// §5.4 — the early eligibility check
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn the_early_check_sees_an_exhausted_chain_without_reserving(pool: PgPool) {
    // O-1: eligibility is evaluated early and takes no reservation, so it is
    // safe to run at RCPT TO where the recipient count is not yet final.
    let cfg = config();
    let store = store(pool);
    let chain = vec!["warming".to_string()];

    assert!(chain::any_eligible(
        &cfg,
        &simmer::routing::domain_group::Grouper::literal(),
        &store,
        &chain,
        "bob@example.com"
    )
    .await
    .unwrap());
    // ...and asking did not consume anything.
    assert_eq!(
        store
            .usage("warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        0
    );

    for _ in 0..3 {
        let mut ev = Vec::new();
        if let Walk::Selected(s) = chain::walk_and_reserve(
            &cfg,
            &simmer::routing::domain_group::Grouper::literal(),
            &store,
            &frequency(),
            &simmer::preflight::Registry::new(),
            &chain,
            None,
            &["bob@example.com".to_string()],
            "c",
            &mut ev,
        )
        .await
        .unwrap()
        {
            store.commit(&s.reservation, &[]).await.unwrap();
        }
    }

    assert!(!chain::any_eligible(
        &cfg,
        &simmer::routing::domain_group::Grouper::literal(),
        &store,
        &chain,
        "bob@example.com"
    )
    .await
    .unwrap());
}

#[sqlx::test]
async fn the_early_check_always_passes_a_chain_ending_in_overflow(pool: PgPool) {
    let cfg = config();
    let store = store(pool);
    let chain = vec!["warming".to_string(), "overflow".to_string()];

    for _ in 0..10 {
        let mut ev = Vec::new();
        if let Walk::Selected(s) = chain::walk_and_reserve(
            &cfg,
            &simmer::routing::domain_group::Grouper::literal(),
            &store,
            &frequency(),
            &simmer::preflight::Registry::new(),
            &chain,
            None,
            &["bob@example.com".to_string()],
            "c",
            &mut ev,
        )
        .await
        .unwrap()
        {
            store.commit(&s.reservation, &[]).await.unwrap();
        }
    }

    assert!(
        chain::any_eligible(
            &cfg,
            &simmer::routing::domain_group::Grouper::literal(),
            &store,
            &chain,
            "bob@example.com"
        )
        .await
        .unwrap(),
        "an overflow route is never exhausted, so the chain never is"
    );
}
