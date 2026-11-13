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
use simmer::quota::{QuotaError, QuotaStore, Reservation, ReserveRequest, Reserved, UsageKey};
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
        $harness!(an_over_cap_reservation_is_taken_and_counted);
        $harness!(an_over_cap_reservation_on_a_fresh_row_writes_the_allowance);
        $harness!(releasing_an_over_cap_reservation_gives_nothing_extra_back);
        $harness!(concurrent_over_cap_reservations_are_all_counted);
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
        $harness!(two_ramps_keep_separate_counters);
        $harness!(two_ramps_keep_separate_route_state);
        $harness!(two_ramps_keep_separate_events);
        $harness!(the_sweeper_releases_into_the_right_ramp);
        $harness!(a_reset_recomputes_from_its_own_ramps_reservations);
        $harness!(adoption_moves_every_legacy_row_into_the_ramp);
        $harness!(adoption_is_idempotent);
        $harness!(adoption_refuses_a_quota_key_that_exists_under_the_ramp);
        $harness!(adoption_refuses_a_route_state_that_exists_under_the_ramp);
        $harness!(a_new_row_records_its_step_and_keeps_it);
        $harness!(an_override_that_creates_the_row_records_its_step);
        $harness!(advance_rows_split_today_from_the_latest_earlier_row);
        $harness!(advance_rows_skip_days_with_no_row);
        $harness!(advance_rows_are_per_ramp_route_and_group);
        $harness!(only_the_reservation_that_creates_a_row_says_so);
        $harness!(exactly_one_racing_reservation_reports_creating_the_row);
        $harness!(a_commit_with_a_member_key_makes_a_member);
        $harness!(a_commit_without_a_member_key_makes_none);
        $harness!(an_undelivered_message_makes_no_member);
        $harness!(membership_is_per_ramp_and_route);
        $harness!(a_repeat_commit_keeps_one_member_row);
        $harness!(members_are_dropped_only_once_the_route_is_idle);
        $harness!(dropping_one_route_keeps_the_others);
        $harness!(racing_first_commits_for_one_recipient_make_one_member);
        $harness!(a_grouped_reservation_counts_on_both_rows);
        $harness!(a_grouped_release_gives_both_rows_back);
        $harness!(the_group_cap_binds_across_ramps);
        $harness!(the_route_cap_binds_when_the_group_has_room);
        $harness!(a_day_both_are_full_is_reported_as_the_groups);
        $harness!(a_refused_group_reservation_changes_nothing);
        $harness!(an_over_cap_grouped_reservation_counts_on_both_rows);
        $harness!(the_first_group_allowance_written_is_authoritative);
        $harness!(the_sweeper_releases_group_rows_too);
        $harness!(committing_a_swept_grouped_reservation_still_counts_on_both);
        $harness!(group_advance_rows_split_today_from_the_latest_earlier_row);
        $harness!(an_ungrouped_reservation_never_touches_a_group_row);
        $harness!(racing_ramps_never_overshoot_their_group);
        $harness!(exactly_one_racing_reservation_creates_the_group_row);
        $harness!(grouped_reserves_commits_and_releases_interleave_without_deadlock);
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
        ramp: "main".into(),
        route: route.into(),
        domain_group: group.into(),
        day_index: day,
        allowance,
        count,
        correlation_id: Uuid::new_v4().to_string(),
        expires_at: Utc::now() + Duration::minutes(10),
        over_cap: false,
        ..Default::default()
    }
}

/// `req`, in another ramp. `""` is the pre-ramps ramp the migration fills
/// existing rows with (D-099), which is how the adoption tests write the rows a
/// v0.8 instance left behind without any backend-specific SQL.
fn in_ramp(ramp: &str, req: ReserveRequest) -> ReserveRequest {
    ReserveRequest {
        ramp: ramp.into(),
        ..req
    }
}

fn over_cap(req: ReserveRequest) -> ReserveRequest {
    ReserveRequest {
        ramp: "main".into(),
        over_cap: true,
        ..req
    }
}

fn expired(route: &str, allowance: Option<i64>) -> ReserveRequest {
    ReserveRequest {
        ramp: "main".into(),
        expires_at: Utc::now() - Duration::minutes(1),
        ..request(route, allowance, 1)
    }
}

async fn taken(store: &Arc<dyn QuotaStore>, req: &ReserveRequest) -> Reservation {
    match store.reserve(req).await.expect("reserve") {
        Reserved::Taken(r) => r,
        Reserved::NoHeadroom { usage } => panic!("expected headroom, got {usage:?}"),
        Reserved::NoGroupHeadroom { usage } => {
            panic!("expected group headroom, got {usage:?}")
        }
    }
}

async fn refused(store: &Arc<dyn QuotaStore>, req: &ReserveRequest) {
    match store.reserve(req).await.expect("reserve") {
        Reserved::NoHeadroom { .. } => {}
        other => panic!("expected no headroom on the route, got {other:?}"),
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
    let mid = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((mid.reserved, mid.committed), (1, 0));

    s.commit(&r, &[]).await.unwrap();
    let after = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((after.reserved, after.committed), (0, 1));
    assert_eq!(after.allowance, Some(10));
}

pub async fn reserve_then_release_gives_the_slot_back(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &request("warming", Some(1), 1)).await;
    refused(&s, &request("warming", Some(1), 1)).await;
    s.release(&r).await.unwrap();

    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
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
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
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
    let u = s.usage("main", "overflow", "catchall", 0).await.unwrap();
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
        s.usage("main", "warming", "catchall", 0)
            .await
            .unwrap()
            .allowance,
        Some(1)
    );
}

/// D-090: a thread-affinity reply on its pinned route. Past the cap, but under
/// the same row lock and counted — `committed` ends above `allowance` and the
/// row says so, and the next ordinary reservation is still refused.
pub async fn an_over_cap_reservation_is_taken_and_counted(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &request("warming", Some(1), 1)).await;
    s.commit(&r, &[]).await.unwrap();
    refused(&s, &request("warming", Some(1), 1)).await;

    let r = taken(&s, &over_cap(request("warming", Some(1), 1))).await;
    s.commit(&r, &[]).await.unwrap();

    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved), (2, 0), "counted past the cap");
    assert_eq!(u.allowance, Some(1), "and the ceiling is untouched");
    refused(&s, &request("warming", Some(1), 1)).await;
}

