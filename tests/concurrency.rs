//! Bounds under real parallelism — a multi-threaded runtime, which no other
//! in-process suite uses.
//!
//! Every other `#[tokio::test]` in the repository runs on the current-thread
//! runtime, where "concurrent" sessions interleave on one thread and a race can
//! only happen at an `.await`. That is enough to exercise the logic and not
//! enough to trust a bound: §5.1's session cap and §8.3's pool are claims about
//! what happens when things *really* run at once. Each test here says so in its
//! attribute.
//!
//! What they measure is always the far side's view — replies a client got, or
//! connections a downstream accepted — never the implementation's own counters.

mod support;

use std::time::Duration;

use support::{config_for, FakeDownstream, GrantAllQuota, Script, Simmer};

const BODY: &str = "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n";

/// Poll until `done` holds, or panic after `within`.
async fn eventually(within: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + within;
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixteen_sessions_never_hold_more_than_four_pool_connections() {
    // The STATE.md gap: the saturation test in tests/pool.rs uses one permit and
    // two sessions. Here sixteen sessions contend for four permits on four worker
    // threads, and the downstream holds each connection 200 ms, so the permits
    // are genuinely fought over. Sixteen over four at 200 ms is 0.8 s of queueing
    // at worst, inside the 2 s connect budget, so every message must get through.
    let down = FakeDownstream::start(Script::with(|s| {
        s.final_dot_delay = Some(Duration::from_millis(200));
    }))
    .await;
    let cfg = config_for(down.addr, "").replace(
        "pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }",
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    );
    let simmer = Simmer::start(&cfg).await;

    let sessions: Vec<_> = (0..16)
        .map(|i| {
            let addr = simmer.addr;
            tokio::spawn(async move {
                let mut c = support::Client::connect(addr).await;
                c.hello().await;
                c.deliver("jane@oldbrand.com", &format!("r{i}@example.net"), BODY)
                    .await
            })
        })
        .collect();
    for (i, s) in sessions.into_iter().enumerate() {
        let r = s.await.expect("session task");
        assert_eq!(r.code, 250, "session {i}: {r:?}");
    }

    assert_eq!(down.messages().len(), 16);
    assert!(
        down.peak_connections() <= 4,
        "the downstream saw {} connections at once; max_connections is 4",
        down.peak_connections()
    );
    // And the bound was actually reached — otherwise this proves nothing about
    // what happens at it.
    assert_eq!(
        down.peak_connections(),
        4,
        "the pool never ran four connections at once, so the bound was not exercised"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_session_cap_is_shared_by_every_listener_under_a_burst() {
    // §5.1: `max_concurrent_sessions` is a property of the process. Forty clients
    // arrive at once, split across two listeners, against a cap of eight: exactly
    // eight get a banner and thirty-two are refused, whichever port they chose.
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "")
        .replace(
            "  listeners:\n    - address: \"127.0.0.1:0\"\n",
            "  listeners:\n    - address: \"127.0.0.1:0\"\n    - address: \"127.0.0.1:0\"\n",
        )
        .replace("max_concurrent_sessions: 16", "max_concurrent_sessions: 8");
    let simmer = Simmer::start(&cfg).await;
    assert_eq!(simmer.addrs.len(), 2);

    let clients: Vec<_> = (0..40)
        .map(|i| {
            let addr = simmer.addrs[i % 2];
            tokio::spawn(async move {
                let mut c = support::Client::connect(addr).await;
                let code = c.read_reply().await.code;
                // Held until every client has its answer, so no admitted session
                // gives its permit back mid-burst.
                (code, c)
            })
        })
        .collect();

    let mut held = Vec::new();
    let (mut admitted, mut refused) = (0, 0);
    for c in clients {
        let (code, client) = c.await.expect("client task");
        match code {
            220 => admitted += 1,
            421 => refused += 1,
            other => panic!("unexpected greeting {other}"),
        }
        held.push(client);
    }
    assert_eq!(
        (admitted, refused),
        (8, 32),
        "the cap is shared: 8 banners, 32 refusals"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_reservation_is_resolved_when_clients_walk_away() {
    // §7.4's obligation — every reservation is committed or released exactly
    // once — under clients that do not behave. Eight finish normally, eight hang
    // up mid-DATA (before any reservation exists), and eight send the final dot
    // and disconnect without waiting for the verdict. The session outlives its
    // client, so those relays must still finish and commit.
    let down = FakeDownstream::start(Script::with(|s| {
        s.final_dot_delay = Some(Duration::from_millis(100));
    }))
    .await;
    // Room for all 24 clients at once: this test is about reservations, and a
    // session refused at the banner never takes one.
    let cfg = config_for(down.addr, "")
        .replace(
            "pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }",
            "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
        )
        .replace("max_concurrent_sessions: 16", "max_concurrent_sessions: 32");
    let quota = std::sync::Arc::new(GrantAllQuota::new());
    let simmer = Simmer::start_with_quota(&cfg, quota.clone()).await;

    let clients: Vec<_> = (0..24)
        .map(|i| {
            let addr = simmer.addr;
            tokio::spawn(async move {
                let mut c = support::Client::connect(addr).await;
                c.hello().await;
                let to = format!("r{i}@example.net");
                match i % 3 {
                    0 => {
                        let r = c.deliver("jane@oldbrand.com", &to, BODY).await;
                        assert_eq!(r.code, 250, "{r:?}");
                    }
                    1 => {
                        assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
                        assert_eq!(c.command(&format!("RCPT TO:<{to}>")).await.code, 250);
                        assert_eq!(c.command("DATA").await.code, 354);
                        c.send_raw(b"From: jane@oldbrand.com\r\nSubject: half\r\n")
                            .await;
                        // Dropped here, mid-DATA.
                    }
                    _ => {
                        assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
                        assert_eq!(c.command(&format!("RCPT TO:<{to}>")).await.code, 250);
                        assert_eq!(c.command("DATA").await.code, 354);
                        c.send_raw(format!("{BODY}.\r\n").as_bytes()).await;
                        // Dropped here, before the verdict.
                    }
                }
            })
        })
        .collect();
    for c in clients {
        c.await.expect("client task");
    }

    // Sixteen messages reached the final dot; the eight abandoned mid-DATA never
    // took a reservation.
    eventually(Duration::from_secs(10), "sixteen deliveries", || {
        down.messages().len() == 16
    })
    .await;
    eventually(Duration::from_secs(10), "an empty registry", || {
        simmer.registry.is_empty()
    })
    .await;
    eventually(Duration::from_secs(10), "sixteen commits", || {
        quota.committed().len() == 16
    })
    .await;
    assert!(
        quota.released().is_empty(),
        "nothing failed, so nothing should have been released: {:?}",
        quota.released()
    );
}
