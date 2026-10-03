//! §11 for the spool (D-116): the contract every backend's [`SpoolStore`] must
//! meet. Run by `tests/store_postgres.rs` and `tests/store_mssql.rs` through
//! [`spool_conformance_suite!`], like the quota suite in `mod.rs`.

use std::sync::Arc;

use chrono::{DateTime, Duration, TimeZone, Utc};
use simmer::config::FrequencyMode;
use simmer::frequency::Keyer;
use simmer::quota::{QuotaStore, Reservation, ReserveRequest, Reserved};
use simmer::spool::{
    BookedSlot, ClaimRequest, Claimed, DeadLetterRequest, DeadReason, NewSpooled, Reschedule,
    RetryDead, SpoolStore,
};
use uuid::Uuid;

/// A new spool store on a new pool, over the test's one database.
pub type SpoolStores<'a> = &'a (dyn Fn() -> Arc<dyn SpoolStore> + Sync);

#[macro_export]
macro_rules! spool_conformance_suite {
    ($harness:ident) => {
        $harness!(spool_enqueue_then_claim_round_trips_every_field);
        $harness!(spool_only_due_rows_are_claimed_oldest_first);
        $harness!(spool_a_claimed_row_is_not_claimed_again_until_its_lease_ends);
        $harness!(spool_claims_respect_the_batch);
        $harness!(spool_a_paused_ramp_is_not_claimed);
        $harness!(spool_reschedule_is_fenced_and_counts_attempts);
        $harness!(spool_reschedule_keeps_a_pin_and_replaces_a_booking);
        $harness!(spool_renew_is_fenced);
        $harness!(spool_dead_letter_is_fenced_and_keeps_or_clears_the_body);
        $harness!(spool_commit_and_complete_commits_the_quota_and_deletes_the_row);
        $harness!(spool_commit_and_complete_after_a_lost_lease_still_counts);
        $harness!(spool_commit_and_complete_records_events);
        $harness!(spool_dead_bodies_and_purge_by_cutoff);
        $harness!(spool_known_body_refs_reports_only_named_bodies);
        $harness!(spool_totals_and_lanes_count_live_messages);
        $harness!(spool_retry_dead_needs_the_body);
        $harness!(spool_delete_message_returns_its_body);
        $harness!(spool_ramp_state_round_trips);
        $harness!(spool_concurrent_claims_are_exclusive);
    };
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Whole seconds, so both backends' instants compare equal.
fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 1, 9, 0, 0).unwrap()
}

fn secs(n: i64) -> Duration {
    Duration::seconds(n)
}

fn message(ramp: &str, group: &str, due: DateTime<Utc>) -> NewSpooled {
    let id = Uuid::new_v4();
    NewSpooled {
        id,
        ramp: ramp.into(),
        domain_group: group.into(),
        group_basis: "literal".into(),
        received_at: t0() - secs(60),
        expires_at: t0() + Duration::hours(6),
        next_attempt_at: due,
        envelope: r#"{"mail_from":"a@b.test","recipients":["c@d.test"]}"#.into(),
        body_ref: format!("bodies/{id}"),
        body_bytes: 1234,
        body_sha256: vec![7; 32],
        uuid_seed: Uuid::new_v4(),
    }
}

async fn enqueued(s: &Arc<dyn SpoolStore>, m: NewSpooled) -> NewSpooled {
    s.enqueue(&m).await.expect("enqueue");
    m
}

fn claim_at(now: DateTime<Utc>, batch: u32) -> ClaimRequest {
    ClaimRequest {
        owner: "test".into(),
        now,
        batch,
        lease: secs(60),
    }
}

async fn claim(s: &Arc<dyn SpoolStore>, now: DateTime<Utc>, batch: u32) -> Vec<Claimed> {
    s.claim_due(&claim_at(now, batch)).await.expect("claim")
}

async fn claim_one(s: &Arc<dyn SpoolStore>, now: DateTime<Utc>) -> Claimed {
    let mut got = claim(s, now, 1).await;
    assert_eq!(got.len(), 1, "expected one claim, got {got:?}");
    got.remove(0)
}