/// D-026 still holds when the day's first reservation is past the cap: the row
/// is created with the schedule's ceiling, not an unlimited one, so the replies
/// that follow a zero-allowance day do not uncap it.
pub async fn an_over_cap_reservation_on_a_fresh_row_writes_the_allowance(stores: Stores<'_>) {
    let s = stores();
    taken(&s, &over_cap(request("warming", Some(0), 1))).await;
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!(u.allowance, Some(0));
    assert_eq!(u.reserved, 1);
    refused(&s, &request("warming", Some(0), 1)).await;
}

pub async fn releasing_an_over_cap_reservation_gives_nothing_extra_back(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &request("warming", Some(1), 1)).await;
    s.commit(&r, &[]).await.unwrap();
    let r = taken(&s, &over_cap(request("warming", Some(1), 1))).await;
    s.release(&r).await.unwrap();
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved), (1, 0));
    refused(&s, &request("warming", Some(1), 1)).await;
}

/// D-090 under contention: past the cap nothing is refused, so the risk is a
/// lost update rather than an overshoot — every over-cap send must land in
/// `committed`. Two pools, warmed, released by a barrier.
pub async fn concurrent_over_cap_reservations_are_all_counted(stores: Stores<'_>) {
    const N: usize = 16;
    let (a, b) = (stores(), stores());
    let r = taken(&a, &request("warming", Some(1), 1)).await;
    a.commit(&r, &[]).await.unwrap();
    warm(&[Arc::clone(&a), Arc::clone(&b)], N).await;

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
            let r = taken(&s, &over_cap(request("warming", Some(1), 1))).await;
            s.commit(&r, &[]).await.expect("commit");
        }));
    }
    for h in handles {
        h.await.expect("task");
    }

    let u = b.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved), (1 + N as i64, 0));
    assert_eq!(u.allowance, Some(1));
    refused(&a, &request("warming", Some(1), 1)).await;
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
    s.set_paused("main", "Warming", true).await.unwrap();
    let states = s.route_states("main").await.unwrap();
    assert!(states["Warming"].paused);
    assert!(!states.get("warming").map(|st| st.paused).unwrap_or(false));
}

pub async fn an_override_raises_the_ceiling_and_can_be_cleared(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &request("warming", Some(1), 1)).await;
    s.commit(&r, &[]).await.unwrap();
    refused(&s, &request("warming", Some(1), 1)).await;

    s.set_allowance_override("main", "warming", "catchall", 0, Some(3), Some(1), None)
        .await
        .unwrap();
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((u.allowance, u.allowance_override), (Some(1), Some(3)));
    taken(&s, &request("warming", Some(1), 1)).await;
    taken(&s, &request("warming", Some(1), 1)).await;
    refused(&s, &request("warming", Some(1), 1)).await;

    s.set_allowance_override("main", "warming", "catchall", 0, None, Some(1), None)
        .await
        .unwrap();
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((u.allowance, u.allowance_override), (Some(1), None));
}

pub async fn an_override_on_a_fresh_row_keeps_the_schedule_behind_it(stores: Stores<'_>) {
    // D-025: clearing an override on a row the override created leaves the
    // schedule's number, not a null (which would read as unlimited).
    let s = stores();
    s.set_allowance_override("main", "warming", "google", 4, Some(0), Some(20), None)
        .await
        .unwrap();
    refused(&s, &at("warming", "google", 4, Some(20), 1)).await;
    s.set_allowance_override("main", "warming", "google", 4, None, Some(20), None)
        .await
        .unwrap();
    let u = s.usage("main", "warming", "google", 4).await.unwrap();
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
    assert_eq!(
        s.usage("main", "warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        1
    );
    assert!(s.sweep_expired().await.unwrap().is_empty(), "idempotent");
}

pub async fn committing_after_the_sweeper_still_counts(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &expired("warming", Some(5))).await;
    s.sweep_expired().await.unwrap();
    s.commit(&r, &[]).await.unwrap();
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved), (1, 0));
}

pub async fn releasing_after_the_sweeper_is_a_no_op(stores: Stores<'_>) {
    let s = stores();
    let gone = taken(&s, &expired("warming", Some(5))).await;
    let live = taken(&s, &request("warming", Some(5), 1)).await;
    s.sweep_expired().await.unwrap();
    s.release(&gone).await.unwrap();
    // Decrementing again would have taken `live`'s headroom.
    assert_eq!(
        s.usage("main", "warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        1
    );
    s.release(&live).await.unwrap();
    assert_eq!(
        s.usage("main", "warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        0
    );
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
        .reset_counters("main", "warming", "catchall", 0)
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
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved, u.allowance), (0, 2, Some(5)));
}

pub async fn resetting_a_missing_row_is_none(stores: Stores<'_>) {
    let s = stores();
    assert!(s
        .reset_counters("main", "nowhere", "catchall", 0)
        .await
        .unwrap()
        .is_none());
}

pub async fn route_state_round_trips(stores: Stores<'_>) {
    let s = stores();
    assert!(s.route_states("main").await.unwrap().is_empty());
    s.set_paused("main", "a", true).await.unwrap();
    s.set_graduated("main", "b", true).await.unwrap();
    s.set_graduated("main", "a", true).await.unwrap();
    s.set_paused("main", "a", false).await.unwrap();

    let st = s.route_states("main").await.unwrap();
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
        .usage_many(
            "main",
            &[key("google", 2), key("yahoo", 2), key("microsoft", 2)],
        )
        .await
        .unwrap();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[&("warming".into(), "google".into())].reserved, 1);
    assert_eq!(got[&("warming".into(), "yahoo".into())].reserved, 3);
    assert!(s.usage_many("main", &[]).await.unwrap().is_empty());
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
                Ok(other) => panic!("ungrouped: {other:?}"),
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
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved), (N as i64 - 1, 0));
}

