//! §7.4 through the whole stack: client → Simmer → downstream → quota row.
//!
//! `tests/quota.rs` tests the protocol against the store directly. This asserts
//! the two claims that only hold if the *relay path* wires it up correctly:
//! §12.3's "a failed send leaves `committed` unchanged", and §3.2's fall-through
//! to overflow at exhaustion.

mod support;

use std::sync::Arc;

use simmer::quota::store::QuotaStore;
use simmer::quota::PgQuotaStore;
use sqlx::PgPool;
use support::{Act, FakeDownstream, Script, Simmer};

const BODY: &str = "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n";

/// A warming route capped at 2/day in front of an uncapped overflow, each with
/// its own downstream so a test can tell which one carried the message.
fn config(warming_port: u16, overflow_port: u16) -> String {
    format!(
        r#"
server:
  listen: "127.0.0.1:0"
  hostname: "simmer.test"
  max_message_bytes: 100000
  max_recipients: 5
  max_concurrent_sessions: 16
  allowed_cidrs: ["127.0.0.0/8"]
  timeouts: {{ command: 5s, data: 5s, session: 60s }}
  auth: {{ required: false, allow_insecure_auth: true }}
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
      port: {warming_port}
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
      timeouts: {{ connect: 2s, command: 2s, data: 2s }}
    identity: {{ envelope_from: "b@newbrand.com" }}
    warmup:
      started: "2020-01-01T00:00:00Z"
      schedule: {{ default: [2] }}
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

/// Today's index for the **warming** route, whose start is fixed in the fixture.
fn today() -> i64 {
    let started = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    simmer::quota::day::index(started, chrono::Utc::now())
}

/// Today's index for the **overflow** route.
///
/// A different number from [`today`], and deliberately so: D-024 gives an
/// overflow route a synthetic start of the Unix epoch, because it has no
/// `warmup.started` of its own. Warming routes bucket on their configured
/// anniversary, overflow routes on UTC midnight. The clocks are never compared —
/// they only key rows — but a test that reads the wrong one finds an empty row.
fn overflow_today() -> i64 {
    use chrono::TimeZone;
    simmer::quota::day::index(chrono::Utc.timestamp_opt(0, 0).unwrap(), chrono::Utc::now())
}

#[sqlx::test]
async fn a_delivered_message_increments_committed(pool: PgPool) {
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let store: Arc<dyn QuotaStore> = Arc::new(PgQuotaStore::new(pool.clone()));

    let simmer = Simmer::start_with_quota(
        &config(warm.addr.port(), over.addr.port()),
        Arc::clone(&store),
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c
        .deliver("jane@oldbrand.com", "bob@example.com", BODY)
        .await;
    assert_eq!(r.code, 250, "{r:?}");

    let usage = store.usage("warming", "catchall", today()).await.unwrap();
    assert_eq!(usage.committed, 1);
    assert_eq!(usage.reserved, 0, "the reservation was resolved");
    assert_eq!(usage.allowance, Some(2), "written from the schedule");
}

#[sqlx::test]
async fn a_failed_send_leaves_committed_unchanged(pool: PgPool) {
    // §12.3, verbatim: "assert that a failed send leaves `committed` unchanged".
    // §7.4: "Counters increment on downstream success only."
    let warm = FakeDownstream::start(Script::with(|s| {
        s.final_dot = Act::Reply(451, "4.3.0 try later");
    }))
    .await;
    let over = FakeDownstream::start(Script::default()).await;
    let store: Arc<dyn QuotaStore> = Arc::new(PgQuotaStore::new(pool.clone()));

    let simmer = Simmer::start_with_quota(
        &config(warm.addr.port(), over.addr.port()),
        Arc::clone(&store),
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c
        .deliver("jane@oldbrand.com", "bob@example.com", BODY)
        .await;
    assert_eq!(r.code, 451, "{r:?}");

    let usage = store.usage("warming", "catchall", today()).await.unwrap();
    assert_eq!(
        usage.committed, 0,
        "a failed send must not consume allowance"
    );
    assert_eq!(usage.reserved, 0, "and must not strand the reservation");
}

#[sqlx::test]
async fn a_downstream_connect_failure_also_leaves_committed_unchanged(pool: PgPool) {
    // The same claim for the row of §10.1 that never reaches the final dot.
    let dead = FakeDownstream::unreachable().await;
    let over = FakeDownstream::start(Script::default()).await;
    let store: Arc<dyn QuotaStore> = Arc::new(PgQuotaStore::new(pool.clone()));

    let simmer =
        Simmer::start_with_quota(&config(dead.port(), over.addr.port()), Arc::clone(&store)).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c
        .deliver("jane@oldbrand.com", "bob@example.com", BODY)
        .await;
    assert_eq!(r.code, 451, "{r:?}");

    // §3.3: no failover. The message must NOT have gone out via overflow — that
    // would emit under the wrong identity and corrupt the ramp.
    assert!(
        over.last().is_none(),
        "a downstream failure must not fall through to the next route"
    );

    let usage = store.usage("warming", "catchall", today()).await.unwrap();
    assert_eq!(usage.committed, 0);
    assert_eq!(usage.reserved, 0);
}

#[sqlx::test]
async fn traffic_falls_through_to_overflow_when_the_ramp_is_spent(pool: PgPool) {
    // The whole point of the component, end to end.
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let store: Arc<dyn QuotaStore> = Arc::new(PgQuotaStore::new(pool.clone()));

    let simmer = Simmer::start_with_quota(
        &config(warm.addr.port(), over.addr.port()),
        Arc::clone(&store),
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;

    // The schedule is [2], so two messages ride the warming route...
    for i in 0..2 {
        let r = c
            .deliver("jane@oldbrand.com", &format!("a{i}@example.com"), BODY)
            .await;
        assert_eq!(r.code, 250, "message {i}: {r:?}");
    }
    assert_eq!(warm.messages().len(), 2);
    assert_eq!(over.messages().len(), 0);

    // ...and the third spills over, still with a 250 to the client.
    let r = c.deliver("jane@oldbrand.com", "c@example.com", BODY).await;
    assert_eq!(r.code, 250, "{r:?}");
    assert_eq!(warm.messages().len(), 2, "the ramp is not exceeded");
    assert_eq!(over.messages().len(), 1, "and the overflow carried it");

    let warming = store.usage("warming", "catchall", today()).await.unwrap();
    assert_eq!(
        warming.committed, 2,
        "exactly the day's allowance, no overshoot"
    );

    // O-2 / D-024: the overflow route accounts too, which is what makes "how
    // much spilled today" answerable.
    let overflow = store
        .usage("overflow", "catchall", overflow_today())
        .await
        .unwrap();
    assert_eq!(overflow.committed, 1);
    assert_eq!(overflow.allowance, None, "but is never capped");
}

#[sqlx::test]
async fn a_failed_send_does_not_consume_the_ramp_it_reserved(pool: PgPool) {
    // The reservation must be given back, so the *next* message still gets the
    // warming route rather than being pushed to overflow by a failure.
    let warm = FakeDownstream::start(Script::with(|s| {
        s.final_dot = Act::Reply(451, "4.3.0 not now");
    }))
    .await;
    let over = FakeDownstream::start(Script::default()).await;
    let store: Arc<dyn QuotaStore> = Arc::new(PgQuotaStore::new(pool.clone()));

    let simmer = Simmer::start_with_quota(
        &config(warm.addr.port(), over.addr.port()),
        Arc::clone(&store),
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    for _ in 0..3 {
        let r = c
            .deliver("jane@oldbrand.com", "bob@example.com", BODY)
            .await;
        assert_eq!(r.code, 451, "{r:?}");
    }

    let usage = store.usage("warming", "catchall", today()).await.unwrap();
    assert_eq!(usage.committed, 0);
    assert_eq!(usage.reserved, 0, "three failures, three releases");
    assert!(over.last().is_none(), "§3.3: no failover, on every attempt");
}

#[sqlx::test]
async fn concurrent_sessions_never_overshoot_the_ramp(pool: PgPool) {
    // §12.3's concurrency requirement, through the real ingress rather than
    // against the store: N sessions, N-1 slots.
    const N: usize = 8;
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let store: Arc<dyn QuotaStore> = Arc::new(PgQuotaStore::new(pool.clone()));

    // Allowance of N-1 for this test only.
    let cfg = config(warm.addr.port(), over.addr.port())
        .replace("default: [2]", &format!("default: [{}]", N - 1));
    let simmer = Arc::new(Simmer::start_with_quota(&cfg, Arc::clone(&store)).await);

    let mut handles = Vec::new();
    for i in 0..N {
        let simmer = Arc::clone(&simmer);
        handles.push(tokio::spawn(async move {
            let mut c = simmer.connect().await;
            c.hello().await;
            c.deliver("jane@oldbrand.com", &format!("r{i}@example.com"), BODY)
                .await
                .code
        }));
    }

    let mut accepted = 0;
    for h in handles {
        if h.await.expect("task") == 250 {
            accepted += 1;
        }
    }

    // Every message is accepted — the overflow route catches the one that did
    // not fit, which is exactly what a chain is for.
    assert_eq!(accepted, N, "all {N} accepted");
    assert_eq!(
        warm.messages().len(),
        N - 1,
        "but the warming route carried exactly its allowance"
    );
    assert_eq!(over.messages().len(), 1, "and one spilled");

    let usage = store.usage("warming", "catchall", today()).await.unwrap();
    assert_eq!(usage.committed as usize, N - 1, "no overshoot in the row");
    assert_eq!(usage.reserved, 0);
}

#[sqlx::test]
async fn an_exhausted_chain_with_no_overflow_is_451(pool: PgPool) {
    // §10.3 and the whole of §14.1: a warming route at its ceiling must not
    // produce a 550, because that would permanently suppress a deliverable
    // recipient in systems that outlive Simmer by years.
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let store: Arc<dyn QuotaStore> = Arc::new(PgQuotaStore::new(pool.clone()));

    // `from_header` matching, so §5.4 pushes the decision to the final dot —
    // this is the *late* rejection path. The early one has its own test below.
    let cfg = config(warm.addr.port(), over.addr.port())
        .replace("chain: [warming, overflow]", "chain: [warming]")
        .replace("match_on: envelope", "match_on: from_header");
    let simmer = Simmer::start_with_quota(&cfg, Arc::clone(&store)).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    for _ in 0..2 {
        assert_eq!(
            c.deliver("jane@oldbrand.com", "bob@example.com", BODY)
                .await
                .code,
            250
        );
    }

    let r = c
        .deliver("jane@oldbrand.com", "bob@example.com", BODY)
        .await;
    assert_eq!(r.code, 451, "must be temporary, never 550: {r:?}");
    assert!(r.contains("4.7.1"), "{r:?}");
    assert!(r.contains("no eligible route"), "{r:?}");
}

#[sqlx::test]
async fn an_exhausted_chain_is_refused_at_rcpt_to_when_rules_are_envelope_only(pool: PgPool) {
    // §5.4's early decision, and O-1's split: the eligibility check runs at RCPT
    // TO and takes no reservation, so the body is never transferred.
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let store: Arc<dyn QuotaStore> = Arc::new(PgQuotaStore::new(pool.clone()));

    let cfg = config(warm.addr.port(), over.addr.port())
        .replace("chain: [warming, overflow]", "chain: [warming]");
    let simmer = Simmer::start_with_quota(&cfg, Arc::clone(&store)).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    for _ in 0..2 {
        assert_eq!(
            c.deliver("jane@oldbrand.com", "bob@example.com", BODY)
                .await
                .code,
            250
        );
    }

    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    let r = c.command("RCPT TO:<bob@example.com>").await;
    assert_eq!(
        r.code, 451,
        "rejected at RCPT TO, not at the final dot: {r:?}"
    );
    assert!(r.contains("4.7.1"), "{r:?}");
}