fn reschedule(c: &Claimed, at: DateTime<Utc>, attempted: bool) -> Reschedule {
    Reschedule {
        id: c.id,
        token: c.lease_token,
        next_attempt_at: at,
        attempted,
        pinned_route: None,
        booked: None,
        last_code: Some(451),
        last_error: Some("try later".into()),
    }
}

fn dead(c: &Claimed, keep_body: bool) -> DeadLetterRequest {
    DeadLetterRequest {
        id: c.id,
        token: c.lease_token,
        reason: DeadReason::Rejected,
        at: t0(),
        attempted: true,
        pinned_route: Some("warming".into()),
        last_code: Some(550),
        last_error: Some("no such user".into()),
        keep_body,
    }
}

fn quota(s: &Arc<dyn SpoolStore>) -> Arc<dyn QuotaStore> {
    Arc::clone(s) as Arc<dyn QuotaStore>
}

async fn reservation(s: &Arc<dyn SpoolStore>, allowance: i64) -> Reservation {
    let req = ReserveRequest {
        ramp: "main".into(),
        route: "warming".into(),
        domain_group: "catchall".into(),
        day_index: 0,
        allowance: Some(allowance),
        count: 1,
        correlation_id: Uuid::new_v4().to_string(),
        expires_at: Utc::now() + Duration::minutes(10),
        over_cap: false,
    };
    match quota(s).reserve(&req).await.expect("reserve") {
        Reserved::Taken(r) => r,
        Reserved::NoHeadroom { usage } => panic!("no headroom: {usage:?}"),
    }
}

async fn usage(s: &Arc<dyn SpoolStore>) -> (i64, i64) {
    let u = quota(s)
        .usage("main", "warming", "catchall", 0)
        .await
        .expect("usage");
    (u.committed, u.reserved)
}

// ---------------------------------------------------------------------------
// the suite
// ---------------------------------------------------------------------------

pub async fn spool_enqueue_then_claim_round_trips_every_field(stores: SpoolStores<'_>) {
    let s = stores();
    let m = enqueued(&s, message("main", "google", t0())).await;
    let c = claim_one(&s, t0()).await;
    assert_eq!(c.id, m.id);
    assert_eq!(c.ramp, m.ramp);
    assert_eq!(c.domain_group, m.domain_group);
    assert_eq!(c.group_basis, m.group_basis);
    assert_eq!(c.received_at, m.received_at);
    assert_eq!(c.expires_at, m.expires_at);
    assert_eq!(c.next_attempt_at, m.next_attempt_at);
    assert_eq!(c.envelope, m.envelope);
    assert_eq!(c.body_ref.as_deref(), Some(m.body_ref.as_str()));
    assert_eq!(c.body_bytes, m.body_bytes);
    assert_eq!(c.body_sha256, m.body_sha256);
    assert_eq!(c.uuid_seed, m.uuid_seed);
    assert_eq!(c.attempts, 0);
    assert_eq!(c.pinned_route, None);
    assert_eq!(c.booked, None);
    assert_eq!(c.lease_until, t0() + secs(60));
}

pub async fn spool_only_due_rows_are_claimed_oldest_first(stores: SpoolStores<'_>) {
    let s = stores();
    let later = enqueued(&s, message("main", "google", t0() - secs(10))).await;
    let earlier = enqueued(&s, message("main", "google", t0() - secs(20))).await;
    let _future = enqueued(&s, message("main", "google", t0() + secs(1))).await;

    let got: Vec<Uuid> = claim(&s, t0(), 10).await.iter().map(|c| c.id).collect();
    assert_eq!(got, vec![earlier.id, later.id], "due only, oldest first");
}

pub async fn spool_a_claimed_row_is_not_claimed_again_until_its_lease_ends(
    stores: SpoolStores<'_>,
) {
    let s = stores();
    enqueued(&s, message("main", "google", t0())).await;
    let first = claim_one(&s, t0()).await;
    assert!(claim(&s, t0() + secs(59), 10).await.is_empty(), "leased");
    let second = claim_one(&s, t0() + secs(61)).await;
    assert_eq!(second.id, first.id);
    assert_ne!(second.lease_token, first.lease_token, "a fresh token");

    // The first holder's token no longer fences anything.
    assert!(!s
        .reschedule(&reschedule(&first, t0() + secs(100), true))
        .await
        .unwrap());
    assert!(s
        .reschedule(&reschedule(&second, t0() + secs(100), true))
        .await
        .unwrap());
}