pub async fn two_independent_pools_never_overshoot(stores: Stores<'_>) {
    // D-061: the guarantee is the database's row lock, not anything in-process,
    // so two pools — two instances' worth of connections — must hold it too.
    const N: usize = 24;
    let (a, b) = (stores(), stores());
    let granted = race(&[a, Arc::clone(&b)], N, 10).await;
    assert_eq!(granted, 10);
    let u = b.usage("main", "warming", "catchall", 0).await.unwrap();
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
            a.usage("main", "fresh", "google", day)
                .await
                .unwrap()
                .reserved,
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
            let _ = s.route_states("main").await;
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
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
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
                    s.set_allowance_override(
                        "main",
                        &route,
                        "google",
                        round,
                        Some(5),
                        Some(1),
                        None,
                    )
                    .await
                } else if i % 3 == 1 {
                    s.set_paused("main", &route, true).await
                } else {
                    s.set_graduated("main", &route, true).await
                }
            }));
        }
        for h in handles {
            let got = h.await.expect("task");
            assert!(got.is_ok(), "round {round}: {got:?}");
        }
    }
    let st = a.route_states("main").await.unwrap();
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
        s.recipient_event_count("main", "warming", &jane, hour_ago)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        s.recipient_event_count("main", "other", &jane, hour_ago)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        s.recipient_event_count("main", "warming", &bob, hour_ago)
            .await
            .unwrap(),
        0
    );
    let later = Utc::now() + Duration::seconds(5);
    assert_eq!(
        s.recipient_event_count("main", "warming", &jane, later)
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
        s.recipient_event_count("main", "warming", &jane, since)
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
        s.recipient_event_count("main", "warming", &jane, since)
            .await
            .unwrap(),
        0
    );
}

pub async fn the_store_reports_itself_available(stores: Stores<'_>) {
    assert!(stores().is_available().await);
}

// ---------------------------------------------------------------------------
// D-099 — ramps share nothing
// ---------------------------------------------------------------------------

pub async fn two_ramps_keep_separate_counters(stores: Stores<'_>) {
    let s = stores();
    // One route name, one group, one day, in two ramps: two rows.
    let a = request("warming", Some(1), 1);
    taken(&s, &a).await;
    refused(&s, &a).await;
    let b = taken(&s, &in_ramp("brand-b", request("warming", Some(1), 1))).await;
    s.commit(&b, &[]).await.unwrap();

    let main = s.usage("main", "warming", "catchall", 0).await.unwrap();
    let other = s.usage("brand-b", "warming", "catchall", 0).await.unwrap();
    assert_eq!((main.reserved, main.committed), (1, 0));
    assert_eq!((other.reserved, other.committed), (0, 1));

    let keys = [UsageKey {
        route: "warming".into(),
        domain_group: "catchall".into(),
        day_index: 0,
    }];
    let many = s.usage_many("brand-b", &keys).await.unwrap();
    assert_eq!(many.len(), 1);
    assert_eq!(
        many[&("warming".to_string(), "catchall".to_string())].committed,
        1,
        "usage_many reads the ramp it was asked about"
    );
    assert!(s.usage_many("nobody", &keys).await.unwrap().is_empty());
}

pub async fn two_ramps_keep_separate_route_state(stores: Stores<'_>) {
    let s = stores();
    s.set_paused("main", "warming", true).await.unwrap();
    s.set_graduated("brand-b", "warming", true).await.unwrap();

    let main = s.route_states("main").await.unwrap();
    let other = s.route_states("brand-b").await.unwrap();
    assert_eq!(
        (main["warming"].paused, main["warming"].graduated),
        (true, false)
    );
    assert_eq!(
        (other["warming"].paused, other["warming"].graduated),
        (false, true)
    );
}

pub async fn two_ramps_keep_separate_events(stores: Stores<'_>) {
    let s = stores();
    let k = keyer(&s).await;
    let jane = k.key_for("jane@example.com", FrequencyMode::ToAddress, &[]);

    let r = taken(&s, &request("warming", Some(9), 1)).await;
    s.commit(&r, std::slice::from_ref(&jane)).await.unwrap();

    let hour_ago = Utc::now() - Duration::hours(1);
    assert_eq!(
        s.recipient_event_count("main", "warming", &jane, hour_ago)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        s.recipient_event_count("brand-b", "warming", &jane, hour_ago)
            .await
            .unwrap(),
        0,
        "§7.3's window is per ramp"
    );
}

pub async fn the_sweeper_releases_into_the_right_ramp(stores: Stores<'_>) {
    // The bug D-099's fence exists for: a sweep joining on (route, group, day)
    // alone would release one ramp's expired count from both ramps' rows.
    let s = stores();
    taken(&s, &expired("warming", Some(5))).await;
    taken(&s, &in_ramp("brand-b", request("warming", Some(5), 1))).await;

    let swept = s.sweep_expired().await.unwrap();
    assert_eq!(swept.len(), 1, "{swept:?}");
    assert_eq!(
        (
            swept[0].ramp.as_str(),
            swept[0].route.as_str(),
            swept[0].count
        ),
        ("main", "warming", 1)
    );
    let usage = |ramp: &'static str| {
        let s = s.clone();
        async move { s.usage(ramp, "warming", "catchall", 0).await.unwrap() }
    };
    assert_eq!(usage("main").await.reserved, 0);
    assert_eq!(usage("brand-b").await.reserved, 1, "untouched");
}

pub async fn a_reset_recomputes_from_its_own_ramps_reservations(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &request("warming", Some(5), 1)).await;
    s.commit(&r, &[]).await.unwrap();
    taken(&s, &in_ramp("brand-b", request("warming", Some(5), 2))).await;

    let reset = s
        .reset_counters("main", "warming", "catchall", 0)
        .await
        .unwrap()
        .expect("a row to reset");
    assert_eq!(reset.committed_before, 1);
    assert_eq!(
        reset.reserved_after, 0,
        "brand-b's live reservation is not main's"
    );
    let other = s.usage("brand-b", "warming", "catchall", 0).await.unwrap();
    assert_eq!(other.reserved, 2, "and brand-b's row is untouched");
}

