//! §8.3 — the connection pool, from the client's side of Simmer.
//!
//! Everything here is asserted through a real SMTP session against a real
//! scripted downstream, because the pool's whole claim is about what the
//! *downstream* sees: fewer connections than messages, an `RSET` between them,
//! and a bound on how many exist at once. A unit test over the semaphore can
//! prove the bound; only this tier can prove the conversation.
//!
//! The one thing to keep in mind when reading these: `tests/support`'s fake
//! counts a message at the final dot, so "three messages, one connection" is
//! three deliveries over one accepted socket, which is exactly §8.3's point.

mod support;

use std::time::Duration;

use support::{config_for, Act, FakeDownstream, Script, Simmer, Turn};

/// A route with the given pool block and a short connect budget, so a test that
/// means to exhaust the pool does not sit for ten seconds waiting to find out.
fn config_with_pool(addr: std::net::SocketAddr, pool: &str) -> String {
    config_for(addr, "")
        .replace(
            "pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }",
            pool,
        )
        // A connect budget well under the data budget, so that the test which
        // means to exhaust the pool measures the wait for a permit rather than
        // racing the stalled session that is holding it.
        .replace(
            "timeouts: { connect: 2s, command: 2s, data: 2s }",
            "timeouts: { connect: 1s, command: 3s, data: 3s }",
        )
}

// ---------------------------------------------------------------------------
// Reuse
// ---------------------------------------------------------------------------

#[tokio::test]
async fn three_messages_share_one_connection() {
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    ))
    .await;

    // One client connection, three transactions — but the assertion is about the
    // *downstream* side, and it would hold just as well across three clients.
    let mut client = simmer.connect().await;
    client.hello().await;
    for i in 0..3 {
        let r = client
            .deliver(
                "a@oldbrand.com",
                "b@example.com",
                &format!("Subject: {i}\r\n\r\nbody\r\n"),
            )
            .await;
        assert_eq!(r.code, 250, "message {i}: {r:?}");
    }

    assert_eq!(down.messages().len(), 3, "three messages arrived");
    assert_eq!(
        down.connections(),
        1,
        "and §8.3 says they share a connection; {} were opened",
        down.connections()
    );
}

#[tokio::test]
async fn a_reused_connection_is_reset_between_messages() {
    // §8.3: "RSET between messages on a reused connection." Simmer issues it on
    // the way back to the pool, so what sits idle is never mid-transaction.
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    ))
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    for _ in 0..3 {
        assert_eq!(
            client
                .deliver("a@oldbrand.com", "b@example.com", "Subject: x\r\n\r\nb\r\n")
                .await
                .code,
            250
        );
    }

    assert_eq!(
        down.command_count("RSET"),
        3,
        "one per message returned to the pool: {:?}",
        down.commands()
    );
    assert_eq!(
        down.command_count("EHLO"),
        1,
        "and no re-greeting, which is what makes reuse worth having"
    );
    assert_eq!(
        down.command_count("AUTH"),
        0,
        "no downstream credentials in this fixture, so nothing to re-authenticate"
    );
}

#[tokio::test]
async fn simmer_greets_the_downstream_by_its_own_hostname() {
    // Finding F14 (D-077): §4.1's `server.hostname` is Simmer's EHLO identity,
    // outbound as well as in. The downstream's own name in that slot is the
    // spoof an anti-forgery rule refuses, and a refusal at EHLO is D-023's 451
    // for every message on the route.
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    ))
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    assert_eq!(
        client
            .deliver("a@oldbrand.com", "b@example.com", "Subject: x\r\n\r\nb\r\n")
            .await
            .code,
        250
    );

    let ehlos: Vec<String> = down
        .commands()
        .into_iter()
        .filter(|c| c.to_ascii_uppercase().starts_with("EHLO"))
        .collect();
    assert_eq!(ehlos, ["EHLO simmer.test"], "{:?}", down.commands());
}