pub async fn spool_claims_respect_the_batch(stores: SpoolStores<'_>) {
    let s = stores();
    for _ in 0..5 {
        enqueued(&s, message("main", "google", t0())).await;
    }
    assert_eq!(claim(&s, t0(), 3).await.len(), 3);
    assert_eq!(claim(&s, t0(), 3).await.len(), 2);
    assert!(claim(&s, t0(), 3).await.is_empty());
}

pub async fn spool_a_paused_ramp_is_not_claimed(stores: SpoolStores<'_>) {
    let s = stores();
    let m = enqueued(&s, message("other", "google", t0())).await;
    s.set_spool_paused("other", true).await.unwrap();
    assert!(claim(&s, t0(), 10).await.is_empty());
    s.set_spool_paused("other", false).await.unwrap();
    assert_eq!(claim_one(&s, t0()).await.id, m.id);
}

pub async fn spool_reschedule_is_fenced_and_counts_attempts(stores: SpoolStores<'_>) {
    let s = stores();
    enqueued(&s, message("main", "google", t0())).await;
    let c = claim_one(&s, t0()).await;
    let forged = Reschedule {
        token: Uuid::new_v4(),
        ..reschedule(&c, t0(), true)
    };
    assert!(!s.reschedule(&forged).await.unwrap(), "wrong token");

    assert!(s
        .reschedule(&reschedule(&c, t0() + secs(30), true))
        .await
        .unwrap());
    assert!(
        claim(&s, t0() + secs(29), 10).await.is_empty(),
        "not due yet"
    );
    let c = claim_one(&s, t0() + secs(30)).await;
    assert_eq!(c.attempts, 1);

    // A requeue that made no attempt (an exhausted pool) is not counted.
    assert!(s
        .reschedule(&reschedule(&c, t0() + secs(31), false))
        .await
        .unwrap());
    let c = claim_one(&s, t0() + secs(31)).await;
    assert_eq!(c.attempts, 1);

    // A token is spent by the reschedule that used it.
    assert!(s
        .reschedule(&reschedule(&c, t0() + secs(32), true))
        .await
        .unwrap());
    assert!(!s
        .reschedule(&reschedule(&c, t0() + secs(33), true))
        .await
        .unwrap());
}

pub async fn spool_reschedule_keeps_a_pin_and_replaces_a_booking(stores: SpoolStores<'_>) {
    let s = stores();
    enqueued(&s, message("main", "google", t0())).await;
    let c = claim_one(&s, t0()).await;
    let slot = BookedSlot {
        route: "warming".into(),
        domain_group: "google".into(),
        tat: t0() + secs(120),
    };
    assert!(s
        .reschedule(&Reschedule {
            pinned_route: Some("warming".into()),
            booked: Some(slot.clone()),
            ..reschedule(&c, t0() + secs(1), true)
        })
        .await
        .unwrap());
    let c = claim_one(&s, t0() + secs(1)).await;
    assert_eq!(c.pinned_route.as_deref(), Some("warming"));
    assert_eq!(c.booked, Some(slot));

    // No pin given: the existing one stays. No booking given: it is cleared.
    assert!(s
        .reschedule(&reschedule(&c, t0() + secs(2), true))
        .await
        .unwrap());
    let c = claim_one(&s, t0() + secs(2)).await;
    assert_eq!(c.pinned_route.as_deref(), Some("warming"));
    assert_eq!(c.booked, None);
}

pub async fn spool_renew_is_fenced(stores: SpoolStores<'_>) {
    let s = stores();
    enqueued(&s, message("main", "google", t0())).await;
    let c = claim_one(&s, t0()).await;
    assert!(!s
        .renew_lease(c.id, Uuid::new_v4(), t0() + secs(600))
        .await
        .unwrap());
    assert!(s
        .renew_lease(c.id, c.lease_token, t0() + secs(600))
        .await
        .unwrap());
    assert!(
        claim(&s, t0() + secs(599), 10).await.is_empty(),
        "the renewed lease holds"
    );
    assert_eq!(claim_one(&s, t0() + secs(601)).await.id, c.id);
}