// ---------------------------------------------------------------------------
// D-099 — adopting pre-ramps state
// ---------------------------------------------------------------------------

pub async fn adoption_moves_every_legacy_row_into_the_ramp(stores: Stores<'_>) {
    let s = stores();
    let k = keyer(&s).await;
    let jane = k.key_for("jane@example.com", FrequencyMode::ToAddress, &[]);

    // What a v0.8 instance leaves behind, after the migration's '' fill.
    let done = taken(&s, &in_ramp("", request("warming", Some(9), 1))).await;
    s.commit(&done, std::slice::from_ref(&jane)).await.unwrap();
    taken(&s, &in_ramp("", request("warming", Some(9), 1))).await; // in flight
    s.set_paused("", "warming", true).await.unwrap();
    s.set_graduated("", "retired", true).await.unwrap();

    let adopted = s.adopt_legacy_rows("main").await.unwrap();
    assert_eq!(adopted.quota_usage, 1);
    assert_eq!(adopted.quota_reservation, 1);
    assert_eq!(adopted.route_state, 2);
    assert_eq!(adopted.recipient_event, 1);
    assert_eq!(adopted.routes, vec!["retired", "warming"]);

    let usage = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((usage.committed, usage.reserved), (1, 1));
    assert!(s.route_states("main").await.unwrap()["warming"].paused);
    assert!(s.route_states("").await.unwrap().is_empty());
    let hour_ago = Utc::now() - Duration::hours(1);
    assert_eq!(
        s.recipient_event_count("main", "warming", &jane, hour_ago)
            .await
            .unwrap(),
        1
    );
}

pub async fn adoption_is_idempotent(stores: Stores<'_>) {
    let s = stores();
    taken(&s, &in_ramp("", request("warming", Some(9), 1))).await;
    // One reservation is two rows: the usage row and the reservation itself.
    assert_eq!(s.adopt_legacy_rows("main").await.unwrap().total(), 2);

    let again = s.adopt_legacy_rows("main").await.unwrap();
    assert_eq!(again.total(), 0);
    assert!(again.routes.is_empty());
    // A second instance starting later finds nothing to do either.
    let replica = stores();
    assert_eq!(replica.adopt_legacy_rows("main").await.unwrap().total(), 0);
}

pub async fn adoption_refuses_a_quota_key_that_exists_under_the_ramp(stores: Stores<'_>) {
    let s = stores();
    taken(&s, &in_ramp("", request("warming", Some(9), 1))).await;
    taken(&s, &request("warming", Some(9), 1)).await;

    let err = s.adopt_legacy_rows("main").await.unwrap_err();
    assert!(
        matches!(err, QuotaError::LegacyConflict(ref m) if m.contains("warming")),
        "{err}"
    );
    // Nothing moved: both rows are where they were.
    assert_eq!(
        s.usage("", "warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        1
    );
    assert_eq!(
        s.usage("main", "warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        1
    );
    // A different ramp has no clash.
    assert_eq!(s.adopt_legacy_rows("brand-b").await.unwrap().quota_usage, 1);
}

pub async fn adoption_refuses_a_route_state_that_exists_under_the_ramp(stores: Stores<'_>) {
    let s = stores();
    s.set_paused("", "warming", true).await.unwrap();
    s.set_graduated("main", "warming", true).await.unwrap();

    let err = s.adopt_legacy_rows("main").await.unwrap_err();
    assert!(
        matches!(err, QuotaError::LegacyConflict(ref m) if m.contains("route_state")),
        "{err}"
    );
    assert!(s.route_states("").await.unwrap()["warming"].paused);
}

// ---------------------------------------------------------------------------
// D-115 — the step a row is created at
// ---------------------------------------------------------------------------

fn stepped(route: &str, day: i64, allowance: i64, step: i64, count: i64) -> ReserveRequest {
    ReserveRequest {
        step: Some(step),
        ..at(route, "catchall", day, Some(allowance), count)
    }
}

pub async fn a_new_row_records_its_step_and_keeps_it(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &stepped("warming", 3, 50, 1, 1)).await;
    s.commit(&r, &[]).await.unwrap();
    // A later reservation offering a different step changes nothing: the row is
    // authoritative once written, like its allowance (D-026).
    let r = taken(&s, &stepped("warming", 3, 999, 2, 1)).await;
    s.commit(&r, &[]).await.unwrap();

    let rows = s
        .advance_rows("main", "warming", "catchall", 3)
        .await
        .unwrap();
    let today = rows.today.expect("today's row");
    assert_eq!(
        (
            today.day_index,
            today.step,
            today.allowance,
            today.committed
        ),
        (3, Some(1), Some(50), 2)
    );
}

pub async fn an_override_that_creates_the_row_records_its_step(stores: Stores<'_>) {
    let s = stores();
    s.set_allowance_override(
        "main",
        "warming",
        "catchall",
        4,
        Some(10),
        Some(50),
        Some(2),
    )
    .await
    .unwrap();
    let today = s
        .advance_rows("main", "warming", "catchall", 4)
        .await
        .unwrap()
        .today
        .expect("the override created it");
    // The planned allowance, not the override: D-115 measures against the plan.
    assert_eq!((today.step, today.allowance), (Some(2), Some(50)));
}

pub async fn advance_rows_split_today_from_the_latest_earlier_row(stores: Stores<'_>) {
    let s = stores();
    for (day, step) in [(0, 0), (1, 1), (2, 1)] {
        let r = taken(&s, &stepped("warming", day, 100, step, 1)).await;
        s.commit(&r, &[]).await.unwrap();
    }
    // Asking about day 2: today is day 2, previous is day 1.
    let rows = s
        .advance_rows("main", "warming", "catchall", 2)
        .await
        .unwrap();
    assert_eq!(rows.today.map(|r| r.day_index), Some(2));
    assert_eq!(
        rows.previous.map(|r| (r.day_index, r.step)),
        Some((1, Some(1)))
    );
    // Asking about day 3, which has no row: no today, previous is day 2.
    let rows = s
        .advance_rows("main", "warming", "catchall", 3)
        .await
        .unwrap();
    assert_eq!(rows.today, None);
    assert_eq!(rows.previous.map(|r| r.day_index), Some(2));
    // Asking about day 1 never sees day 2: nothing from the future.
    let rows = s
        .advance_rows("main", "warming", "catchall", 1)
        .await
        .unwrap();
    assert_eq!(rows.today.map(|r| r.day_index), Some(1));
    assert_eq!(rows.previous.map(|r| r.day_index), Some(0));
}

pub async fn advance_rows_skip_days_with_no_row(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &stepped("warming", 1, 100, 1, 1)).await;
    s.commit(&r, &[]).await.unwrap();
    // Days 2–9 sent nothing; day 10's previous is still day 1.
    let rows = s
        .advance_rows("main", "warming", "catchall", 10)
        .await
        .unwrap();
    assert_eq!(rows.today, None);
    let p = rows.previous.expect("day 1");
    assert_eq!((p.day_index, p.committed, p.allowance), (1, 1, Some(100)));
    // And an element that has never sent has neither.
    let none = s
        .advance_rows("main", "never", "catchall", 10)
        .await
        .unwrap();
    assert_eq!((none.today, none.previous), (None, None));
}