#[tokio::test]
async fn max_messages_per_connection_retires_the_connection() {
    // §8.3's third knob. Two messages per connection, four messages, so the
    // second and fourth retire theirs and the downstream sees two connections.
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 2 }",
    ))
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    for i in 0..4 {
        assert_eq!(
            client
                .deliver(
                    "a@oldbrand.com",
                    "b@example.com",
                    &format!("Subject: {i}\r\n\r\nbody\r\n")
                )
                .await
                .code,
            250,
            "message {i}"
        );
    }

    assert_eq!(down.messages().len(), 4);
    assert_eq!(
        down.connections(),
        2,
        "four messages at two per connection is two connections, not one and not four"
    );
    assert_eq!(
        down.command_count("QUIT"),
        2,
        "a retired connection says goodbye rather than vanishing: {:?}",
        down.commands()
    );
}

#[tokio::test]
async fn an_idle_connection_past_its_ttl_is_not_reused() {
    // idle_ttl of 1s, and a second between messages.
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 1s, max_messages_per_connection: 100 }",
    ))
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    assert_eq!(
        client
            .deliver("a@oldbrand.com", "b@example.com", "Subject: 1\r\n\r\nb\r\n")
            .await
            .code,
        250
    );

    tokio::time::sleep(Duration::from_millis(1200)).await;

    assert_eq!(
        client
            .deliver("a@oldbrand.com", "b@example.com", "Subject: 2\r\n\r\nb\r\n")
            .await
            .code,
        250
    );
    assert_eq!(
        down.connections(),
        2,
        "an entry older than idle_ttl is dropped rather than validated"
    );
}

// ---------------------------------------------------------------------------
// The stale connection, which is the reason the retry exists
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_connection_the_downstream_closed_costs_a_reconnect_and_not_the_message() {
    // The fake answers the `RSET` and then closes, so the connection goes back
    // into the pool looking healthy and is dead when the next message takes it
    // out — inside VALIDATE_AFTER, so the `NOOP` never runs. Without the retry
    // in `client::relay` this message is a 451 for a recipient that is perfectly
    // deliverable, which is §14.1's whole concern arriving by a side door.
    let down = FakeDownstream::start(Script::with(|s| s.close_after_rset = true)).await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    ))
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    for i in 0..3 {
        let r = client
            .deliver(
                "a@oldbrand.com",
                "b@example.com",
                &format!("Subject: {i}\r\n\r\nbody\r\n"),
            )
            .await;
        assert_eq!(
            r.code, 250,
            "message {i} must survive a dead pooled connection: {r:?}"
        );
    }

    assert_eq!(down.messages().len(), 3, "and all three actually arrived");
    assert_eq!(
        down.connections(),
        3,
        "each one paying exactly one reconnect, never two"
    );
}

#[tokio::test]
async fn the_retry_does_not_resend_past_the_final_dot() {
    // §10.2's window. The fake accepts the whole message and then drops without
    // replying to the terminating dot, which is indistinguishable from a delivery
    // that succeeded. Retrying there is how one message becomes two, so the
    // client is answered 451 and told the fate is unknown — and the downstream
    // must have seen the body exactly once.
    let down = FakeDownstream::start(Script::with(|s| s.final_dot = Act::Drop)).await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    ))
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    let r = client
        .deliver("a@oldbrand.com", "b@example.com", "Subject: x\r\n\r\nb\r\n")
        .await;

    assert_eq!(r.code, 451, "{r:?}");
    assert!(
        r.contains("delivery unknown"),
        "§10.2's wording, not a retry: {r:?}"
    );
    assert_eq!(
        down.connections(),
        1,
        "one connection, one attempt: the ambiguous window is never retried"
    );
    assert_eq!(
        down.command_count("DATA"),
        1,
        "and the body was offered exactly once: {:?}",
        down.commands()
    );
}