pub async fn spool_dead_letter_is_fenced_and_keeps_or_clears_the_body(stores: SpoolStores<'_>) {
    let s = stores();
    let kept = enqueued(&s, message("main", "google", t0())).await;
    let gone = enqueued(&s, message("main", "google", t0() + secs(1))).await;

    let c = claim_one(&s, t0()).await;
    assert_eq!(c.id, kept.id);
    let forged = DeadLetterRequest {
        token: Uuid::new_v4(),
        ..dead(&c, true)
    };
    assert!(!s.dead_letter(&forged).await.unwrap());
    assert!(s.dead_letter(&dead(&c, true)).await.unwrap());

    let c = claim_one(&s, t0() + secs(1)).await;
    assert_eq!(c.id, gone.id);
    assert!(s.dead_letter(&dead(&c, false)).await.unwrap());

    assert!(
        claim(&s, t0() + Duration::days(1), 10).await.is_empty(),
        "a dead letter is never claimed"
    );
    let entries = s.dead_entries(10).await.unwrap();
    assert_eq!(entries.len(), 2);
    let by_id = |id| entries.iter().find(|e| e.id == id).expect("entry");
    let k = by_id(kept.id);
    assert!(k.body_retained);
    assert_eq!(k.reason, Some(DeadReason::Rejected));
    assert_eq!(k.last_code, Some(550));
    assert_eq!(k.route.as_deref(), Some("warming"));
    assert_eq!(k.attempts, 1);
    assert_eq!(k.dead_at, Some(t0()));
    assert!(!by_id(gone.id).body_retained);
}

pub async fn spool_commit_and_complete_commits_the_quota_and_deletes_the_row(
    stores: SpoolStores<'_>,
) {
    let s = stores();
    let m = enqueued(&s, message("main", "catchall", t0())).await;
    let c = claim_one(&s, t0()).await;
    let r = reservation(&s, 10).await;
    assert_eq!(usage(&s).await, (0, 1));

    assert!(s
        .commit_and_complete(&r, &[], c.id, c.lease_token)
        .await
        .unwrap());
    assert_eq!(usage(&s).await, (1, 0));
    assert!(claim(&s, t0() + Duration::days(1), 10).await.is_empty());
    assert_eq!(
        s.delete_message(m.id).await.unwrap(),
        None,
        "the row is gone"
    );
}

pub async fn spool_commit_and_complete_after_a_lost_lease_still_counts(stores: SpoolStores<'_>) {
    let s = stores();
    let m = enqueued(&s, message("main", "catchall", t0())).await;
    let stale = claim_one(&s, t0()).await;
    let current = claim_one(&s, t0() + secs(61)).await;
    assert_eq!(current.id, m.id);
    let r = reservation(&s, 10).await;

    // The stale holder was delivered to: the quota counts it, the row goes,
    // and the caller is told its lease was lost.
    assert!(!s
        .commit_and_complete(&r, &[], stale.id, stale.lease_token)
        .await
        .unwrap());
    assert_eq!(usage(&s).await, (1, 0));
    assert_eq!(s.delete_message(m.id).await.unwrap(), None);
    // And the current holder's fenced writes now find nothing.
    assert!(!s
        .reschedule(&reschedule(&current, t0() + secs(100), true))
        .await
        .unwrap());
}

pub async fn spool_commit_and_complete_records_events(stores: SpoolStores<'_>) {
    let s = stores();
    let q = quota(&s);
    let keyer = Keyer::new(q.recipient_hash_salt().await.unwrap());
    let jane = keyer.key_for("jane@example.com", FrequencyMode::ToAddress, &[]);
    enqueued(&s, message("main", "catchall", t0())).await;
    let c = claim_one(&s, t0()).await;
    let r = reservation(&s, 10).await;
    s.commit_and_complete(&r, std::slice::from_ref(&jane), c.id, c.lease_token)
        .await
        .unwrap();
    assert_eq!(
        q.recipient_event_count("main", "warming", &jane, Utc::now() - Duration::hours(1))
            .await
            .unwrap(),
        1
    );
}

