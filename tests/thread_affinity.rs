//! §3.2 step 2a — thread affinity (D-090), through the whole stack.
//!
//! Every test sends a first message, reads the `Message-ID:` Simmer actually
//! emitted off the fake downstream, and builds the application's reply the way
//! RFC 5322 §3.6.4 says to: `In-Reply-To:` the recipient's message, and
//! `References:` carrying the chain — which is where Simmer's own ID sits.
//!
//! The tests on [`GrantAllQuota`] run in both builds. The two that need a real
//! ramp to spend are Postgres-backed, at the bottom.

mod support;

use std::sync::Arc;

use support::{Act, FakeDownstream, GrantAllQuota, Received, Script, Simmer};

/// A warming route and an overflow, each with its own downstream and its own
/// `Message-ID:` domain. `extra_warming` is spliced into the warming route and
/// `top` onto the end of the document.
fn config(
    warming_port: u16,
    overflow_port: u16,
    cap: i64,
    extra_warming: &str,
    top: &str,
) -> String {
    format!(
        r#"
server:
  listeners:
    - address: "127.0.0.1:0"
  hostname: "simmer.test"
  max_message_bytes: 100000
  max_recipients: 1
  max_concurrent_sessions: 16
  allowed_cidrs: ["127.0.0.0/8"]
  timeouts: {{ command: 5s, data: 5s, session: 60s }}
  auth: {{ allow_insecure_auth: true }}
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
    identity:
      envelope_from: "b@newbrand.com"
      set_headers:
        Message-ID: "<{{{{uuid}}}}@newbrand.com>"
    warmup:
      started: "2020-01-01T00:00:00Z"
      schedule: {{ default: [{cap}] }}
{extra_warming}
  - name: overflow
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: {overflow_port}
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
      timeouts: {{ connect: 2s, command: 2s, data: 2s }}
    identity:
      envelope_from: "b@mail.established.com"
      set_headers:
        Message-ID: "<{{{{uuid}}}}@mail.established.com>"
{top}
"#
    )
}

const ON: &str = "thread_affinity: true";

/// The first message of a conversation.
fn first(n: u32) -> String {
    format!(
        "From: jane@oldbrand.com\r\nTo: bob@example.com\r\nSubject: Your quote\r\n\
         Message-ID: <app-{n}@oldbrand.com>\r\n\r\nhello\r\n"
    )
}

/// The application's reply to the recipient's reply to `emitted` — the
/// headers the README's table lists, built from the recipient's message.
fn reply_to(emitted: &str) -> String {
    format!(
        "From: jane@oldbrand.com\r\nTo: bob@example.com\r\nSubject: Re: Your quote\r\n\
         Message-ID: <app-reply@oldbrand.com>\r\n\
         In-Reply-To: <CAx9reply@mail.gmail.com>\r\n\
         References: {emitted}\r\n <CAx9reply@mail.gmail.com>\r\n\
         \r\nthanks\r\n"
    )
}

/// The `Message-ID:` a downstream received — the one Simmer emitted.
fn emitted_id(received: &Received) -> String {
    let text = String::from_utf8_lossy(&received.body);
    text.lines()
        .find_map(|l| l.strip_prefix("Message-ID: "))
        .unwrap_or_else(|| panic!("no Message-ID in {text}"))
        .trim()
        .to_string()
}

async fn send(simmer: &Simmer, body: &str) -> support::Reply {
    let mut c = simmer.connect().await;
    c.hello().await;
    c.deliver("jane@oldbrand.com", "bob@example.com", body)
        .await
}

