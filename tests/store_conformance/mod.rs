//! The §11 storage contract, as one suite every backend must pass (D-084).
//!
//! Each test takes a [`Stores`] factory rather than a store: calling it twice
//! must yield two stores on **independent pools** over one database, which is
//! how the multi-instance tests put two processes' worth of connections on the
//! same rows. `tests/store_postgres.rs` runs the suite against Postgres and
//! `tests/store_mssql.rs` against SQL Server; the suite passing on Postgres is
//! what makes a pass on SQL Server mean something.
//!
//! This is the storage layer only. The relay, chain walk and admin tests in
//! `tests/quota*.rs`, `tests/frequency.rs` and `tests/admin_api.rs` stay
//! Postgres-backed: nothing above [`QuotaStore`] knows which backend it has.

#![allow(dead_code)]

use std::sync::Arc;

use chrono::{Duration, Utc};
use simmer::config::FrequencyMode;
use simmer::frequency::Keyer;
use simmer::quota::{QuotaStore, Reservation, ReserveRequest, Reserved, UsageKey};
use uuid::Uuid;

/// A new store on a new pool, over the test's one database.
pub type Stores<'a> = &'a (dyn Fn() -> Arc<dyn QuotaStore> + Sync);

/// Runs `$body` once per backend harness. `$harness` is a macro taking the test
/// name and the suite function.
#[macro_export]
macro_rules! conformance_suite {
    ($harness:ident) => {
        $harness!(reserve_then_commit_moves_the_count);
        $harness!(reserve_then_release_gives_the_slot_back);
        $harness!(the_allowance_is_exhausted_exactly);
        $harness!(a_multi_recipient_reservation_takes_its_whole_magnitude);
        $harness!(an_overflow_row_never_runs_out_but_counts);
        $harness!(the_first_allowance_written_is_authoritative);
        $harness!(buckets_are_independent_by_group_and_day);
        $harness!(route_names_are_case_sensitive);
        $harness!(an_override_raises_the_ceiling_and_can_be_cleared);
        $harness!(an_override_on_a_fresh_row_keeps_the_schedule_behind_it);
        $harness!(the_sweeper_releases_only_expired_reservations);
        $harness!(committing_after_the_sweeper_still_counts);
        $harness!(releasing_after_the_sweeper_is_a_no_op);
        $harness!(reset_zeroes_committed_and_keeps_live_reservations);
        $harness!(resetting_a_missing_row_is_none);
        $harness!(route_state_round_trips);
        $harness!(usage_many_reads_only_the_rows_asked_for);
        $harness!(n_concurrent_reservations_never_overshoot);
        $harness!(two_independent_pools_never_overshoot);
        $harness!(concurrent_first_reservations_create_one_row);
        $harness!(concurrent_failures_leave_nothing_committed);
        $harness!(the_salt_is_shared_and_stable);
        $harness!(racing_stores_agree_on_one_salt);
        $harness!(concurrent_admin_upserts_on_fresh_keys_do_not_collide);
        $harness!(events_are_counted_per_route_inside_the_window);
        $harness!(an_undelivered_message_records_no_event);
        $harness!(the_event_sweeper_evicts_by_cutoff);
        $harness!(the_store_reports_itself_available);
    };
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn request(route: &str, allowance: Option<i64>, count: i64) -> ReserveRequest {
    at(route, "catchall", 0, allowance, count)
}

fn at(route: &str, group: &str, day: i64, allowance: Option<i64>, count: i64) -> ReserveRequest {
    ReserveRequest {
        route: route.into(),
        domain_group: group.into(),
        day_index: day,
        allowance,
        count,
        correlation_id: Uuid::new_v4().to_string(),
        expires_at: Utc::now() + Duration::minutes(10),
    }
}

fn expired(route: &str, allowance: Option<i64>) -> ReserveRequest {
    ReserveRequest {
        expires_at: Utc::now() - Duration::minutes(1),
        ..request(route, allowance, 1)
    }
}

async fn taken(store: &Arc<dyn QuotaStore>, req: &ReserveRequest) -> Reservation {
    match store.reserve(req).await.expect("reserve") {
        Reserved::Taken(r) => r,
        Reserved::NoHeadroom { usage } => panic!("expected headroom, got {usage:?}"),
    }
}

async fn refused(store: &Arc<dyn QuotaStore>, req: &ReserveRequest) {
    match store.reserve(req).await.expect("reserve") {
        Reserved::NoHeadroom { .. } => {}
        Reserved::Taken(r) => panic!("expected no headroom, got {r:?}"),
    }
}

async fn keyer(store: &Arc<dyn QuotaStore>) -> Keyer {
    Keyer::new(store.recipient_hash_salt().await.expect("salt"))
}

// ---------------------------------------------------------------------------
// §7.4 — the three phases
// ---------------------------------------------------------------------------

pub async fn reserve_then_commit_moves_the_count(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &request("warming", Some(10), 1)).await;
    let mid = s.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!((mid.reserved, mid.committed), (1, 0));

    s.commit(&r, &[]).await.unwrap();
    let after = s.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!((after.reserved, after.committed), (0, 1));
    assert_eq!(after.allowance, Some(10));
}