pub async fn spool_dead_bodies_and_purge_by_cutoff(stores: SpoolStores<'_>) {
    let s = stores();
    let old = enqueued(&s, message("main", "google", t0())).await;
    let new = enqueued(&s, message("main", "google", t0() + secs(1))).await;
    let c = claim_one(&s, t0()).await;
    s.dead_letter(&DeadLetterRequest {
        at: t0() - Duration::hours(2),
        ..dead(&c, true)
    })
    .await
    .unwrap();
    let c = claim_one(&s, t0() + secs(1)).await;
    s.dead_letter(&dead(&c, true)).await.unwrap();

    let taken = s.take_dead_bodies(t0() - Duration::hours(1)).await.unwrap();
    assert_eq!(taken, vec![old.body_ref.clone()]);
    assert!(s
        .take_dead_bodies(t0() - Duration::hours(1))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        s.retry_dead(old.id, t0(), t0() + secs(60)).await.unwrap(),
        RetryDead::NoBody
    );

    // Purging past both returns the body still named, and removes both rows.
    let purged = s.purge_dead(t0() + secs(1)).await.unwrap();
    assert_eq!(purged, vec![new.body_ref.clone()]);
    assert!(s.dead_entries(10).await.unwrap().is_empty());
}

pub async fn spool_known_body_refs_reports_only_named_bodies(stores: SpoolStores<'_>) {
    let s = stores();
    let live = enqueued(&s, message("main", "google", t0())).await;
    let dropped = enqueued(&s, message("main", "google", t0() + secs(1))).await;
    claim_one(&s, t0()).await;
    let c = claim_one(&s, t0() + secs(1)).await;
    s.dead_letter(&dead(&c, false)).await.unwrap();

    let asked = vec![
        live.body_ref.clone(),
        dropped.body_ref.clone(),
        "bodies/never".to_string(),
    ];
    let known = s.known_body_refs(&asked).await.unwrap();
    assert_eq!(known.len(), 1);
    assert!(known.contains(&live.body_ref));
    assert!(s.known_body_refs(&[]).await.unwrap().is_empty());
}

pub async fn spool_totals_and_lanes_count_live_messages(stores: SpoolStores<'_>) {
    let s = stores();
    enqueued(&s, message("main", "google", t0() - secs(5))).await;
    enqueued(&s, message("main", "google", t0())).await;
    enqueued(&s, message("main", "yahoo", t0() + secs(5))).await;
    let other = enqueued(&s, message("other", "google", t0() + secs(10))).await;

    let totals = s.totals().await.unwrap();
    assert_eq!((totals.messages, totals.bytes), (4, 4 * 1234));
    assert_eq!(s.lane_depth("main", "google").await.unwrap(), 2);
    assert_eq!(s.lane_depth("main", "nowhere").await.unwrap(), 0);

    let mut lanes = s.lanes().await.unwrap();
    lanes.sort_by(|a, b| (&a.ramp, &a.domain_group).cmp(&(&b.ramp, &b.domain_group)));
    let shape: Vec<(&str, &str, i64)> = lanes
        .iter()
        .map(|l| (l.ramp.as_str(), l.domain_group.as_str(), l.depth))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("main", "google", 2),
            ("main", "yahoo", 1),
            ("other", "google", 1)
        ]
    );
    assert_eq!(lanes[0].next_attempt_at, Some(t0() - secs(5)));
    assert_eq!(lanes[0].oldest_received_at, Some(t0() - secs(60)));

    // A dead letter leaves the live count; with its body kept, not the bytes.
    let c = s
        .claim_due(&claim_at(t0() + secs(10), 10))
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.id == other.id)
        .expect("claimed");
    s.dead_letter(&dead(&c, true)).await.unwrap();
    let totals = s.totals().await.unwrap();
    assert_eq!((totals.messages, totals.bytes), (3, 4 * 1234));
}

pub async fn spool_retry_dead_needs_the_body(stores: SpoolStores<'_>) {
    let s = stores();
    let m = enqueued(&s, message("main", "google", t0())).await;
    let c = claim_one(&s, t0()).await;
    s.dead_letter(&dead(&c, true)).await.unwrap();

    assert_eq!(
        s.retry_dead(Uuid::new_v4(), t0(), t0()).await.unwrap(),
        RetryDead::NotFound
    );
    let later = t0() + Duration::hours(1);
    assert_eq!(
        s.retry_dead(m.id, later, later + Duration::hours(6))
            .await
            .unwrap(),
        RetryDead::Requeued
    );
    let c = claim_one(&s, later).await;
    assert_eq!(c.id, m.id);
    assert_eq!(c.expires_at, later + Duration::hours(6));
    assert_eq!(c.pinned_route.as_deref(), Some("warming"), "Q3: pin kept");
    assert!(s.dead_entries(10).await.unwrap().is_empty());
    // Only a dead letter can be retried.
    assert_eq!(
        s.retry_dead(m.id, later, later).await.unwrap(),
        RetryDead::NotFound
    );
}