/// Message 2 starts here: the arrival time of the second `MAIL FROM` the
/// downstream saw, on any connection.
fn second_mail_from(down: &FakeDownstream) -> std::time::Instant {
    down.timed_commands()
        .iter()
        .filter(|c| c.line.to_ascii_uppercase().starts_with("MAIL FROM"))
        .nth(1)
        .expect("a second MAIL FROM")
        .at
}

/// How many times `verb` reached the downstream from `since` on, any connection.
fn count_since(down: &FakeDownstream, verb: &str, since: std::time::Instant) -> usize {
    down.timed_commands()
        .iter()
        .filter(|c| c.at >= since && c.line.to_ascii_uppercase().starts_with(verb))
        .count()
}

#[tokio::test]
async fn a_drop_after_the_final_dot_on_a_reused_connection_is_not_retried() {
    // The test above runs on a FRESH connection, where no retry is considered
    // at all, so it cannot see D-068's third condition. This one reuses: message
    // 1 succeeds, and message 2 — the second transaction on the same socket —
    // loses its connection after the terminating dot. A fresh connection would
    // accept it, so a retry would "succeed" and deliver it twice.
    let down = FakeDownstream::start(Script::with(|s| {
        s.transactions = vec![
            Turn::default(),
            Turn {
                final_dot: Some(Act::Drop),
                ..Turn::default()
            },
        ];
    }))
    .await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    ))
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    let r = client
        .deliver("a@oldbrand.com", "b@example.com", "Subject: 1\r\n\r\nb\r\n")
        .await;
    assert_eq!(r.code, 250, "message 1: {r:?}");

    let r = client
        .deliver("a@oldbrand.com", "b@example.com", "Subject: 2\r\n\r\nb\r\n")
        .await;
    assert_eq!(r.code, 451, "{r:?}");
    assert!(r.contains("delivery unknown"), "§10.2's wording: {r:?}");

    let since = second_mail_from(&down);
    assert_eq!(
        count_since(&down, "DATA", since),
        1,
        "message 2's body was offered exactly once: {:?}",
        down.commands()
    );
    assert_eq!(
        down.connections(),
        1,
        "and no fresh connection was opened for it"
    );
}

#[tokio::test]
async fn a_body_write_failure_on_a_reused_connection_is_not_retried() {
    // The case D-068's `!= FinalDot` condition actually decides. A drop after the
    // dot is read as §10.2's ambiguity before the retry is ever considered; a
    // connection that dies while the BODY is being written surfaces as a
    // protocol error at `Stage::FinalDot`, which is otherwise exactly the shape
    // the retry is for. The downstream may have read and kept a prefix — or, for
    // all Simmer can tell, the whole message — so it must not be sent again.
    //
    // The body is far larger than the socket buffers, so the write is still in
    // progress when the downstream reads 16 bytes and closes: the write, not the
    // reply read, is what fails.
    let down = FakeDownstream::start(Script::with(|s| {
        s.transactions = vec![
            Turn::default(),
            Turn {
                drop_mid_data: Some(true),
                ..Turn::default()
            },
        ];
    }))
    .await;
    let cfg = config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    )
    .replace("max_message_bytes: 100000", "max_message_bytes: 33554432");
    let simmer = Simmer::start(&cfg).await;

    let mut client = simmer.connect().await;
    client.hello().await;
    let r = client
        .deliver("a@oldbrand.com", "b@example.com", "Subject: 1\r\n\r\nb\r\n")
        .await;
    assert_eq!(r.code, 250, "message 1: {r:?}");

    let line = format!("{}\r\n", "x".repeat(70));
    let big = format!("Subject: 2\r\n\r\n{}", line.repeat(16 * 1024 * 1024 / 72));
    let r = client
        .deliver("a@oldbrand.com", "b@example.com", &big)
        .await;
    assert_eq!(r.code, 451, "never a 250 from a second attempt: {r:?}");

    let since = second_mail_from(&down);
    assert_eq!(
        count_since(&down, "DATA", since),
        1,
        "message 2's body was offered exactly once: {:?}",
        down.commands()
            .iter()
            .filter(|c| c.len() < 80)
            .collect::<Vec<_>>()
    );
    assert_eq!(down.connections(), 1, "no retry connection was opened");
    assert_eq!(
        down.messages().len(),
        1,
        "only message 1 was ever delivered"
    );
}