#[tokio::test]
async fn a_reply_to_overflow_mail_stays_on_overflow_though_warming_has_room() {
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let quota = Arc::new(GrantAllQuota::new());
    quota.pause("warming");
    let simmer = Simmer::start_with_quota(
        &config(warm.addr.port(), over.addr.port(), 10, "", ON),
        quota.clone(),
    )
    .await;

    // Warming paused: the first message leaves via overflow.
    assert_eq!(send(&simmer, &first(1)).await.code, 250);
    let id = emitted_id(&over.last().expect("overflow carried it"));
    assert!(id.ends_with("@mail.established.com>"), "{id}");

    // Unpaused, warming would win an ordinary walk. A new message proves it.
    let quota = Arc::new(GrantAllQuota::new());
    let simmer = Simmer::start_with_quota(
        &config(warm.addr.port(), over.addr.port(), 10, "", ON),
        quota,
    )
    .await;
    assert_eq!(send(&simmer, &first(2)).await.code, 250);
    assert_eq!(warm.messages().len(), 1);

    // The reply goes where the conversation started.
    assert_eq!(send(&simmer, &reply_to(&id)).await.code, 250);
    assert_eq!(warm.messages().len(), 1, "not warming");
    assert_eq!(over.messages().len(), 2, "overflow carried the reply");
}

#[tokio::test]
async fn without_thread_affinity_references_are_ignored() {
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config(warm.addr.port(), over.addr.port(), 10, "", "")).await;

    let r = send(&simmer, &reply_to("<3f2a@mail.established.com>")).await;
    assert_eq!(r.code, 250);
    assert_eq!(warm.messages().len(), 1, "the ordinary walk");
    assert_eq!(over.messages().len(), 0);
}

#[tokio::test]
async fn a_paused_pinned_route_falls_back_to_the_ordinary_walk() {
    // An operator's pause still means stop, pinned or not.
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let quota = Arc::new(GrantAllQuota::new());
    let yaml = config(warm.addr.port(), over.addr.port(), 10, "", ON);

    let simmer = Simmer::start_with_quota(&yaml, quota.clone()).await;
    assert_eq!(send(&simmer, &first(1)).await.code, 250);
    let id = emitted_id(&warm.last().expect("warming carried it"));

    quota.pause("warming");
    assert_eq!(send(&simmer, &reply_to(&id)).await.code, 250);
    assert_eq!(warm.messages().len(), 1, "the pause held");
    assert_eq!(over.messages().len(), 1, "and the walk moved on");
}

#[tokio::test]
async fn a_pinned_reply_is_not_steered_by_recipient_frequency_but_is_recorded() {
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let quota = Arc::new(GrantAllQuota::new());
    let frequency = "    recipient_frequency:\n      \
                     mode: to_address\n      \
                     window: { unit: daily, count: 1 }\n      \
                     threshold: 1";
    let simmer = Simmer::start_with_quota(
        &config(warm.addr.port(), over.addr.port(), 10, frequency, ON),
        quota.clone(),
    )
    .await;

    assert_eq!(send(&simmer, &first(1)).await.code, 250);
    let id = emitted_id(&warm.last().expect("warming carried it"));

    // Bob is now over warming's threshold.
    quota.set_recipient_count("warming", 5);

    // A new message to him is steered away, as §7.3 says...
    assert_eq!(send(&simmer, &first(2)).await.code, 250);
    assert_eq!(over.messages().len(), 1, "steered to overflow");

    // ...but his conversation is not.
    let recorded_before = quota.recorded().len();
    assert_eq!(send(&simmer, &reply_to(&id)).await.code, 250);
    assert_eq!(warm.messages().len(), 2, "the reply stayed on warming");
    assert_eq!(
        quota.recorded().len(),
        recorded_before + 1,
        "and still recorded its event, so the window stays true"
    );
    assert_eq!(quota.recorded().last().unwrap().0, "warming");
}