pub async fn spool_delete_message_returns_its_body(stores: SpoolStores<'_>) {
    let s = stores();
    let m = enqueued(&s, message("main", "google", t0())).await;
    assert_eq!(
        s.delete_message(m.id).await.unwrap(),
        Some(Some(m.body_ref.clone()))
    );
    assert_eq!(s.delete_message(m.id).await.unwrap(), None);

    let m = enqueued(&s, message("main", "google", t0())).await;
    let c = claim_one(&s, t0()).await;
    s.dead_letter(&dead(&c, false)).await.unwrap();
    assert_eq!(s.delete_message(m.id).await.unwrap(), Some(None));
}

pub async fn spool_ramp_state_round_trips(stores: SpoolStores<'_>) {
    let s = stores();
    assert!(s.spool_states().await.unwrap().is_empty());
    s.set_spool_draining("main", true).await.unwrap();
    s.set_spool_paused("other", true).await.unwrap();
    let states = s.spool_states().await.unwrap();
    assert!(states["main"].draining && !states["main"].paused);
    assert!(states["other"].paused && !states["other"].draining);
    s.set_spool_draining("main", false).await.unwrap();
    assert!(!s.spool_states().await.unwrap()["main"].draining);
}

pub async fn spool_concurrent_claims_are_exclusive(stores: SpoolStores<'_>) {
    // The guarantee is SKIP LOCKED / READPAST under the row locks, across two
    // independent pools. Warmed, then released by a barrier, so the claimants
    // really overlap (D-084); every round enqueues fresh rows.
    const N: usize = 12;
    const ROWS: usize = 20;
    const ROUNDS: usize = 5;
    let (a, b) = (stores(), stores());
    warm(&[Arc::clone(&a), Arc::clone(&b)], N).await;

    for round in 0..ROUNDS {
        let now = t0() + Duration::hours(round as i64);
        for _ in 0..ROWS {
            enqueued(&a, message("main", "google", now)).await;
        }
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
                // A day's lease, so no earlier round's rows come due again.
                s.claim_due(&ClaimRequest {
                    lease: Duration::days(1),
                    ..claim_at(now, 3)
                })
                .await
            }));
        }
        let mut ids = Vec::new();
        for h in handles {
            ids.extend(
                h.await
                    .expect("task")
                    .expect("claim")
                    .into_iter()
                    .map(|c| c.id),
            );
        }
        // Exclusive, and nothing lost — but not necessarily all in the
        // concurrent round: a claimant passes over rows another is examining
        // (`SKIP LOCKED` / `READPAST`), and on SQL Server that can include rows
        // the other does not end up taking. The next poll takes them, which is
        // what the sequential claims below stand for (D-122).
        loop {
            let more = a
                .claim_due(&ClaimRequest {
                    lease: Duration::days(1),
                    ..claim_at(now, 50)
                })
                .await
                .expect("follow-up claim");
            if more.is_empty() {
                break;
            }
            ids.extend(more.into_iter().map(|c| c.id));
        }
        let distinct: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            distinct.len(),
            ids.len(),
            "round {round}: a row was claimed twice"
        );
        assert_eq!(ids.len(), ROWS, "round {round}: every due row claimed once");
    }
}

async fn warm(stores: &[Arc<dyn SpoolStore>], n: usize) {
    let gate = Arc::new(tokio::sync::Barrier::new(n));
    let mut handles = Vec::new();
    for i in 0..n {
        let s = Arc::clone(&stores[i % stores.len()]);
        let gate = Arc::clone(&gate);
        handles.push(tokio::spawn(async move {
            let _ = s.totals().await;
            gate.wait().await;
        }));
    }
    for h in handles {
        h.await.expect("warm");
    }
}