pub async fn advance_rows_are_per_ramp_route_and_group(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &stepped("warming", 0, 100, 0, 1)).await;
    s.commit(&r, &[]).await.unwrap();
    for (ramp, route, group) in [
        ("other", "warming", "catchall"),
        ("main", "warming-b", "catchall"),
        ("main", "warming", "google"),
    ] {
        let rows = s.advance_rows(ramp, route, group, 1).await.unwrap();
        assert_eq!(rows.previous, None, "{ramp}/{route}/{group}");
    }
}

pub async fn only_the_reservation_that_creates_a_row_says_so(stores: Stores<'_>) {
    let s = stores();
    let first = taken(&s, &stepped("warming", 0, 10, 0, 1)).await;
    let second = taken(&s, &stepped("warming", 0, 10, 0, 1)).await;
    assert!(first.created_row);
    assert!(!second.created_row);
    // A refused reservation on a fresh zero-allowance row creates nothing that
    // lasts, and the next attempt still finds no row to have been created.
    refused(&s, &stepped("empty", 0, 0, 0, 1)).await;
    assert_eq!(
        s.advance_rows("main", "empty", "catchall", 0)
            .await
            .unwrap()
            .today,
        None
    );
}

pub async fn exactly_one_racing_reservation_reports_creating_the_row(stores: Stores<'_>) {
    // D-115's counter counts each element's daily decision once across every
    // instance. Two pools, warmed, released together on fresh keys.
    const N: usize = 16;
    const ROUNDS: i64 = 10;
    let (a, b) = (stores(), stores());
    warm(&[Arc::clone(&a), Arc::clone(&b)], N).await;
    for day in 0..ROUNDS {
        let gate = Arc::new(tokio::sync::Barrier::new(N));
        let mut handles = Vec::new();
        for i in 0..N {
            let st = if i % 2 == 0 {
                Arc::clone(&a)
            } else {
                Arc::clone(&b)
            };
            let gate = Arc::clone(&gate);
            handles.push(tokio::spawn(async move {
                gate.wait().await;
                match st.reserve(&stepped("race", day, N as i64, 0, 1)).await {
                    Ok(Reserved::Taken(r)) => r.created_row,
                    other => panic!("every contender reserves: {other:?}"),
                }
            }));
        }
        let mut created = 0;
        for h in handles {
            if h.await.expect("task") {
                created += 1;
            }
        }
        assert_eq!(created, 1, "round {day}");
    }
}

// ---------------------------------------------------------------------------
// D-118 — sticky membership of an auto partial ramp
// ---------------------------------------------------------------------------

async fn member(store: &Arc<dyn QuotaStore>, recipient: &str) -> simmer::frequency::Key {
    keyer(store).await.key(&format!("{recipient}\0member"))
}

async fn is_member(
    store: &Arc<dyn QuotaStore>,
    ramp: &str,
    route: &str,
    key: &simmer::frequency::Key,
) -> bool {
    store
        .is_partial_member(ramp, route, key)
        .await
        .expect("is_partial_member")
}

pub async fn a_commit_with_a_member_key_makes_a_member(stores: Stores<'_>) {
    let s = stores();
    let k = member(&s, "a@example.com").await;
    assert!(!is_member(&s, "main", "warming", &k).await);
    let r = taken(&s, &request("warming", Some(10), 1)).await;
    s.commit_recording(&r, &[], Some(&k)).await.unwrap();
    assert!(is_member(&s, "main", "warming", &k).await);
    // Someone else is not.
    let other = member(&s, "b@example.com").await;
    assert!(!is_member(&s, "main", "warming", &other).await);
    // And the count moved exactly as a plain commit's does.
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((u.reserved, u.committed), (0, 1));
}

pub async fn a_commit_without_a_member_key_makes_none(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &request("warming", Some(10), 1)).await;
    s.commit(&r, &[]).await.unwrap();
    assert!(s.partial_member_routes().await.unwrap().is_empty());
}

pub async fn an_undelivered_message_makes_no_member(stores: Stores<'_>) {
    // Membership is "a message was COMMITTED to them on this route": a release
    // is not that, so it must leave no row behind.
    let s = stores();
    let k = member(&s, "a@example.com").await;
    let r = taken(&s, &request("warming", Some(10), 1)).await;
    s.release(&r).await.unwrap();
    assert!(!is_member(&s, "main", "warming", &k).await);
    assert!(s.partial_member_routes().await.unwrap().is_empty());
}

pub async fn membership_is_per_ramp_and_route(stores: Stores<'_>) {
    let s = stores();
    let k = member(&s, "a@example.com").await;
    let r = taken(&s, &request("warming", Some(10), 1)).await;
    s.commit_recording(&r, &[], Some(&k)).await.unwrap();
    assert!(is_member(&s, "main", "warming", &k).await);
    assert!(!is_member(&s, "main", "other", &k).await, "another route");
    assert!(
        !is_member(&s, "partner", "warming", &k).await,
        "another ramp"
    );
    assert!(
        !is_member(&s, "main", "Warming", &k).await,
        "case-sensitive"
    );
}