#[tokio::test]
async fn a_pinned_route_that_fails_downstream_does_not_fail_over() {
    // §3.3 is unchanged by a pin.
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let yaml = config(warm.addr.port(), over.addr.port(), 10, "", ON);
    let simmer = Simmer::start(&yaml).await;
    assert_eq!(send(&simmer, &first(1)).await.code, 250);
    let id = emitted_id(&warm.last().expect("warming carried it"));
    drop(simmer);

    let failing = FakeDownstream::start(Script::with(|s| {
        s.final_dot = Act::Reply(451, "4.3.0 try later");
    }))
    .await;
    let quota = Arc::new(GrantAllQuota::new());
    let simmer = Simmer::start_with_quota(
        &config(failing.addr.port(), over.addr.port(), 10, "", ON),
        quota.clone(),
    )
    .await;

    let r = send(&simmer, &reply_to(&id)).await;
    assert_eq!(r.code, 451, "{r:?}");
    assert_eq!(over.messages().len(), 0, "no failover");
    assert_eq!(quota.released().len(), 1, "the reservation was released");
}

#[tokio::test]
async fn the_threading_headers_leave_with_their_original_bytes() {
    let warm = FakeDownstream::start(Script::default()).await;
    let over = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config(warm.addr.port(), over.addr.port(), 10, "", ON)).await;

    let body = reply_to("<3f2a@newbrand.com>");
    assert_eq!(send(&simmer, &body).await.code, 250);
    let out = String::from_utf8_lossy(&warm.last().expect("warming").body).into_owned();
    assert!(
        out.contains("In-Reply-To: <CAx9reply@mail.gmail.com>\r\n"),
        "{out}"
    );
    assert!(
        out.contains("References: <3f2a@newbrand.com>\r\n <CAx9reply@mail.gmail.com>\r\n"),
        "folded exactly as sent: {out}"
    );
}

// ---------------------------------------------------------------------------
// Against a real ramp
// ---------------------------------------------------------------------------

#[cfg(feature = "postgres")]
mod over_the_cap {
    use super::*;
    use simmer::quota::store::QuotaStore;
    use simmer::quota::PgQuotaStore;
    use sqlx::PgPool;

    fn today() -> i64 {
        let started = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        simmer::quota::day::index(started, chrono::Utc::now())
    }

    #[sqlx::test]
    async fn a_pinned_reply_takes_its_route_past_the_cap_and_is_counted(pool: PgPool) {
        let warm = FakeDownstream::start(Script::default()).await;
        let over = FakeDownstream::start(Script::default()).await;
        let store: Arc<dyn QuotaStore> = Arc::new(PgQuotaStore::new(pool));
        let simmer = Simmer::start_with_quota(
            &config(warm.addr.port(), over.addr.port(), 1, "", ON),
            Arc::clone(&store),
        )
        .await;

        // The day's one warming slot.
        assert_eq!(send(&simmer, &first(1)).await.code, 250);
        let id = emitted_id(&warm.last().expect("warming carried it"));

        // Spent: a new conversation spills over.
        assert_eq!(send(&simmer, &first(2)).await.code, 250);
        assert_eq!(over.messages().len(), 1);

        // The reply does not.
        assert_eq!(send(&simmer, &reply_to(&id)).await.code, 250);
        assert_eq!(warm.messages().len(), 2, "past the cap, on its own route");

        let usage = store.usage("warming", "catchall", today()).await.unwrap();
        assert_eq!(usage.allowance, Some(1), "the ceiling is unchanged");
        assert_eq!(usage.committed, 2, "and the reply is counted against it");
        assert_eq!(usage.reserved, 0);

        // And the ramp still holds for everything that is not a reply.
        assert_eq!(send(&simmer, &first(3)).await.code, 250);
        assert_eq!(over.messages().len(), 2);
    }

    fn store_for(pool: PgPool) -> Arc<dyn QuotaStore> {
        Arc::new(PgQuotaStore::new(pool))
    }

    async fn warming_usage(store: &Arc<dyn QuotaStore>) -> (Option<i64>, i64, i64) {
        let u = store.usage("warming", "catchall", today()).await.unwrap();
        (u.allowance, u.committed, u.reserved)
    }