pub async fn reserve_then_release_gives_the_slot_back(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &request("warming", Some(1), 1)).await;
    refused(&s, &request("warming", Some(1), 1)).await;
    s.release(&r).await.unwrap();

    let u = s.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!((u.reserved, u.committed), (0, 0));
    taken(&s, &request("warming", Some(1), 1)).await;
}

pub async fn the_allowance_is_exhausted_exactly(stores: Stores<'_>) {
    let s = stores();
    for _ in 0..3 {
        let r = taken(&s, &request("warming", Some(3), 1)).await;
        s.commit(&r, &[]).await.unwrap();
    }
    refused(&s, &request("warming", Some(3), 1)).await;
    let u = s.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved), (3, 0));
}

pub async fn a_multi_recipient_reservation_takes_its_whole_magnitude(stores: Stores<'_>) {
    let s = stores();
    taken(&s, &request("warming", Some(5), 4)).await;
    refused(&s, &request("warming", Some(5), 2)).await;
    taken(&s, &request("warming", Some(5), 1)).await;
    refused(&s, &request("warming", Some(5), 10)).await;
}

pub async fn an_overflow_row_never_runs_out_but_counts(stores: Stores<'_>) {
    let s = stores();
    for _ in 0..25 {
        let r = taken(&s, &request("overflow", None, 1)).await;
        s.commit(&r, &[]).await.unwrap();
    }
    let u = s.usage("overflow", "catchall", 0).await.unwrap();
    assert_eq!(u.allowance, None);
    assert_eq!(u.committed, 25);
}

pub async fn the_first_allowance_written_is_authoritative(stores: Stores<'_>) {
    // D-026: a restart with a new schedule must not raise today's ceiling.
    let s = stores();
    let r = taken(&s, &request("warming", Some(1), 1)).await;
    s.commit(&r, &[]).await.unwrap();
    refused(&s, &request("warming", Some(100), 1)).await;
    assert_eq!(
        s.usage("warming", "catchall", 0).await.unwrap().allowance,
        Some(1)
    );
}

pub async fn buckets_are_independent_by_group_and_day(stores: Stores<'_>) {
    let s = stores();
    taken(&s, &at("warming", "google", 0, Some(1), 1)).await;
    refused(&s, &at("warming", "google", 0, Some(1), 1)).await;
    taken(&s, &at("warming", "microsoft", 0, Some(1), 1)).await;
    taken(&s, &at("warming", "google", 1, Some(1), 1)).await;
    // A negative day index is a route that has not started (§7.2); it is still
    // a key like any other.
    taken(&s, &at("warming", "google", -3, Some(1), 1)).await;
}