pub async fn a_repeat_commit_keeps_one_member_row(stores: Stores<'_>) {
    let s = stores();
    let k = member(&s, "a@example.com").await;
    for day in 0..3 {
        let r = taken(&s, &at("warming", "catchall", day, Some(10), 1)).await;
        s.commit_recording(&r, &[], Some(&k)).await.unwrap();
    }
    let routes = s.partial_member_routes().await.unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(
        (
            routes[0].ramp.as_str(),
            routes[0].route.as_str(),
            routes[0].members
        ),
        ("main", "warming", 1)
    );
}

pub async fn members_are_dropped_only_once_the_route_is_idle(stores: Stores<'_>) {
    let s = stores();
    let (a, b) = (
        member(&s, "a@example.com").await,
        member(&s, "b@example.com").await,
    );
    for k in [&a, &b] {
        let r = taken(&s, &request("warming", Some(10), 1)).await;
        s.commit_recording(&r, &[], Some(k)).await.unwrap();
    }
    let before = Utc::now();
    let last = s.partial_member_routes().await.unwrap()[0].last_committed_at;
    assert!(last <= before + Duration::seconds(5), "{last} vs {before}");

    // Idle since an hour ago: the route committed after that, so nothing goes.
    let kept = s
        .drop_partial_members("main", "warming", Utc::now() - Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(kept, 0);
    assert!(is_member(&s, "main", "warming", &a).await);

    // Idle since a minute from now: every commit was before it, so the route's
    // rows go together.
    let dropped = s
        .drop_partial_members("main", "warming", Utc::now() + Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(dropped, 2);
    assert!(!is_member(&s, "main", "warming", &a).await);
    assert!(!is_member(&s, "main", "warming", &b).await);
    assert!(s.partial_member_routes().await.unwrap().is_empty());
}

pub async fn dropping_one_route_keeps_the_others(stores: Stores<'_>) {
    let s = stores();
    let k = member(&s, "a@example.com").await;
    for (ramp, route) in [
        ("main", "warming"),
        ("main", "other"),
        ("partner", "warming"),
    ] {
        let r = taken(&s, &in_ramp(ramp, request(route, Some(10), 1))).await;
        s.commit_recording(&r, &[], Some(&k)).await.unwrap();
    }
    let n = s
        .drop_partial_members("main", "warming", Utc::now() + Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert!(!is_member(&s, "main", "warming", &k).await);
    assert!(is_member(&s, "main", "other", &k).await);
    assert!(is_member(&s, "partner", "warming", &k).await);
}

pub async fn racing_first_commits_for_one_recipient_make_one_member(stores: Stores<'_>) {
    // Two pools, warmed, released together: every contender is the recipient's
    // "first" commit. Insert-if-absent must leave exactly one row, and none of
    // the commits may fail on the duplicate.
    const N: usize = 12;
    let (a, b) = (stores(), stores());
    warm(&[Arc::clone(&a), Arc::clone(&b)], N).await;
    let k = member(&a, "racer@example.com").await;
    let mut reservations = Vec::new();
    for _ in 0..N {
        reservations.push(taken(&a, &request("warming", Some(N as i64), 1)).await);
    }
    let gate = Arc::new(tokio::sync::Barrier::new(N));
    let mut handles = Vec::new();
    for (i, r) in reservations.into_iter().enumerate() {
        let st = if i % 2 == 0 {
            Arc::clone(&a)
        } else {
            Arc::clone(&b)
        };
        let (gate, k) = (Arc::clone(&gate), k.clone());
        handles.push(tokio::spawn(async move {
            gate.wait().await;
            st.commit_recording(&r, &[], Some(&k)).await
        }));
    }
    for h in handles {
        h.await.expect("task").expect("every commit succeeds");
    }
    let routes = a.partial_member_routes().await.unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].members, 1);
    let u = a.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!(u.committed, N as i64);
}

// ---------------------------------------------------------------------------
// D-116 — ramp groups: one shared ceiling over several ramps' warming routes
// ---------------------------------------------------------------------------

/// `req`, also counted against `group`'s row for `day` at `allowance`.
fn grouped(req: ReserveRequest, group: &str, day: i64, allowance: i64) -> ReserveRequest {
    ReserveRequest {
        group: Some(simmer::quota::GroupReserve {
            name: group.into(),
            day_index: day,
            allowance,
            step: day.max(0),
        }),
        ..req
    }
}

async fn group_refused(store: &Arc<dyn QuotaStore>, req: &ReserveRequest) {
    match store.reserve(req).await.expect("reserve") {
        Reserved::NoGroupHeadroom { .. } => {}
        other => panic!("expected no group headroom, got {other:?}"),
    }
}

async fn group(store: &Arc<dyn QuotaStore>, name: &str, day: i64) -> simmer::quota::Usage {
    store.group_usage(name, day).await.expect("group_usage")
}

pub async fn a_grouped_reservation_counts_on_both_rows(stores: Stores<'_>) {
    let s = stores();
    let r = taken(
        &s,
        &grouped(request("warming", Some(10), 1), "brands", 0, 5),
    )
    .await;
    assert_eq!(
        r.ramp_group,
        Some(simmer::quota::GroupHold {
            name: "brands".into(),
            day_index: 0
        })
    );
    let g = group(&s, "brands", 0).await;
    assert_eq!((g.allowance, g.reserved, g.committed), (Some(5), 1, 0));

    s.commit(&r, &[]).await.unwrap();
    let g = group(&s, "brands", 0).await;
    assert_eq!((g.reserved, g.committed), (0, 1));
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((u.reserved, u.committed), (0, 1));
}

pub async fn a_grouped_release_gives_both_rows_back(stores: Stores<'_>) {
    let s = stores();
    let r = taken(
        &s,
        &grouped(request("warming", Some(10), 1), "brands", 0, 5),
    )
    .await;
    s.release(&r).await.unwrap();
    let g = group(&s, "brands", 0).await;
    assert_eq!((g.reserved, g.committed), (0, 0));
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((u.reserved, u.committed), (0, 0));
}

pub async fn the_group_cap_binds_across_ramps(stores: Stores<'_>) {
    // Two ramps, each with plenty of its own room, share a ceiling of 3.
    let s = stores();
    let main = || grouped(request("warming", Some(10), 1), "brands", 0, 3);
    let partner = || in_ramp("partner", main());
    for req in [main(), partner(), main()] {
        let r = taken(&s, &req).await;
        s.commit(&r, &[]).await.unwrap();
    }
    group_refused(&s, &partner()).await;
    group_refused(&s, &main()).await;

    let g = group(&s, "brands", 0).await;
    assert_eq!((g.committed, g.reserved), (3, 0));
    assert_eq!(
        s.usage("main", "warming", "catchall", 0)
            .await
            .unwrap()
            .committed,
        2
    );
    assert_eq!(
        s.usage("partner", "warming", "catchall", 0)
            .await
            .unwrap()
            .committed,
        1
    );
}

pub async fn the_route_cap_binds_when_the_group_has_room(stores: Stores<'_>) {
    let s = stores();
    let r = taken(
        &s,
        &grouped(request("warming", Some(1), 1), "brands", 0, 10),
    )
    .await;
    s.commit(&r, &[]).await.unwrap();
    refused(
        &s,
        &grouped(request("warming", Some(1), 1), "brands", 0, 10),
    )
    .await;
}

pub async fn a_day_both_are_full_is_reported_as_the_groups(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &grouped(request("warming", Some(1), 1), "brands", 0, 1)).await;
    s.commit(&r, &[]).await.unwrap();
    group_refused(&s, &grouped(request("warming", Some(1), 1), "brands", 0, 1)).await;
}