    #[sqlx::test]
    async fn a_pinned_reply_within_the_cap_takes_an_ordinary_slot(pool: PgPool) {
        // The cap is 2 and one slot is left: the reply uses it, like any message.
        let warm = FakeDownstream::start(Script::default()).await;
        let over = FakeDownstream::start(Script::default()).await;
        let store = store_for(pool);
        let simmer = Simmer::start_with_quota(
            &config(warm.addr.port(), over.addr.port(), 2, "", ON),
            Arc::clone(&store),
        )
        .await;

        assert_eq!(send(&simmer, &first(1)).await.code, 250);
        let id = emitted_id(&warm.last().expect("warming carried it"));
        assert_eq!(send(&simmer, &reply_to(&id)).await.code, 250);
        assert_eq!(warm.messages().len(), 2);
        assert_eq!(
            warming_usage(&store).await,
            (Some(2), 2, 0),
            "the cap, exactly met"
        );

        // It counted toward the cap: the next new conversation has no room.
        assert_eq!(send(&simmer, &first(2)).await.code, 250);
        assert_eq!(
            warm.messages().len(),
            2,
            "the reply spent the day's last slot"
        );
        assert_eq!(over.messages().len(), 1);
        assert_eq!(warming_usage(&store).await, (Some(2), 2, 0));
    }

    #[sqlx::test]
    async fn replies_go_past_the_cap_only_once_it_has_been_met_and_every_one_counts(pool: PgPool) {
        let warm = FakeDownstream::start(Script::default()).await;
        let over = FakeDownstream::start(Script::default()).await;
        let store = store_for(pool);
        let simmer = Simmer::start_with_quota(
            &config(warm.addr.port(), over.addr.port(), 3, "", ON),
            Arc::clone(&store),
        )
        .await;

        assert_eq!(send(&simmer, &first(1)).await.code, 250);
        let id = emitted_id(&warm.last().expect("warming carried it"));

        // Committed: 1 of 3. Five replies: two fill the cap, three go past it.
        for n in 0..5 {
            assert_eq!(send(&simmer, &reply_to(&id)).await.code, 250, "reply {n}");
            let (allowance, committed, reserved) = warming_usage(&store).await;
            assert_eq!(allowance, Some(3), "the ceiling never moves");
            assert_eq!(committed, 2 + n, "reply {n} was counted");
            assert_eq!(reserved, 0);
        }
        assert_eq!(warm.messages().len(), 6);
        assert_eq!(over.messages().len(), 0);

        // The ramp is still closed to everything that is not a reply.
        assert_eq!(send(&simmer, &first(2)).await.code, 250);
        assert_eq!(over.messages().len(), 1);
        assert_eq!(
            warming_usage(&store).await.1,
            6,
            "and that spilled one is not counted here"
        );
    }

    #[sqlx::test]
    async fn the_walk_marks_a_reply_over_cap_only_when_there_was_no_headroom(pool: PgPool) {
        let yaml = config(1, 2, 2, "", ON);
        let cfg = simmer::config::from_str(&yaml, "test").expect("valid");
        let store = store_for(pool);
        let frequency = simmer::frequency::Frequency::new();
        let preflight = simmer::preflight::Registry::new();
        let chain = ["warming".to_string(), "overflow".to_string()];
        let recipients = ["bob@example.com".to_string()];

        let mut marks = Vec::new();
        for n in 0..4 {
            let mut ev = Vec::new();
            let walk = simmer::routing::chain::walk_and_reserve(
                &cfg,
                &store,
                &frequency,
                &preflight,
                &chain,
                Some("warming"),
                &recipients,
                &format!("reply-{n}"),
                &mut ev,
            )
            .await
            .expect("walk");
            let simmer::routing::chain::Walk::Selected(s) = walk else {
                panic!("a pinned reply is never exhausted by quota");
            };
            assert_eq!(s.route.name, "warming");
            marks.push((s.over_cap, simmer::routing::chain::render(&ev)));
            store
                .commit(&s.reservation, &s.recipient_keys)
                .await
                .unwrap();
        }

        assert_eq!(
            marks,
            vec![
                (false, "warming=selected".to_string()),
                (false, "warming=selected".to_string()),
                (true, "warming=selected_over_cap".to_string()),
                (true, "warming=selected_over_cap".to_string()),
            ],
            "the cap of 2 is spent in ordinary slots first"
        );
        assert_eq!(warming_usage(&store).await, (Some(2), 4, 0));
    }