pub async fn route_names_are_case_sensitive(stores: Stores<'_>) {
    // Postgres compares text by bytes. A backend whose default collation folds
    // case would merge these two routes' quota — the SQL Server migrations
    // declare a binary collation for exactly this.
    let s = stores();
    taken(&s, &request("warming", Some(1), 1)).await;
    taken(&s, &request("Warming", Some(1), 1)).await;
    s.set_paused("Warming", true).await.unwrap();
    let states = s.route_states().await.unwrap();
    assert!(states["Warming"].paused);
    assert!(!states.get("warming").map(|st| st.paused).unwrap_or(false));
}

pub async fn an_override_raises_the_ceiling_and_can_be_cleared(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &request("warming", Some(1), 1)).await;
    s.commit(&r, &[]).await.unwrap();
    refused(&s, &request("warming", Some(1), 1)).await;

    s.set_allowance_override("warming", "catchall", 0, Some(3), Some(1))
        .await
        .unwrap();
    let u = s.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!((u.allowance, u.allowance_override), (Some(1), Some(3)));
    taken(&s, &request("warming", Some(1), 1)).await;
    taken(&s, &request("warming", Some(1), 1)).await;
    refused(&s, &request("warming", Some(1), 1)).await;

    s.set_allowance_override("warming", "catchall", 0, None, Some(1))
        .await
        .unwrap();
    let u = s.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!((u.allowance, u.allowance_override), (Some(1), None));
}

pub async fn an_override_on_a_fresh_row_keeps_the_schedule_behind_it(stores: Stores<'_>) {
    // D-025: clearing an override on a row the override created leaves the
    // schedule's number, not a null (which would read as unlimited).
    let s = stores();
    s.set_allowance_override("warming", "google", 4, Some(0), Some(20))
        .await
        .unwrap();
    refused(&s, &at("warming", "google", 4, Some(20), 1)).await;
    s.set_allowance_override("warming", "google", 4, None, Some(20))
        .await
        .unwrap();
    let u = s.usage("warming", "google", 4).await.unwrap();
    assert_eq!((u.allowance, u.allowance_override), (Some(20), None));
}

// ---------------------------------------------------------------------------
// §7.4 — the sweeper
// ---------------------------------------------------------------------------

pub async fn the_sweeper_releases_only_expired_reservations(stores: Stores<'_>) {
    let s = stores();
    taken(&s, &expired("warming", Some(5))).await;
    taken(&s, &expired("warming", Some(5))).await;
    taken(&s, &request("warming", Some(5), 1)).await;

    let swept = s.sweep_expired().await.unwrap();
    assert_eq!(swept.len(), 1, "one row touched: {swept:?}");
    assert_eq!((swept[0].route.as_str(), swept[0].count), ("warming", 2));
    assert_eq!(s.usage("warming", "catchall", 0).await.unwrap().reserved, 1);
    assert!(s.sweep_expired().await.unwrap().is_empty(), "idempotent");
}

pub async fn committing_after_the_sweeper_still_counts(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &expired("warming", Some(5))).await;
    s.sweep_expired().await.unwrap();
    s.commit(&r, &[]).await.unwrap();
    let u = s.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved), (1, 0));
}

pub async fn releasing_after_the_sweeper_is_a_no_op(stores: Stores<'_>) {
    let s = stores();
    let gone = taken(&s, &expired("warming", Some(5))).await;
    let live = taken(&s, &request("warming", Some(5), 1)).await;
    s.sweep_expired().await.unwrap();
    s.release(&gone).await.unwrap();
    // Decrementing again would have taken `live`'s headroom.
    assert_eq!(s.usage("warming", "catchall", 0).await.unwrap().reserved, 1);
    s.release(&live).await.unwrap();
    assert_eq!(s.usage("warming", "catchall", 0).await.unwrap().reserved, 0);
}

// ---------------------------------------------------------------------------
// §9.3 — admin state
// ---------------------------------------------------------------------------

