//! §7.7 (D-116 – D-124) end to end: a `delivery: spool` ramp over a real
//! Postgres store, a real volume, the real dispatcher and the scripted fake
//! downstream.
//!
//! Real time throughout, not a paused clock: Simmer and the fake talk over
//! TCP, and a paused runtime auto-advances whenever every task waits on I/O,
//! which fires the stage timeouts (D-112). The fixtures keep every interval
//! short instead.

#![cfg(feature = "postgres")]

mod support;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use simmer::quota::{PgQuotaStore, QuotaStore};
use simmer::spool::{ClaimRequest, DeadReason, SpoolStore};
use sqlx::PgPool;
use support::{config_for, without_received, Act, FakeDownstream, Script, Simmer, Turn};

const BODY: &str =
    "From: App <app@oldbrand.com>\r\nTo: bob@example.com\r\nSubject: spooled\r\n\r\nhello\r\n";

/// `config_for` with the ramp spooling into `dir`. `route` continues the
/// route's keys (four spaces); `spool` continues the `spool:` block (two).
fn spooled(addr: std::net::SocketAddr, dir: &Path, route: &str, spool: &str) -> String {
    config_for(
        addr,
        &format!(
            "{route}  delivery: spool\nspool:\n  body_store: {{ kind: volume, path: \"{}\" }}\n  \
             dispatch: {{ poll_interval: 50ms, batch: 8 }}\n  \
             retry: {{ initial: 200ms, max: 400ms, factor: 2.0 }}\n{spool}",
            dir.display()
        ),
    )
}

fn store(pool: PgPool) -> Arc<PgQuotaStore> {
    Arc::new(PgQuotaStore::new(pool))
}

async fn wait_until<F, Fut>(what: &str, within: Duration, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn bodies_in(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().ends_with(".eml"))
                .count()
        })
        .unwrap_or(0)
}

async fn send(simmer: &Simmer) -> support::Reply {
    let mut c = simmer.connect().await;
    c.hello().await;
    c.deliver("app@oldbrand.com", "bob@example.com", BODY).await
}