    #[sqlx::test]
    async fn concurrent_replies_fill_the_cap_exactly_before_any_goes_past_it(pool: PgPool) {
        // Twelve pinned replies race for a cap of 5 with 2 already spent. All
        // twelve go out on warming; exactly the 3 remaining slots are taken as
        // ordinary reservations, and the other 9 are past the cap. The row lock
        // is what makes the split exact, so this has to be a real race.
        const N: usize = 12;
        let cfg =
            Arc::new(simmer::config::from_str(&config(1, 2, 5, "", ON), "test").expect("valid"));
        let store = store_for(pool);
        let frequency = Arc::new(simmer::frequency::Frequency::new());
        let preflight = Arc::new(simmer::preflight::Registry::new());

        let walk_one = |label: String| {
            let (cfg, store, frequency, preflight) = (
                Arc::clone(&cfg),
                Arc::clone(&store),
                Arc::clone(&frequency),
                Arc::clone(&preflight),
            );
            async move {
                let mut ev = Vec::new();
                let walk = simmer::routing::chain::walk_and_reserve(
                    &cfg,
                    &store,
                    &frequency,
                    &preflight,
                    &["warming".to_string(), "overflow".to_string()],
                    Some("warming"),
                    &["bob@example.com".to_string()],
                    &label,
                    &mut ev,
                )
                .await
                .expect("walk");
                let simmer::routing::chain::Walk::Selected(s) = walk else {
                    panic!("exhausted");
                };
                assert_eq!(s.route.name, "warming");
                store
                    .commit(&s.reservation, &s.recipient_keys)
                    .await
                    .unwrap();
                s.over_cap
            }
        };

        for n in 0..2 {
            assert!(!walk_one(format!("spent-{n}")).await);
        }

        let gate = Arc::new(tokio::sync::Barrier::new(N));
        let mut handles = Vec::new();
        for n in 0..N {
            let gate = Arc::clone(&gate);
            let fut = walk_one(format!("race-{n}"));
            handles.push(tokio::spawn(async move {
                gate.wait().await;
                fut.await
            }));
        }
        let mut past = 0;
        for h in handles {
            past += usize::from(h.await.expect("task"));
        }

        assert_eq!(past, N - 3, "only what the cap could not hold went past it");
        assert_eq!(warming_usage(&store).await, (Some(5), 2 + N as i64, 0));
    }

    #[sqlx::test]
    async fn a_failed_reply_past_the_cap_counts_nothing(pool: PgPool) {
        let warm = FakeDownstream::start(Script::default()).await;
        let over = FakeDownstream::start(Script::default()).await;
        let store = store_for(pool);
        let simmer = Simmer::start_with_quota(
            &config(warm.addr.port(), over.addr.port(), 1, "", ON),
            Arc::clone(&store),
        )
        .await;
        assert_eq!(send(&simmer, &first(1)).await.code, 250);
        let id = emitted_id(&warm.last().expect("warming carried it"));
        drop(simmer);

        let failing = FakeDownstream::start(Script::with(|s| {
            s.final_dot = Act::Reply(451, "4.3.0 try later");
        }))
        .await;
        let simmer = Simmer::start_with_quota(
            &config(failing.addr.port(), over.addr.port(), 1, "", ON),
            Arc::clone(&store),
        )
        .await;

        let r = send(&simmer, &reply_to(&id)).await;
        assert_eq!(r.code, 451, "{r:?}");
        assert_eq!(over.messages().len(), 0, "no failover past the cap either");
        assert_eq!(
            warming_usage(&store).await,
            (Some(1), 1, 0),
            "§7.4: a failed send consumes nothing, over the cap or not"
        );
    }