#[tokio::test]
async fn a_timeout_on_a_reused_connection_is_not_retried() {
    // D-068's second condition, on the connection where the retry is actually
    // considered. A downstream slow enough to blow a stage budget is slow, not
    // stale; retrying spends the budget twice and, on a fresh connection that
    // answers promptly, would turn the slowness into a delivery the client was
    // never going to get in time.
    let down = FakeDownstream::start(Script::with(|s| {
        s.transactions = vec![
            Turn::default(),
            Turn {
                rcpt_to: Some(Act::Stall),
                ..Turn::default()
            },
        ];
    }))
    .await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    ))
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    let r = client
        .deliver("a@oldbrand.com", "b@example.com", "Subject: 1\r\n\r\nb\r\n")
        .await;
    assert_eq!(r.code, 250, "message 1: {r:?}");

    client.send("MAIL FROM:<a@oldbrand.com>").await;
    client.send("RCPT TO:<b@example.com>").await;
    client.send("DATA").await;
    assert_eq!(client.read_reply().await.code, 250);
    assert_eq!(client.read_reply().await.code, 250);
    assert_eq!(client.read_reply().await.code, 354);
    client.send_raw(b"Subject: 2\r\n\r\nb\r\n.\r\n").await;
    let r = client.read_reply().await;
    assert_eq!(r.code, 451, "{r:?}");
    assert!(r.contains("4.4.2"), "a timeout, reported as one: {r:?}");

    let since = second_mail_from(&down);
    assert_eq!(
        count_since(&down, "MAIL FROM", since),
        1,
        "message 2 was attempted once: {:?}",
        down.commands()
    );
    assert_eq!(down.connections(), 1, "no retry connection was opened");
    assert_eq!(down.messages().len(), 1);
}

#[tokio::test]
async fn a_rejection_keeps_the_connection_and_is_not_retried() {
    // A `550` at RCPT TO is the downstream's considered answer over a connection
    // that is still perfectly well. Retrying it would ask the same question twice
    // and get the same answer; discarding the connection would throw away a
    // working socket on every rejected recipient.
    let down = FakeDownstream::start(Script::with(|s| {
        s.rcpt_to = Act::Reply(550, "5.1.1 no such user")
    }))
    .await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    ))
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    for _ in 0..2 {
        let r = client
            .deliver("a@oldbrand.com", "b@example.com", "Subject: x\r\n\r\nb\r\n")
            .await;
        // D-008: a 5xx at RCPT TO is the one place §14.1 permits a 550 through.
        assert_eq!(r.code, 550, "{r:?}");
    }

    assert_eq!(
        down.connections(),
        1,
        "the connection survived a rejection and served the second attempt too"
    );
    assert_eq!(
        down.command_count("RSET"),
        2,
        "with an RSET clearing the half-finished transaction each time: {:?}",
        down.commands()
    );
}

#[tokio::test]
async fn a_broken_connection_does_not_go_back_in_the_pool() {
    // "Discarded on any protocol error rather than returned to the pool." The
    // fake drops mid-DATA, so the connection is in a state nobody can describe;
    // the next message must get a new one rather than inherit it.
    let down = FakeDownstream::start(Script::with(|s| s.drop_mid_data = true)).await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    ))
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    for _ in 0..2 {
        let r = client
            .deliver("a@oldbrand.com", "b@example.com", "Subject: x\r\n\r\nb\r\n")
            .await;
        assert_eq!(r.code, 451, "{r:?}");
    }

    assert_eq!(
        down.connections(),
        2,
        "each attempt opened its own connection; a broken one is never pooled"
    );
    assert_eq!(
        down.command_count("RSET"),
        0,
        "and no RSET was attempted on a connection we could not describe"
    );
}