pub async fn reset_zeroes_committed_and_keeps_live_reservations(stores: Stores<'_>) {
    let s = stores();
    for _ in 0..2 {
        let r = taken(&s, &request("warming", Some(5), 1)).await;
        s.commit(&r, &[]).await.unwrap();
    }
    taken(&s, &request("warming", Some(5), 2)).await;

    let reset = s
        .reset_counters("warming", "catchall", 0)
        .await
        .unwrap()
        .expect("row exists");
    assert_eq!(
        (
            reset.committed_before,
            reset.reserved_before,
            reset.reserved_after
        ),
        (2, 2, 2)
    );
    let u = s.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved, u.allowance), (0, 2, Some(5)));
}

pub async fn resetting_a_missing_row_is_none(stores: Stores<'_>) {
    let s = stores();
    assert!(s
        .reset_counters("nowhere", "catchall", 0)
        .await
        .unwrap()
        .is_none());
}

pub async fn route_state_round_trips(stores: Stores<'_>) {
    let s = stores();
    assert!(s.route_states().await.unwrap().is_empty());
    s.set_paused("a", true).await.unwrap();
    s.set_graduated("b", true).await.unwrap();
    s.set_graduated("a", true).await.unwrap();
    s.set_paused("a", false).await.unwrap();

    let st = s.route_states().await.unwrap();
    assert_eq!((st["a"].paused, st["a"].graduated), (false, true));
    assert_eq!((st["b"].paused, st["b"].graduated), (false, true));
    assert_eq!(st.len(), 2);
}

pub async fn usage_many_reads_only_the_rows_asked_for(stores: Stores<'_>) {
    let s = stores();
    taken(&s, &at("warming", "google", 2, Some(9), 1)).await;
    taken(&s, &at("warming", "yahoo", 2, Some(9), 3)).await;
    taken(&s, &at("warming", "google", 1, Some(9), 5)).await;

    let key = |g: &str, d| UsageKey {
        route: "warming".into(),
        domain_group: g.into(),
        day_index: d,
    };
    let got = s
        .usage_many(&[key("google", 2), key("yahoo", 2), key("microsoft", 2)])
        .await
        .unwrap();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[&("warming".into(), "google".into())].reserved, 1);
    assert_eq!(got[&("warming".into(), "yahoo".into())].reserved, 3);
    assert!(s.usage_many(&[]).await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// §12.3 — concurrency
// ---------------------------------------------------------------------------

async fn race(stores: &[Arc<dyn QuotaStore>], n: usize, slots: i64) -> i64 {
    let mut handles = Vec::new();
    for i in 0..n {
        let s = Arc::clone(&stores[i % stores.len()]);
        handles.push(tokio::spawn(async move {
            match s.reserve(&request("warming", Some(slots), 1)).await {
                Ok(Reserved::Taken(r)) => {
                    s.commit(&r, &[]).await.expect("commit");
                    1
                }
                Ok(Reserved::NoHeadroom { .. }) => 0,
                Err(e) => panic!("reserve failed: {e}"),
            }
        }));
    }
    let mut granted = 0;
    for h in handles {
        granted += h.await.expect("task");
    }
    granted
}

pub async fn n_concurrent_reservations_never_overshoot(stores: Stores<'_>) {
    const N: usize = 16;
    let s = stores();
    let granted = race(&[Arc::clone(&s)], N, N as i64 - 1).await;
    assert_eq!(granted, N as i64 - 1);
    let u = s.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved), (N as i64 - 1, 0));
}

pub async fn two_independent_pools_never_overshoot(stores: Stores<'_>) {
    // D-061: the guarantee is the database's row lock, not anything in-process,
    // so two pools — two instances' worth of connections — must hold it too.
    const N: usize = 24;
    let (a, b) = (stores(), stores());
    let granted = race(&[a, Arc::clone(&b)], N, 10).await;
    assert_eq!(granted, 10);
    let u = b.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved), (10, 0));
}