    #[sqlx::test]
    async fn only_the_pinned_route_goes_past_its_cap(pool: PgPool) {
        // Chain [warming, second], both capped at 1 and spent, no overflow.
        // Pinned to `second` while it is paused: the walk falls back to
        // warming, which is full — and is skipped for quota, not over-capped.
        let second = "\n  - name: second\n    downstream:\n      host: \"127.0.0.1\"\n      port: 1\n      \
                      tls: off\n      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }\n    \
                      identity:\n      envelope_from: \"b@second.com\"\n      set_headers:\n        \
                      Message-ID: \"<{{uuid}}@second.com>\"\n    warmup:\n      started: \"2020-01-01T00:00:00Z\"\n      \
                      schedule: { default: [1] }";
        let yaml = config(1, 2, 1, second, ON)
            .replace("chain: [warming, overflow] }", "chain: [warming, second] }");
        let cfg = simmer::config::from_str(&yaml, "test").expect("valid");
        let store = store_for(pool);
        let frequency = simmer::frequency::Frequency::new();
        let preflight = simmer::preflight::Registry::new();
        let recipients = ["bob@example.com".to_string()];

        for route in ["warming", "second"] {
            let mut ev = Vec::new();
            let simmer::routing::chain::Walk::Selected(s) =
                simmer::routing::chain::walk_and_reserve(
                    &cfg,
                    &store,
                    &frequency,
                    &preflight,
                    &[route.to_string()],
                    None,
                    &recipients,
                    "fill",
                    &mut ev,
                )
                .await
                .expect("walk")
            else {
                panic!("{route} had its one slot");
            };
            store.commit(&s.reservation, &[]).await.unwrap();
        }
        store.set_paused("second", true).await.unwrap();

        let pin = simmer::routing::thread::Pin::Route("second".into());
        let order =
            simmer::routing::thread::order(&["warming".to_string(), "second".to_string()], &pin);
        let mut ev = Vec::new();
        let walk = simmer::routing::chain::walk_and_reserve(
            &cfg,
            &store,
            &frequency,
            &preflight,
            &order,
            pin.route(),
            &recipients,
            "reply",
            &mut ev,
        )
        .await
        .expect("walk");
        assert!(matches!(walk, simmer::routing::chain::Walk::Exhausted));
        assert_eq!(
            simmer::routing::chain::render(&ev),
            "second=paused,warming=quota"
        );
        assert_eq!(warming_usage(&store).await, (Some(1), 1, 0));
    }

    #[sqlx::test]
    async fn affinity_turns_off_the_rcpt_to_early_check(pool: PgPool) {
        // Envelope-only rules and a chain with no overflow: §5.4's early check
        // would answer 451 at RCPT TO once warming is spent, before the reply's
        // headers exist. `Client::deliver` asserts RCPT TO is 250.
        let warm = FakeDownstream::start(Script::default()).await;
        let over = FakeDownstream::start(Script::default()).await;
        let yaml = config(warm.addr.port(), over.addr.port(), 1, "", ON)
            .replace("chain: [warming, overflow] }", "chain: [warming] }");
        let store: Arc<dyn QuotaStore> = Arc::new(PgQuotaStore::new(pool));
        let simmer = Simmer::start_with_quota(&yaml, Arc::clone(&store)).await;

        assert_eq!(send(&simmer, &first(1)).await.code, 250);
        let id = emitted_id(&warm.last().expect("warming carried it"));

        let r = send(&simmer, &first(2)).await;
        assert_eq!(r.code, 451, "exhausted, answered at the final dot: {r:?}");

        assert_eq!(send(&simmer, &reply_to(&id)).await.code, 250);
        assert_eq!(warm.messages().len(), 2);
        assert_eq!(over.messages().len(), 0);
    }
}