// ---------------------------------------------------------------------------
// §9.2's statistics, and §10.4's drain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_statistics_track_what_the_downstream_saw() {
    // §9.2 asks for pool statistics, and they are only worth reporting if they
    // are the same numbers the downstream would recognise.
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    ))
    .await;

    assert_eq!(
        simmer.pools.stats("main", "only").expect("a pool").opened,
        0,
        "nothing is dialled before a message needs it"
    );

    let mut client = simmer.connect().await;
    client.hello().await;
    for _ in 0..3 {
        assert_eq!(
            client
                .deliver("a@oldbrand.com", "b@example.com", "Subject: x\r\n\r\nb\r\n")
                .await
                .code,
            250
        );
    }

    let stats = simmer.pools.stats("main", "only").expect("a pool");
    assert_eq!(stats.max_connections, 4);
    assert_eq!(stats.opened, 1, "one connection opened");
    assert_eq!(stats.reused, 2, "and taken back out twice");
    assert_eq!(stats.idle, 1, "waiting for a fourth message");
    assert_eq!(stats.active, 0, "with nothing checked out");
    assert_eq!(stats.discarded, 0);
    assert_eq!(stats.retired, 0);
    assert_eq!(
        stats.opened as usize,
        down.connections(),
        "the statistics and the downstream must agree about how many \
         connections exist, or §9.2 is reporting a fiction"
    );
}

#[tokio::test]
async fn draining_closes_the_idle_connections() {
    // §10.4's fourth clause. In the service this runs after in-flight sessions
    // have finished, so everything left is idle — which is what this drives.
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
    ))
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    assert_eq!(
        client
            .deliver("a@oldbrand.com", "b@example.com", "Subject: x\r\n\r\nb\r\n")
            .await
            .code,
        250
    );
    assert_eq!(simmer.pools.stats("main", "only").expect("a pool").idle, 1);
    assert_eq!(down.command_count("QUIT"), 0);

    simmer.pools.drain().await;

    assert_eq!(
        down.command_count("QUIT"),
        1,
        "an orderly goodbye rather than a reset the provider counts against us: {:?}",
        down.commands()
    );
    let stats = simmer.pools.stats("main", "only").expect("a pool");
    assert_eq!(stats.idle, 0);
    assert_eq!(stats.active, 0);
}

// ---------------------------------------------------------------------------
// The bound, which is the clause a socket cache would not satisfy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_saturated_pool_defers_rather_than_opening_another_connection() {
    // max_connections: 1 against a downstream that never answers the final dot.
    // The second session cannot have a connection and must not be given one: the
    // bound is the point of §8.3's last sentence. What it gets is a 451 of its
    // own class — never a 5xx, because a saturated pool is Simmer's problem and
    // §14.1 forbids charging it to the recipient.
    let down = FakeDownstream::start(Script::with(|s| s.final_dot = Act::Stall)).await;
    let simmer = Simmer::start(&config_with_pool(
        down.addr,
        "pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 100 }",
    ))
    .await;

    let mut first = simmer.connect().await;
    first.hello().await;
    let stalled = tokio::spawn(async move {
        first
            .deliver("a@oldbrand.com", "b@example.com", "Subject: 1\r\n\r\nb\r\n")
            .await
    });

    // Let the first session get as far as holding the only permit.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut second = simmer.connect().await;
    second.hello().await;
    let r = second
        .deliver("a@oldbrand.com", "b@example.com", "Subject: 2\r\n\r\nb\r\n")
        .await;

    assert_eq!(r.code, 451, "{r:?}");
    assert!(
        r.contains("pool exhausted"),
        "an exhausted pool must be distinguishable from a downstream that is \
         refusing connections — they call for opposite responses: {r:?}"
    );
    assert_eq!(
        down.connections(),
        1,
        "and above all it must not have opened a second connection"
    );

    let _ = stalled.await;
}