pub async fn concurrent_first_reservations_create_one_row(stores: Stores<'_>) {
    // The row does not exist when the race starts, so every contender tries to
    // create it. Exactly one may; the rest must wait and then see it — a
    // duplicate-key error here is the upsert race the SQL Server store's
    // `SERIALIZABLE` hint exists to close.
    //
    // The race has to be real to prove anything. Both pools are filled first,
    // so no contender spends its head start on a login, and a barrier releases
    // them together; twenty fresh keys give the interleaving twenty chances.
    const N: usize = 16;
    const ROUNDS: i64 = 20;
    let (a, b) = (stores(), stores());
    warm(&[Arc::clone(&a), Arc::clone(&b)], N).await;

    for day in 0..ROUNDS {
        let gate = Arc::new(tokio::sync::Barrier::new(N));
        let mut handles = Vec::new();
        for i in 0..N {
            let s = if i % 2 == 0 {
                Arc::clone(&a)
            } else {
                Arc::clone(&b)
            };
            let gate = Arc::clone(&gate);
            handles.push(tokio::spawn(async move {
                gate.wait().await;
                s.reserve(&at("fresh", "google", day, Some(N as i64), 1))
                    .await
                    .map(|r| matches!(r, Reserved::Taken(_)))
            }));
        }
        for h in handles {
            let got = h.await.expect("task");
            assert!(
                got.as_ref().is_ok_and(|taken| *taken),
                "round {day}: every contender must reserve, got {got:?}"
            );
        }
        assert_eq!(
            a.usage("fresh", "google", day).await.unwrap().reserved,
            N as i64
        );
    }
}

/// Open `n` connections across `stores` and return them to their pools, so a
/// race that follows is decided by the database rather than by who logged in
/// first.
async fn warm(stores: &[Arc<dyn QuotaStore>], n: usize) {
    let gate = Arc::new(tokio::sync::Barrier::new(n));
    let mut handles = Vec::new();
    for i in 0..n {
        let s = Arc::clone(&stores[i % stores.len()]);
        let gate = Arc::clone(&gate);
        handles.push(tokio::spawn(async move {
            // Hold a connection until all n are open, so each task gets its own.
            let _ = s.route_states().await;
            gate.wait().await;
        }));
    }
    for h in handles {
        h.await.expect("warm");
    }
}