pub async fn a_refused_group_reservation_changes_nothing(stores: Stores<'_>) {
    let s = stores();
    let r = taken(
        &s,
        &grouped(request("warming", Some(10), 1), "brands", 0, 1),
    )
    .await;
    s.commit(&r, &[]).await.unwrap();
    group_refused(
        &s,
        &grouped(request("warming", Some(10), 1), "brands", 0, 1),
    )
    .await;
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!(
        (u.committed, u.reserved),
        (1, 0),
        "the route row is untouched"
    );
    let g = group(&s, "brands", 0).await;
    assert_eq!((g.committed, g.reserved), (1, 0));
}

pub async fn an_over_cap_grouped_reservation_counts_on_both_rows(stores: Stores<'_>) {
    // D-090 on a group: a pinned reply skips both checks and counts on both.
    let s = stores();
    let req = || grouped(request("warming", Some(1), 1), "brands", 0, 1);
    let r = taken(&s, &req()).await;
    s.commit(&r, &[]).await.unwrap();
    group_refused(&s, &req()).await;

    let r = taken(&s, &over_cap(req())).await;
    s.commit(&r, &[]).await.unwrap();
    let g = group(&s, "brands", 0).await;
    assert_eq!(
        (g.committed, g.allowance),
        (2, Some(1)),
        "past the group's cap"
    );
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!(u.committed, 2, "and the route's");
    // Nothing else is let through.
    group_refused(&s, &req()).await;
}

pub async fn the_first_group_allowance_written_is_authoritative(stores: Stores<'_>) {
    let s = stores();
    let first = taken(
        &s,
        &grouped(request("warming", Some(10), 1), "brands", 0, 2),
    )
    .await;
    assert!(first.created_group_row);
    let second = taken(
        &s,
        &grouped(request("warming", Some(10), 1), "brands", 0, 999),
    )
    .await;
    assert!(!second.created_group_row);
    assert_eq!(group(&s, "brands", 0).await.allowance, Some(2));
    group_refused(
        &s,
        &grouped(request("warming", Some(10), 1), "brands", 0, 999),
    )
    .await;
}

pub async fn the_sweeper_releases_group_rows_too(stores: Stores<'_>) {
    let s = stores();
    taken(&s, &grouped(expired("warming", Some(5)), "brands", 0, 5)).await;
    taken(&s, &grouped(expired("warming", Some(5)), "brands", 0, 5)).await;
    taken(&s, &grouped(request("warming", Some(5), 1), "brands", 0, 5)).await;

    let swept = s.sweep_expired().await.unwrap();
    assert_eq!(swept.len(), 1, "{swept:?}");
    assert_eq!(swept[0].count, 2);
    assert_eq!(group(&s, "brands", 0).await.reserved, 1);
    assert_eq!(
        s.usage("main", "warming", "catchall", 0)
            .await
            .unwrap()
            .reserved,
        1
    );
    assert!(s.sweep_expired().await.unwrap().is_empty(), "idempotent");
    assert_eq!(group(&s, "brands", 0).await.reserved, 1);
}

pub async fn committing_a_swept_grouped_reservation_still_counts_on_both(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &grouped(expired("warming", Some(5)), "brands", 0, 5)).await;
    s.sweep_expired().await.unwrap();
    s.commit(&r, &[]).await.unwrap();
    let g = group(&s, "brands", 0).await;
    assert_eq!((g.committed, g.reserved), (1, 0));
    let u = s.usage("main", "warming", "catchall", 0).await.unwrap();
    assert_eq!((u.committed, u.reserved), (1, 0));

    // And a release after the sweeper gives nothing back twice.
    let r = taken(&s, &grouped(expired("warming", Some(5)), "brands", 0, 5)).await;
    taken(&s, &grouped(request("warming", Some(5), 1), "brands", 0, 5)).await;
    s.sweep_expired().await.unwrap();
    s.release(&r).await.unwrap();
    assert_eq!(group(&s, "brands", 0).await.reserved, 1);
}