#[sqlx::test]
async fn a_spooled_message_is_queued_then_delivered(pool: PgPool) {
    let fake = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let s = store(pool.clone());
    let simmer = Simmer::start_spooled(&spooled(fake.addr, dir.path(), "", ""), s.clone()).await;

    let reply = send(&simmer).await;
    assert_eq!(reply.code, 250, "{reply:?}");
    assert!(reply.contains("queued as"), "{reply:?}");

    wait_until("delivery", Duration::from_secs(5), || async {
        fake.messages().len() == 1
    })
    .await;
    assert_eq!(without_received(&fake.last().unwrap().body), BODY);
    wait_until(
        "the row and the body to go",
        Duration::from_secs(5),
        || async { s.totals().await.unwrap().messages == 0 && bodies_in(dir.path()) == 0 },
    )
    .await;
    // An overflow route's day is counted from the epoch (D-024); whichever it
    // is, there is one row and it has the send.
    let committed: i64 = sqlx::query_scalar(
        "SELECT committed FROM quota_usage WHERE ramp = 'main' AND route = 'only'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(committed, 1, "the quota is committed with the row (D-122)");
}

#[sqlx::test]
async fn a_4xx_is_retried_until_it_is_delivered(pool: PgPool) {
    // One pooled connection, so the retry is the connection's second
    // transaction, which the script lets through.
    let fake = FakeDownstream::start(Script::with(|s| {
        s.transactions = vec![Turn {
            rcpt_to: Some(Act::Reply(451, "4.2.1 try later")),
            ..Turn::default()
        }];
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let s = store(pool);
    let simmer = Simmer::start_spooled(&spooled(fake.addr, dir.path(), "", ""), s.clone()).await;

    assert_eq!(send(&simmer).await.code, 250);
    wait_until("delivery after a retry", Duration::from_secs(5), || async {
        fake.messages().len() == 1
    })
    .await;
    assert_eq!(fake.command_count("MAIL FROM"), 2, "{:?}", fake.commands());
    wait_until("the row to go", Duration::from_secs(5), || async {
        s.totals().await.unwrap().messages == 0
    })
    .await;
}

#[sqlx::test]
async fn a_retry_stays_on_its_first_route(pool: PgPool) {
    // Q3. `warming` refuses the first attempt; then its headroom is taken
    // away. A retry that walked the chain would go out via `only`; a pinned
    // retry waits for `warming` instead.
    let warming = FakeDownstream::start(Script::with(|s| {
        s.transactions = vec![Turn {
            rcpt_to: Some(Act::Reply(451, "4.2.1 try later")),
            ..Turn::default()
        }];
    }))
    .await;
    let only = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let s = store(pool);
    let warming_route = format!(
        "  - name: warming\n    downstream:\n      host: \"127.0.0.1\"\n      port: {}\n      \
         tls: off\n      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}\n      \
         timeouts: {{ connect: 2s, command: 2s, data: 2s }}\n    identity:\n      \
         envelope_from: \"b@newbrand.com\"\n    warmup:\n      started: \"2020-01-01T00:00:00Z\"\n      \
         schedule: {{ default: [100] }}\n",
        warming.addr.port()
    );
    let yaml = spooled(only.addr, dir.path(), &warming_route, "")
        .replace("chain: [only]", "chain: [warming, only]");
    let simmer = Simmer::start_spooled(&yaml, s.clone()).await;

    assert_eq!(send(&simmer).await.code, 250);
    wait_until("the first attempt", Duration::from_secs(5), || async {
        warming.command_count("RCPT TO") >= 1
    })
    .await;
    let day = simmer::quota::day::index(
        chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        chrono::Utc::now(),
    );
    s.set_allowance_override("main", "warming", "catchall", day, Some(0), Some(100))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        only.messages().is_empty(),
        "a pinned retry must not fall through to the next route"
    );
    assert_eq!(s.totals().await.unwrap().messages, 1, "still queued");

    s.set_allowance_override("main", "warming", "catchall", day, None, Some(100))
        .await
        .unwrap();
    wait_until(
        "delivery via the pinned route",
        Duration::from_secs(5),
        || async { warming.messages().len() == 1 },
    )
    .await;
    assert!(only.messages().is_empty());
}

#[sqlx::test]
async fn a_5xx_at_rcpt_is_dead_lettered_and_the_webhook_hears(pool: PgPool) {
    let events: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let hook = {
        let events = Arc::clone(&events);
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
                let events = Arc::clone(&events);
                async move {
                    events.lock().unwrap().push(v);
                    "ok"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        addr
    };
    let fake = FakeDownstream::start(Script::with(|s| {
        s.rcpt_to = Act::Reply(550, "5.1.1 no such user");
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let s = store(pool);
    let yaml = spooled(
        fake.addr,
        dir.path(),
        "",
        &format!("  dead_letter: {{ webhook: {{ url: \"http://{hook}/hook\", timeout: 2s }} }}\n"),
    );
    let simmer = Simmer::start_spooled(&yaml, s.clone()).await;

    assert_eq!(
        send(&simmer).await.code,
        250,
        "accepted: the 5xx comes later"
    );
    wait_until("the dead letter", Duration::from_secs(5), || async {
        !s.dead_entries(10).await.unwrap().is_empty()
    })
    .await;
    let dead = &s.dead_entries(10).await.unwrap()[0];
    assert_eq!(dead.reason, Some(DeadReason::Rejected));
    assert_eq!(dead.last_code, Some(550));
    assert_eq!(dead.route.as_deref(), Some("only"));
    assert!(
        !dead.body_retained,
        "Q6: the body goes at dead-letter by default"
    );
    wait_until("the body to go", Duration::from_secs(2), || async {
        bodies_in(dir.path()) == 0
    })
    .await;

    wait_until("the webhook", Duration::from_secs(5), || async {
        !events.lock().unwrap().is_empty()
    })
    .await;
    let e = events.lock().unwrap()[0].clone();
    assert_eq!(e["reason"], "rejected");
    assert_eq!(e["code"], 550);
    assert_eq!(e["route"], "only");
    assert_eq!(e["rcpt"][0], "bob@example.com");
    assert_eq!(e["id"], dead.id.to_string());
}

#[sqlx::test]
async fn a_message_whose_hold_runs_out_expires(pool: PgPool) {
    let fake = FakeDownstream::start(Script::with(|s| {
        s.rcpt_to = Act::Reply(451, "4.2.1 try later");
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let s = store(pool);
    let yaml = spooled(fake.addr, dir.path(), "", "  max_hold: 1s\n");
    let simmer = Simmer::start_spooled(&yaml, s.clone()).await;

    assert_eq!(send(&simmer).await.code, 250);
    wait_until("expiry", Duration::from_secs(5), || async {
        s.dead_entries(10)
            .await
            .unwrap()
            .first()
            .is_some_and(|d| d.reason == Some(DeadReason::Expired))
    })
    .await;
    assert!(fake.messages().is_empty());
}

#[sqlx::test]
async fn admission_refuses_a_full_spool_and_a_draining_ramp_with_451(pool: PgPool) {
    let fake = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let s = store(pool);
    let yaml = spooled(fake.addr, dir.path(), "", "  max_messages: 1\n");
    let simmer = Simmer::start_spooled(&yaml, s.clone()).await;
    // Paused, so the first message stays and fills the spool.
    s.set_spool_paused("main", true).await.unwrap();

    assert_eq!(send(&simmer).await.code, 250);
    let full = send(&simmer).await;
    assert_eq!(full.code, 451, "{full:?}");
    assert!(full.contains("4.7.1"), "{full:?}");
    assert_eq!(s.totals().await.unwrap().messages, 1, "nothing stored");

    s.set_spool_paused("main", false).await.unwrap();
    wait_until("the first to deliver", Duration::from_secs(5), || async {
        fake.messages().len() == 1
    })
    .await;
    s.set_spool_draining("main", true).await.unwrap();
    let draining = send(&simmer).await;
    assert_eq!(draining.code, 451, "{draining:?}");
    assert!(draining.contains("not accepting"), "{draining:?}");
}

#[sqlx::test]
async fn delivery_is_paced_at_the_rate(pool: PgPool) {
    // D-118: one a second, waiting. The three are accepted at once and leave a
    // second apart; the waiting route is last in its chain, which a spooling
    // ramp permits.
    let fake = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let s = store(pool);
    let yaml = spooled(
        fake.addr,
        dir.path(),
        "    rate: { per_hour: 3600, burst: 1, on_limit: wait }\n",
        "",
    );
    let simmer = Simmer::start_spooled(&yaml, s.clone()).await;

    let started = Instant::now();
    for _ in 0..3 {
        assert_eq!(send(&simmer).await.code, 250);
    }
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "accepted at once"
    );
    wait_until("three deliveries", Duration::from_secs(8), || async {
        fake.messages().len() == 3
    })
    .await;
    let at: Vec<Instant> = fake
        .timed_commands()
        .into_iter()
        .filter(|c| c.line.starts_with("MAIL FROM"))
        .map(|c| c.at)
        .collect();
    assert_eq!(at.len(), 3);
    for pair in at.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(gap >= Duration::from_millis(900), "{gap:?} between sends");
    }
}

#[sqlx::test]
async fn a_crashed_claim_is_taken_over_after_its_lease_and_delivered_once(pool: PgPool) {
    let fake = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let s = store(pool);
    // A slow poll, so the test's claim lands between the dispatcher's ticks.
    let yaml =
        spooled(fake.addr, dir.path(), "", "").replace("poll_interval: 50ms", "poll_interval: 2s");
    let simmer = Simmer::start_spooled(&yaml, s.clone()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(send(&simmer).await.code, 250);

    // Another instance claims it and dies before reaching the dot: its lease
    // simply runs out.
    let crashed = s
        .claim_due(&ClaimRequest {
            owner: "crashed".into(),
            now: chrono::Utc::now(),
            batch: 1,
            lease: chrono::Duration::seconds(1),
        })
        .await
        .unwrap();
    assert_eq!(crashed.len(), 1);
    let claimed_at = Instant::now();

    wait_until(
        "the takeover's delivery",
        Duration::from_secs(5),
        || async { fake.messages().len() == 1 },
    )
    .await;
    assert!(
        claimed_at.elapsed() >= Duration::from_millis(900),
        "not before the lease ended"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(fake.messages().len(), 1, "exactly one delivery");
    // The crashed holder's token fences nothing now.
    assert_eq!(s.totals().await.unwrap().messages, 0);
}

#[sqlx::test]
async fn two_attempts_of_one_message_send_identical_bytes(pool: PgPool) {
    // §6.6 across attempts (D-116): the rewrite runs at received_at with the
    // message's own seed, so Message-ID, Received: and everything else repeat.
    let fake = FakeDownstream::start(Script::with(|s| {
        s.record_all = true;
        s.transactions = vec![Turn {
            final_dot: Some(Act::Reply(451, "4.3.0 later")),
            ..Turn::default()
        }];
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let s = store(pool);
    let identity = "      set_headers:\n        Message-ID: \"<{{uuid}}@example.com>\"\n";
    let simmer =
        Simmer::start_spooled(&spooled(fake.addr, dir.path(), identity, ""), s.clone()).await;

    assert_eq!(send(&simmer).await.code, 250);
    wait_until("two attempts", Duration::from_secs(5), || async {
        fake.messages().len() == 2
    })
    .await;
    let m = fake.messages();
    assert_eq!(m[0].body, m[1].body, "byte-identical, Received: included");
    assert!(String::from_utf8_lossy(&m[0].body).contains("@example.com>"));
}

#[sqlx::test]
async fn the_orphan_sweep_deletes_only_bodies_no_row_names(pool: PgPool) {
    let fake = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let s = store(pool);
    let simmer = Simmer::start_spooled(&spooled(fake.addr, dir.path(), "", ""), s.clone()).await;
    s.set_spool_paused("main", true).await.unwrap();
    assert_eq!(send(&simmer).await.code, 250);

    // An orphan, as a crash between put and insert leaves one, aged past the
    // sweep's threshold; and the live message's body, aged the same.
    let orphan = dir.path().join(format!("{}.eml", uuid::Uuid::new_v4()));
    std::fs::write(&orphan, b"orphan").unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let f = std::fs::File::options()
            .write(true)
            .open(entry.unwrap().path())
            .unwrap();
        f.set_modified(old).unwrap();
    }
    assert_eq!(bodies_in(dir.path()), 2);

    simmer::spool::sweeper::sweep_once(simmer.spool.as_ref().unwrap()).await;
    assert!(!orphan.exists(), "the orphan is swept");
    assert_eq!(bodies_in(dir.path()), 1, "the live body stays");
}