pub async fn concurrent_failures_leave_nothing_committed(stores: Stores<'_>) {
    const N: usize = 12;
    let s = stores();
    let mut handles = Vec::new();
    for _ in 0..N {
        let s = Arc::clone(&s);
        handles.push(tokio::spawn(async move {
            if let Ok(Reserved::Taken(r)) = s.reserve(&request("warming", Some(3), 1)).await {
                s.release(&r).await.expect("release");
            }
        }));
    }
    for h in handles {
        h.await.expect("task");
    }
    let u = s.usage("warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved), (0, 0));
}

// ---------------------------------------------------------------------------
// §7.3 — the salt and the events
// ---------------------------------------------------------------------------

pub async fn the_salt_is_shared_and_stable(stores: Stores<'_>) {
    let (a, b) = (stores(), stores());
    let first = a.recipient_hash_salt().await.unwrap();
    assert!(first.len() >= 16);
    assert_eq!(a.recipient_hash_salt().await.unwrap(), first);
    assert_eq!(b.recipient_hash_salt().await.unwrap(), first);
}

pub async fn racing_stores_agree_on_one_salt(stores: Stores<'_>) {
    // Sixteen fresh instances starting together, each minting its own salt and
    // offering it: exactly one may win, and all of them must then read that one.
    // A single shot at one fixed key, so it catches a missing lock often rather
    // than always; the admin-upsert test below proves the same pattern every run.
    const N: usize = 16;
    let all: Vec<_> = (0..N).map(|_| stores()).collect();
    warm(&all, N).await;
    let gate = Arc::new(tokio::sync::Barrier::new(N));
    let mut handles = Vec::new();
    for s in all {
        let gate = Arc::clone(&gate);
        handles.push(tokio::spawn(async move {
            gate.wait().await;
            s.recipient_hash_salt().await
        }));
    }
    let mut salts = Vec::new();
    for h in handles {
        salts.push(h.await.expect("task").expect("salt"));
    }
    assert!(
        salts.windows(2).all(|w| w[0] == w[1]),
        "one salt, not several"
    );
}

pub async fn concurrent_admin_upserts_on_fresh_keys_do_not_collide(stores: Stores<'_>) {
    // §9.3's writes create their row when it is absent, the same upsert shape
    // as the reservation — and the same race on a key nobody has written yet.
    const N: usize = 12;
    let (a, b) = (stores(), stores());
    warm(&[Arc::clone(&a), Arc::clone(&b)], N).await;
    for round in 0..10i64 {
        let gate = Arc::new(tokio::sync::Barrier::new(N));
        let mut handles = Vec::new();
        for i in 0..N {
            let s = if i % 2 == 0 {
                Arc::clone(&a)
            } else {
                Arc::clone(&b)
            };
            let gate = Arc::clone(&gate);
            handles.push(tokio::spawn(async move {
                gate.wait().await;
                let route = format!("r{round}");
                if i % 3 == 0 {
                    s.set_allowance_override(&route, "google", round, Some(5), Some(1))
                        .await
                } else if i % 3 == 1 {
                    s.set_paused(&route, true).await
                } else {
                    s.set_graduated(&route, true).await
                }
            }));
        }
        for h in handles {
            let got = h.await.expect("task");
            assert!(got.is_ok(), "round {round}: {got:?}");
        }
    }
    let st = a.route_states().await.unwrap();
    assert_eq!(st.len(), 10);
    assert!(st.values().all(|s| s.paused && s.graduated));
}

pub async fn events_are_counted_per_route_inside_the_window(stores: Stores<'_>) {
    let s = stores();
    let k = keyer(&s).await;
    let jane = k.key_for("jane@example.com", FrequencyMode::ToAddress, &[]);
    let bob = k.key_for("bob@example.com", FrequencyMode::ToAddress, &[]);

    for _ in 0..2 {
        let r = taken(&s, &request("warming", Some(9), 1)).await;
        s.commit(&r, std::slice::from_ref(&jane)).await.unwrap();
    }
    let r = taken(&s, &request("other", Some(9), 1)).await;
    s.commit(&r, &[jane.clone(), bob.clone()]).await.unwrap();

    let hour_ago = Utc::now() - Duration::hours(1);
    assert_eq!(
        s.recipient_event_count("warming", &jane, hour_ago)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        s.recipient_event_count("other", &jane, hour_ago)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        s.recipient_event_count("warming", &bob, hour_ago)
            .await
            .unwrap(),
        0
    );
    let later = Utc::now() + Duration::seconds(5);
    assert_eq!(
        s.recipient_event_count("warming", &jane, later)
            .await
            .unwrap(),
        0
    );
}

pub async fn an_undelivered_message_records_no_event(stores: Stores<'_>) {
    let s = stores();
    let jane = keyer(&s)
        .await
        .key_for("jane@example.com", FrequencyMode::ToAddress, &[]);
    let r = taken(&s, &request("warming", Some(9), 1)).await;
    s.release(&r).await.unwrap();
    let since = Utc::now() - Duration::hours(1);
    assert_eq!(
        s.recipient_event_count("warming", &jane, since)
            .await
            .unwrap(),
        0
    );
}

pub async fn the_event_sweeper_evicts_by_cutoff(stores: Stores<'_>) {
    let s = stores();
    let jane = keyer(&s)
        .await
        .key_for("jane@example.com", FrequencyMode::ToAddress, &[]);
    for _ in 0..3 {
        let r = taken(&s, &request("warming", Some(9), 1)).await;
        s.commit(&r, std::slice::from_ref(&jane)).await.unwrap();
    }
    assert_eq!(
        s.sweep_recipient_events(Utc::now() - Duration::hours(1))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        s.sweep_recipient_events(Utc::now() + Duration::seconds(5))
            .await
            .unwrap(),
        3
    );
    let since = Utc::now() - Duration::hours(1);
    assert_eq!(
        s.recipient_event_count("warming", &jane, since)
            .await
            .unwrap(),
        0
    );
}

pub async fn the_store_reports_itself_available(stores: Stores<'_>) {
    assert!(stores().is_available().await);
}