pub async fn group_advance_rows_split_today_from_the_latest_earlier_row(stores: Stores<'_>) {
    let s = stores();
    // Days 0 and 2 have rows; 1 and 3 do not.
    for (day, n) in [(0, 2), (2, 1)] {
        for _ in 0..n {
            let r = taken(
                &s,
                &grouped(
                    at("warming", "catchall", day, Some(10), 1),
                    "brands",
                    day,
                    4,
                ),
            )
            .await;
            s.commit(&r, &[]).await.unwrap();
        }
    }
    let rows = s.group_advance_rows("brands", 2).await.unwrap();
    let today = rows.today.expect("day 2");
    let prev = rows.previous.expect("day 0");
    assert_eq!(
        (
            today.day_index,
            today.step,
            today.allowance,
            today.committed
        ),
        (2, Some(2), Some(4), 1)
    );
    assert_eq!((prev.day_index, prev.committed), (0, 2));

    let rows = s.group_advance_rows("brands", 3).await.unwrap();
    assert!(rows.today.is_none());
    assert_eq!(rows.previous.map(|r| r.day_index), Some(2));

    assert_eq!(
        s.group_advance_rows("other", 3).await.unwrap(),
        Default::default()
    );
}

pub async fn an_ungrouped_reservation_never_touches_a_group_row(stores: Stores<'_>) {
    let s = stores();
    let r = taken(&s, &request("warming", Some(10), 1)).await;
    assert!(r.ramp_group.is_none() && !r.created_group_row);
    s.commit(&r, &[]).await.unwrap();
    assert_eq!(group(&s, "brands", 0).await, Default::default());
    assert_eq!(
        s.group_advance_rows("brands", 0).await.unwrap(),
        Default::default()
    );
}

pub async fn racing_ramps_never_overshoot_their_group(stores: Stores<'_>) {
    // Two pools, two ramps, routes with room to spare: only the group binds.
    const N: usize = 24;
    const CAP: i64 = 7;
    let (a, b) = (stores(), stores());
    warm(&[Arc::clone(&a), Arc::clone(&b)], N).await;
    let gate = Arc::new(tokio::sync::Barrier::new(N));
    let mut handles = Vec::new();
    for i in 0..N {
        let st = if i % 2 == 0 {
            Arc::clone(&a)
        } else {
            Arc::clone(&b)
        };
        let ramp = if i % 3 == 0 { "main" } else { "partner" };
        let gate = Arc::clone(&gate);
        handles.push(tokio::spawn(async move {
            let req = grouped(
                in_ramp(ramp, request("warming", Some(100), 1)),
                "brands",
                0,
                CAP,
            );
            gate.wait().await;
            match st.reserve(&req).await.expect("reserve") {
                Reserved::Taken(r) => {
                    st.commit(&r, &[]).await.expect("commit");
                    1
                }
                Reserved::NoGroupHeadroom { .. } => 0,
                other => panic!("only the group binds: {other:?}"),
            }
        }));
    }
    let mut won = 0;
    for h in handles {
        won += h.await.expect("task");
    }
    assert_eq!(won, CAP);
    let g = group(&a, "brands", 0).await;
    assert_eq!((g.committed, g.reserved), (CAP, 0));
}

pub async fn exactly_one_racing_reservation_creates_the_group_row(stores: Stores<'_>) {
    const N: usize = 16;
    let (a, b) = (stores(), stores());
    warm(&[Arc::clone(&a), Arc::clone(&b)], N).await;
    for day in 0..5 {
        let gate = Arc::new(tokio::sync::Barrier::new(N));
        let mut handles = Vec::new();
        for i in 0..N {
            let st = if i % 2 == 0 {
                Arc::clone(&a)
            } else {
                Arc::clone(&b)
            };
            let gate = Arc::clone(&gate);
            handles.push(tokio::spawn(async move {
                // Different routes, so only the group row is contended for
                // creation.
                let req = grouped(
                    at(&format!("r{i}"), "catchall", day, Some(10), 1),
                    "brands",
                    day,
                    N as i64,
                );
                gate.wait().await;
                match st.reserve(&req).await {
                    Ok(Reserved::Taken(r)) => r.created_group_row,
                    other => panic!("every contender reserves: {other:?}"),
                }
            }));
        }
        let mut created = 0;
        for h in handles {
            if h.await.expect("task") {
                created += 1;
            }
        }
        assert_eq!(created, 1, "day {day}");
    }
}

pub async fn grouped_reserves_commits_and_releases_interleave_without_deadlock(stores: Stores<'_>) {
    // Every transaction touching both rows takes the group's lock first. Mix
    // reserves, commits, releases and sweeps over two routes sharing a group,
    // from two pools: any order inversion shows up as a deadlock error here.
    const N: usize = 16;
    const ROUNDS: usize = 6;
    let (a, b) = (stores(), stores());
    warm(&[Arc::clone(&a), Arc::clone(&b)], N).await;
    let gate = Arc::new(tokio::sync::Barrier::new(N));
    let mut handles = Vec::new();
    for i in 0..N {
        let st = if i % 2 == 0 {
            Arc::clone(&a)
        } else {
            Arc::clone(&b)
        };
        let gate = Arc::clone(&gate);
        handles.push(tokio::spawn(async move {
            gate.wait().await;
            let mut committed = 0;
            for round in 0..ROUNDS {
                let route = if (i + round) % 2 == 0 { "w1" } else { "w2" };
                let req = grouped(request(route, Some(1000), 1), "brands", 0, 1000);
                let r = taken(&st, &req).await;
                if (i + round) % 3 == 0 {
                    st.release(&r).await.expect("release");
                } else {
                    st.commit(&r, &[]).await.expect("commit");
                    committed += 1;
                }
                if round == ROUNDS / 2 {
                    st.sweep_expired().await.expect("sweep");
                }
            }
            committed
        }));
    }
    let mut total = 0;
    for h in handles {
        total += h.await.expect("no task may fail");
    }
    let g = group(&a, "brands", 0).await;
    assert_eq!((g.committed, g.reserved), (total, 0));
    let w1 = a.usage("main", "w1", "catchall", 0).await.unwrap();
    let w2 = a.usage("main", "w2", "catchall", 0).await.unwrap();
    assert_eq!(w1.committed + w2.committed, total);
    assert_eq!(w1.reserved + w2.reserved, 0);
}
