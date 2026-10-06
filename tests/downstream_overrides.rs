//! D-112 — a route's `downstream_overrides`, through a real SMTP session against
//! two scripted downstreams.
//!
//! The claim is about which MTA a message *meets*, so every assertion is counted
//! from the downstreams' side: what each one accepted and how many connections
//! it saw. And the claim has a boundary worth testing as hard as the feature —
//! an override chooses where a message goes, never which route or identity it
//! goes under, and a failure at the override MTA is a failure (§3.3), not a
//! reason to try the default one.

mod support;

use support::{config_for, Act, FakeDownstream, Script, Simmer};

/// `config_for(default)` with a `google` group (literal `gmail.com`, so no DNS)
/// and the route's `google` mail sent to `over` instead.
fn config_with_override(
    default: std::net::SocketAddr,
    over: std::net::SocketAddr,
    pool: &str,
) -> String {
    config_for(default, "")
        .replace(
            "  - { name: catchall, domains: [\"*\"] }",
            "  - { name: google, domains: [\"gmail.com\"] }\n  - { name: catchall, domains: [\"*\"] }",
        )
        .replace(
            "    identity:\n      envelope_from: \"b@example.com\"",
            &format!(
                "    downstream_overrides:\n      google:\n        host: \"127.0.0.1\"\n        port: {}\n        tls: off\n        pool: {pool}\n        timeouts: {{ connect: 2s, command: 2s, data: 2s }}\n    identity:\n      envelope_from: \"b@example.com\"",
                over.port()
            ),
        )
}

const POOL: &str = "{ max_connections: 3, idle_ttl: 60s, max_messages_per_connection: 10 }";

#[tokio::test]
async fn each_group_meets_its_own_downstream_under_one_identity() {
    let default = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_with_override(default.addr, over.addr, POOL)).await;

    let mut client = simmer.connect().await;
    client.hello().await;
    let body = "From: Sender <a@oldbrand.com>\r\nSubject: hi\r\n\r\nbody\r\n";
    assert_eq!(
        client
            .deliver("a@oldbrand.com", "x@gmail.com", body)
            .await
            .code,
        250
    );
    assert_eq!(
        client
            .deliver("a@oldbrand.com", "y@example.com", body)
            .await
            .code,
        250
    );

    let at_over = over.messages();
    let at_default = default.messages();
    assert_eq!(
        at_over.len(),
        1,
        "the google recipient went to the override"
    );
    assert_eq!(at_over[0].recipients, ["x@gmail.com"]);
    assert_eq!(
        at_default.len(),
        1,
        "the catchall recipient went to the route's own"
    );
    assert_eq!(at_default[0].recipients, ["y@example.com"]);

    // One route, so one identity: the override chose the MTA and nothing else.
    assert_eq!(at_over[0].mail_from, at_default[0].mail_from);
    assert_eq!(at_over[0].mail_from.as_deref(), Some("b@example.com"));
    let from = |b: &[u8]| {
        String::from_utf8_lossy(b)
            .lines()
            .find(|l| l.starts_with("From:"))
            .map(str::to_string)
    };
    assert_eq!(from(&at_over[0].body), from(&at_default[0].body));
}

#[tokio::test]
async fn the_quota_is_spent_against_the_route_and_the_recipients_group() {
    let default = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let quota = std::sync::Arc::new(support::GrantAllQuota::new());
    let simmer = Simmer::start_with_quota(
        &config_with_override(default.addr, over.addr, POOL),
        quota.clone(),
    )
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    let body = "Subject: hi\r\n\r\nbody\r\n";
    assert_eq!(
        client
            .deliver("a@oldbrand.com", "x@gmail.com", body)
            .await
            .code,
        250
    );

    let committed = quota.committed();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].route, "only");
    assert_eq!(committed[0].domain_group, "google");
    assert!(quota.released().is_empty());
}

#[tokio::test]
async fn a_failure_at_the_override_never_falls_through_to_the_default() {
    // §3.3: the override is a choice made after selection, not a fallback, and
    // a failed send is reported, not retried somewhere else.
    let default = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::with(|s| {
        s.rcpt_to = Act::Reply(450, "4.2.0 mailbox busy");
    }))
    .await;
    let quota = std::sync::Arc::new(support::GrantAllQuota::new());
    let simmer = Simmer::start_with_quota(
        &config_with_override(default.addr, over.addr, POOL),
        quota.clone(),
    )
    .await;

    let mut client = simmer.connect().await;
    client.hello().await;
    let r = client
        .deliver(
            "a@oldbrand.com",
            "x@gmail.com",
            "Subject: hi\r\n\r\nbody\r\n",
        )
        .await;
    assert_eq!(
        r.code / 100,
        4,
        "the override's deferral reaches the client: {r:?}"
    );

    assert_eq!(
        default.connections(),
        0,
        "the default MTA never heard of it"
    );
    assert!(default.messages().is_empty());
    assert!(
        quota.committed().is_empty(),
        "nothing committed for a failed send"
    );
    assert_eq!(quota.released().len(), 1, "the reservation was released");
}

#[tokio::test]
async fn each_downstream_has_its_own_pool_and_bound() {
    let default = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_with_override(default.addr, over.addr, POOL)).await;

    // Seeded from configuration before anything is sent: §9.2 must have an
    // answer for an override nothing has used yet.
    let own = simmer
        .pools
        .stats("main", "only")
        .expect("the route's pool");
    let google = simmer
        .pools
        .stats_for("main", "only", Some("google"))
        .expect("the override's pool");
    assert_eq!(own.max_connections, 1, "config_for's own bound");
    assert_eq!(
        google.max_connections, 3,
        "the override's bound, not the route's"
    );
    assert!(
        simmer
            .pools
            .stats_for("main", "only", Some("catchall"))
            .is_none(),
        "a group with no override has no pool of its own: it shares the route's"
    );

    let mut client = simmer.connect().await;
    client.hello().await;
    for i in 0..3 {
        let r = client
            .deliver(
                "a@oldbrand.com",
                "x@gmail.com",
                &format!("Subject: {i}\r\n\r\nb\r\n"),
            )
            .await;
        assert_eq!(r.code, 250, "message {i}: {r:?}");
    }

    let google = simmer
        .pools
        .stats_for("main", "only", Some("google"))
        .expect("the override's pool");
    assert_eq!(
        google.opened, 1,
        "three messages over one pooled connection"
    );
    assert_eq!(google.reused, 2);
    assert_eq!(over.connections(), 1);

    let own = simmer
        .pools
        .stats("main", "only")
        .expect("the route's pool");
    assert_eq!(own.opened, 0, "the route's own pool was never touched");
    assert_eq!(default.connections(), 0);
}
