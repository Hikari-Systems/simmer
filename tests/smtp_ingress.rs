//! §5 ingress, end to end: the state machine, AUTH, limits, and PIPELINING.

mod support;

use support::{config_for, FakeDownstream, Script, Simmer};

const BODY: &str = "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n";

/// A Simmer in front of a downstream that accepts everything.
async fn stack(overrides: &str) -> (FakeDownstream, Simmer) {
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_for(down.addr, overrides)).await;
    (down, simmer)
}

// ---------------------------------------------------------------------------
// §5.2 — the ESMTP surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_banner_announces_esmtp() {
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    let banner = c.read_reply().await;
    assert_eq!(banner.code, 220);
    // Without "ESMTP" in the banner, some clients never try EHLO.
    assert!(banner.contains("ESMTP"), "{banner:?}");
    assert!(banner.contains("simmer.test"), "{banner:?}");
}

#[tokio::test]
async fn ehlo_advertises_exactly_the_spec_list() {
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    let r = c.hello().await;
    assert_eq!(r.code, 250);

    // §5.2: "EHLO advertises exactly PIPELINING, 8BITMIME, SMTPUTF8,
    // SIZE <max_message_bytes>, and AUTH PLAIN LOGIN when auth is enabled.
    // Nothing else."
    assert!(r.contains("PIPELINING"), "{r:?}");
    assert!(r.contains("8BITMIME"), "{r:?}");
    assert!(r.contains("SIZE 100000"), "{r:?}");

    // D-018: SMTPUTF8 is advertised only when every reachable route declares
    // support, and the fixture's route does not.
    assert!(!r.contains("SMTPUTF8"), "{r:?}");

    for banned in [
        "STARTTLS",
        "CHUNKING",
        "BDAT",
        "DSN",
        "ENHANCEDSTATUSCODES",
        "VRFY",
    ] {
        assert!(
            !r.contains(banned),
            "{banned} must not be advertised: {r:?}"
        );
    }
}

#[tokio::test]
async fn smtputf8_is_advertised_when_every_route_declares_it() {
    // D-018 the other way round.
    let down = FakeDownstream::start(Script::default()).await;
    let cfg =
        config_for(down.addr, "").replace("      tls: off", "      tls: off\n      smtputf8: true");
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    let r = c.hello().await;
    assert!(r.contains("SMTPUTF8"), "{r:?}");
}

#[tokio::test]
async fn a_utf8_address_is_refused_when_smtputf8_is_not_advertised() {
    // O-10 / D-018: answered at MAIL FROM, before a body is transferred, rather
    // than discovered mid-relay.
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let r = c.command("MAIL FROM:<jane@öldbrand.com>").await;
    assert_eq!(r.code, 550, "{r:?}");
    assert!(r.contains("5.6.7"), "{r:?}");
}

#[tokio::test]
async fn a_utf8_recipient_is_refused_too() {
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    let r = c.command("RCPT TO:<bob@gmâil.com>").await;
    assert_eq!(r.code, 550, "{r:?}");
    assert!(r.contains("5.6.7"), "{r:?}");
}

#[tokio::test]
async fn the_smtputf8_parameter_is_refused_when_unadvertised() {
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.command("MAIL FROM:<jane@oldbrand.com> SMTPUTF8").await;
    assert_eq!(r.code, 550, "{r:?}");
}

#[tokio::test]
async fn helo_is_accepted() {
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    assert_eq!(c.read_reply().await.code, 220);
    let r = c.command("HELO client.test").await;
    assert_eq!(r.code, 250, "{r:?}");
    assert_eq!(r.lines.len(), 1, "HELO gets a single-line reply: {r:?}");
}

#[tokio::test]
async fn extension_parameters_are_refused_after_helo() {
    // HELO means no extensions were negotiated, so SIZE= is a syntax error.
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    assert_eq!(c.read_reply().await.code, 220);
    assert_eq!(c.command("HELO client.test").await.code, 250);
    let r = c.command("MAIL FROM:<jane@oldbrand.com> SIZE=10").await;
    assert_eq!(r.code, 501, "{r:?}");
}

#[tokio::test]
async fn the_simple_verbs_answer_as_the_spec_requires() {
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;

    assert_eq!(c.command("NOOP").await.code, 250);
    assert_eq!(c.command("RSET").await.code, 250);
    // §5.2: VRFY always 252 — never 250, which would make Simmer an address
    // oracle.
    assert_eq!(c.command("VRFY postmaster").await.code, 252);
    assert_eq!(c.command("EXPN a-list").await.code, 502);
    // §5.2: "BDAT is 502 5.5.1 command not implemented" — 502 rather than 500,
    // so a client knows to fall back to DATA.
    let r = c.command("BDAT 100 LAST").await;
    assert_eq!(r.code, 502, "{r:?}");
    assert_eq!(c.command("FROBNICATE").await.code, 500);
    assert_eq!(c.command("QUIT").await.code, 221);
}

#[tokio::test]
async fn commands_out_of_sequence_are_503() {
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    assert_eq!(c.read_reply().await.code, 220);

    // MAIL before EHLO.
    assert_eq!(c.command("MAIL FROM:<a@oldbrand.com>").await.code, 503);

    c.command("EHLO client.test").await;
    // RCPT before MAIL, and DATA before RCPT.
    assert_eq!(c.command("RCPT TO:<b@gmail.com>").await.code, 503);
    assert_eq!(c.command("DATA").await.code, 503);

    assert_eq!(c.command("MAIL FROM:<a@oldbrand.com>").await.code, 250);
    // A second MAIL inside a transaction.
    assert_eq!(c.command("MAIL FROM:<c@oldbrand.com>").await.code, 503);
    // DATA with no recipients.
    assert_eq!(c.command("DATA").await.code, 503);
}

#[tokio::test]
async fn rset_abandons_the_transaction() {
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;

    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<bob@gmail.com>").await.code, 250);
    assert_eq!(c.command("RSET").await.code, 250);
    // ...so DATA now has nothing to send.
    assert_eq!(c.command("DATA").await.code, 503);
    // ...and a fresh transaction works.
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
}

#[tokio::test]
async fn ehlo_mid_transaction_resets_it() {
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    assert_eq!(c.command("EHLO client.test").await.code, 250);
    assert_eq!(c.command("DATA").await.code, 503);
}

#[tokio::test]
async fn a_malformed_command_does_not_desynchronise_the_session() {
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;

    assert_eq!(c.command("MAIL FROM:no-brackets").await.code, 501);
    assert_eq!(
        c.command("MAIL FROM:<a@oldbrand.com> SIZE=banana")
            .await
            .code,
        501
    );
    // RFC 1869 §6 — a parameter we do not implement is 555, not silence.
    assert_eq!(
        c.command("MAIL FROM:<a@oldbrand.com> RET=FULL").await.code,
        555
    );
    // The session is still usable.
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
}

// ---------------------------------------------------------------------------
// §5.2 — PIPELINING
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pipelined_transaction_gets_one_reply_per_command_in_order() {
    let (down, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;

    // Everything up to DATA in a single write — which is what PIPELINING means
    // and what a client that saw the capability will actually do.
    c.send_raw(
        b"MAIL FROM:<jane@oldbrand.com>\r\n\
          RCPT TO:<bob@gmail.com>\r\n\
          DATA\r\n",
    )
    .await;

    let mail = c.read_reply().await;
    let rcpt = c.read_reply().await;
    let data = c.read_reply().await;
    assert_eq!(mail.code, 250, "MAIL: {mail:?}");
    assert_eq!(rcpt.code, 250, "RCPT: {rcpt:?}");
    assert_eq!(data.code, 354, "DATA: {data:?}");

    c.send_raw(BODY.as_bytes()).await;
    c.send(".").await;
    assert_eq!(c.read_reply().await.code, 250);
    assert!(down.last().is_some());
}

#[tokio::test]
async fn a_pipelined_batch_containing_an_error_stays_in_step() {
    // RFC 2920 §3.1: the server must keep answering, one reply per command, even
    // after a rejection. Dropping buffered input here would make the client
    // attribute every subsequent reply to the wrong command.
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;

    // The second command is syntactically invalid — no angle brackets — so it is
    // refused while the ones around it are not.
    c.send_raw(
        b"MAIL FROM:<jane@oldbrand.com>\r\n\
          RCPT TO:missing-brackets\r\n\
          RCPT TO:<bob@gmail.com>\r\n\
          NOOP\r\n",
    )
    .await;

    assert_eq!(c.read_reply().await.code, 250, "MAIL");
    assert_eq!(c.read_reply().await.code, 501, "the bad RCPT");
    assert_eq!(c.read_reply().await.code, 250, "the good RCPT");
    assert_eq!(c.read_reply().await.code, 250, "NOOP");
}

// ---------------------------------------------------------------------------
// §5.3 — authentication
// ---------------------------------------------------------------------------

/// argon2id hash of "local-dev-password".
const HASH: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$iYj0sVhRvzAWM9kzsFBKyg$V1tnVXTGEO+BD0x9q1uccuo91w84jrz+qDKA3qElLFE";

async fn auth_stack() -> (FakeDownstream, Simmer) {
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "").replace(
        "    required: false\n    allow_insecure_auth: true",
        &format!(
            "    required: true\n    allow_insecure_auth: true\n    users:\n      - username: \"cfapp\"\n        password_hash: \"{HASH}\""
        ),
    );
    let simmer = Simmer::start(&cfg).await;
    (down, simmer)
}

fn b64(s: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

#[tokio::test]
async fn ehlo_advertises_auth_when_it_is_enabled() {
    let (_d, simmer) = auth_stack().await;
    let mut c = simmer.connect().await;
    let r = c.hello().await;
    assert!(r.contains("AUTH PLAIN LOGIN"), "{r:?}");
}

#[tokio::test]
async fn mail_from_is_refused_before_authentication() {
    let (_d, simmer) = auth_stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.command("MAIL FROM:<jane@oldbrand.com>").await;
    assert_eq!(r.code, 530, "{r:?}");
    assert!(r.contains("5.7.0"), "{r:?}");
}

#[tokio::test]
async fn auth_plain_with_an_initial_response_succeeds() {
    let (_d, simmer) = auth_stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let payload = b64("\0cfapp\0local-dev-password");
    let r = c.command(&format!("AUTH PLAIN {payload}")).await;
    assert_eq!(r.code, 235, "{r:?}");
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
}

#[tokio::test]
async fn auth_plain_in_two_steps_succeeds() {
    let (_d, simmer) = auth_stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let r = c.command("AUTH PLAIN").await;
    assert_eq!(r.code, 334, "{r:?}");
    let r = c.command(&b64("\0cfapp\0local-dev-password")).await;
    assert_eq!(r.code, 235, "{r:?}");
}

#[tokio::test]
async fn auth_login_walks_username_then_password() {
    let (_d, simmer) = auth_stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let r = c.command("AUTH LOGIN").await;
    assert_eq!(r.code, 334, "{r:?}");
    assert!(
        r.contains("VXNlcm5hbWU6"),
        "must challenge 'Username:': {r:?}"
    );

    let r = c.command(&b64("cfapp")).await;
    assert_eq!(r.code, 334, "{r:?}");
    assert!(
        r.contains("UGFzc3dvcmQ6"),
        "must challenge 'Password:': {r:?}"
    );

    let r = c.command(&b64("local-dev-password")).await;
    assert_eq!(r.code, 235, "{r:?}");
}

#[tokio::test]
async fn a_wrong_password_is_535() {
    let (_d, simmer) = auth_stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c
        .command(&format!("AUTH PLAIN {}", b64("\0cfapp\0wrong")))
        .await;
    assert_eq!(r.code, 535, "{r:?}");
}

#[tokio::test]
async fn three_failures_end_the_connection_with_421() {
    // §5.3: "Failed attempts are rate-limited per connection (three failures
    // then 421 and disconnect)."
    let (_d, simmer) = auth_stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let bad = format!("AUTH PLAIN {}", b64("\0cfapp\0wrong"));
    assert_eq!(c.command(&bad).await.code, 535, "first failure");
    assert_eq!(c.command(&bad).await.code, 535, "second failure");

    let r = c.command(&bad).await;
    assert_eq!(r.code, 421, "third failure must be 421: {r:?}");
    assert!(c.is_closed().await, "the connection must actually close");
}

#[tokio::test]
async fn an_unknown_username_is_535_not_a_different_code() {
    // Anything that distinguished "no such user" from "wrong password" would be
    // a user-enumeration oracle.
    let (_d, simmer) = auth_stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c
        .command(&format!(
            "AUTH PLAIN {}",
            b64("\0nobody\0local-dev-password")
        ))
        .await;
    assert_eq!(r.code, 535, "{r:?}");
}

#[tokio::test]
async fn an_unsupported_mechanism_is_504() {
    let (_d, simmer) = auth_stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("AUTH CRAM-MD5").await.code, 504);
    assert_eq!(c.command("AUTH XOAUTH2 abcdef").await.code, 504);
}

#[tokio::test]
async fn an_auth_exchange_can_be_cancelled() {
    let (_d, simmer) = auth_stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("AUTH LOGIN").await.code, 334);
    assert_eq!(c.command("*").await.code, 501);
    // ...and the session is still usable afterwards.
    assert_eq!(c.command("NOOP").await.code, 250);
}

#[tokio::test]
async fn a_malformed_auth_payload_does_not_count_as_a_failed_attempt() {
    // A client with a broken base64 encoder should be told what is wrong, not
    // disconnected after three tries.
    let (_d, simmer) = auth_stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;

    for _ in 0..5 {
        assert_eq!(c.command("AUTH PLAIN !!!not-base64!!!").await.code, 501);
    }
    // Still connected, and a correct attempt still works.
    let payload = b64("\0cfapp\0local-dev-password");
    assert_eq!(c.command(&format!("AUTH PLAIN {payload}")).await.code, 235);
}

#[tokio::test]
async fn authenticating_twice_is_503() {
    let (_d, simmer) = auth_stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;
    let payload = b64("\0cfapp\0local-dev-password");
    assert_eq!(c.command(&format!("AUTH PLAIN {payload}")).await.code, 235);
    assert_eq!(c.command(&format!("AUTH PLAIN {payload}")).await.code, 503);
}

// ---------------------------------------------------------------------------
// §5.5, §5.6 — limits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_size_parameter_over_the_limit_is_refused_before_the_body() {
    // The entire reason SIZE is advertised: refuse a 30 MiB message without
    // transferring 30 MiB first.
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c
        .command("MAIL FROM:<jane@oldbrand.com> SIZE=999999999")
        .await;
    assert_eq!(r.code, 552, "{r:?}");
    assert!(r.contains("5.3.4"), "{r:?}");
}

#[tokio::test]
async fn a_body_over_the_limit_is_552() {
    let down = FakeDownstream::start(Script::default()).await;
    let cfg =
        config_for(down.addr, "").replace("max_message_bytes: 100000", "max_message_bytes: 2000");
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<bob@gmail.com>").await.code, 250);
    assert_eq!(c.command("DATA").await.code, 354);

    let mut big = String::from("From: jane@oldbrand.com\r\n\r\n");
    for i in 0..500 {
        big.push_str(&format!("padding line number {i}\r\n"));
    }
    c.send_raw(big.as_bytes()).await;
    c.send(".").await;

    let r = c.read_reply().await;
    assert_eq!(r.code, 552, "{r:?}");
    assert!(r.contains("5.3.4"), "{r:?}");
    assert!(down.last().is_none(), "nothing must reach the downstream");
}

#[tokio::test]
async fn a_generous_max_recipients_does_not_permit_a_second_one() {
    // D-047 — §5.5's ceiling is subsumed. The fixture says `max_recipients: 5`,
    // and it makes no difference: the second RCPT TO is refused anyway, so no
    // configuration can reach the ceiling.
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "").replace("max_recipients: 5", "max_recipients: 100");
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<a@gmail.com>").await.code, 250);

    let r = c.command("RCPT TO:<b@gmail.com>").await;
    assert_eq!(r.code, 452, "{r:?}");
    assert!(r.contains("4.5.3"), "{r:?}");
}

#[tokio::test]
async fn a_second_rcpt_to_is_always_refused() {
    // D-047 — unconditional, with no switch to turn it off: collapsing several
    // per-recipient outcomes into one reply is lossy, and SMTP allows exactly
    // one reply.
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_for(down.addr, "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<a@gmail.com>").await.code, 250);

    let r = c.command("RCPT TO:<b@gmail.com>").await;
    assert_eq!(r.code, 452, "{r:?}");
    assert!(r.contains("multiple recipients"), "{r:?}");
    // 452 rather than 5xx: the recipient is deliverable and §14.1 will not have
    // a limit of ours recorded against them permanently.
}

#[tokio::test]
async fn a_refused_second_recipient_leaves_the_transaction_usable() {
    // The refusal is per RCPT TO, not per transaction: RFC 5321 lets the client
    // carry on with the recipients it does have, and a session that had to be
    // reset would turn our limit into a delivery failure for recipient one.
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_for(down.addr, "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<a@gmail.com>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<b@gmail.com>").await.code, 452);

    assert_eq!(c.command("DATA").await.code, 354);
    c.send_raw(BODY.as_bytes()).await;
    c.send(".").await;
    assert_eq!(c.read_reply().await.code, 250);

    let got = down.last().expect("received");
    assert_eq!(got.recipients, vec!["a@gmail.com".to_string()]);
}

#[tokio::test]
async fn an_over_long_command_line_is_refused_without_closing_the_session() {
    let (_d, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let huge = format!("MAIL FROM:<{}@oldbrand.com>", "x".repeat(8000));
    let r = c.command(&huge).await;
    assert_eq!(r.code, 500, "{r:?}");
    assert!(r.contains("line too long"), "{r:?}");
}

// ---------------------------------------------------------------------------
// §5.1 — access control and concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_peer_outside_allowed_cidrs_is_refused() {
    // §2.3: the listener is plaintext and takes plaintext AUTH, so this is the
    // only thing between it and an untrusted network.
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "").replace(
        r#"allowed_cidrs: ["127.0.0.0/8"]"#,
        r#"allowed_cidrs: ["10.0.0.0/8"]"#,
    );
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    let r = c.read_reply().await;
    assert_eq!(r.code, 554, "{r:?}");
    assert!(r.contains("5.7.1"), "{r:?}");
    assert!(c.is_closed().await);
}

#[tokio::test]
async fn exceeding_max_concurrent_sessions_is_421() {
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "")
        .replace("max_concurrent_sessions: 16", "max_concurrent_sessions: 2");
    let simmer = Simmer::start(&cfg).await;

    // Hold two sessions open.
    let mut a = simmer.connect().await;
    assert_eq!(a.read_reply().await.code, 220);
    let mut b = simmer.connect().await;
    assert_eq!(b.read_reply().await.code, 220);

    let mut third = simmer.connect().await;
    let r = third.read_reply().await;
    assert_eq!(r.code, 421, "{r:?}");
    assert!(r.contains("4.3.2"), "{r:?}");
    assert!(third.is_closed().await);

    // Freeing a slot lets the next connection in.
    assert_eq!(a.command("QUIT").await.code, 221);
    drop(a);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut fourth = simmer.connect().await;
    assert_eq!(fourth.read_reply().await.code, 220);
}

// ---------------------------------------------------------------------------
// §8.4 — timeouts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_idle_client_is_disconnected_with_421() {
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "").replace(
        "timeouts: { command: 5s, data: 5s, session: 60s }",
        "timeouts: { command: 1s, data: 5s, session: 60s }",
    );
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    assert_eq!(c.read_reply().await.code, 220);

    // Say nothing and wait out the command budget.
    let r = c.read_reply().await;
    assert_eq!(r.code, 421, "{r:?}");
    assert!(r.contains("4.4.2"), "{r:?}");
}

#[tokio::test]
async fn a_stalled_data_transfer_times_out_with_421() {
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "").replace(
        "timeouts: { command: 5s, data: 5s, session: 60s }",
        "timeouts: { command: 5s, data: 1s, session: 60s }",
    );
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<bob@gmail.com>").await.code, 250);
    assert_eq!(c.command("DATA").await.code, 354);

    // Send a partial body and then stop, never sending the terminating dot.
    c.send_raw(b"From: jane@oldbrand.com\r\nSubject: never finished\r\n")
        .await;

    let r = c.read_reply().await;
    assert_eq!(r.code, 421, "{r:?}");
    assert!(r.contains("4.4.2"), "{r:?}");
    assert!(
        down.last().is_none(),
        "an unfinished message must not be relayed"
    );
}

// ---------------------------------------------------------------------------
// §5.4 — routing decisions surfaced to the client
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strict_senders_rejects_an_unmatched_sender_at_rcpt_to() {
    // §5.4: "When all rules use `envelope`, Simmer should decide early and
    // reject at RCPT TO to avoid a wasted body transfer." The fixture's one rule
    // is envelope-only, so this lands at RCPT TO rather than the final dot.
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "").replace(
        "default_chain: [only]",
        "default_chain: [only]\nstrict_senders: true",
    );
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(
        c.command("MAIL FROM:<someone@unconfigured.com>").await.code,
        250
    );

    let r = c.command("RCPT TO:<bob@gmail.com>").await;
    assert_eq!(r.code, 550, "{r:?}");
    // §10.3 permits this 550: it is a statement about the *sender*, cannot
    // trigger recipient suppression, and should be loud.
    assert!(r.contains("5.7.1"), "{r:?}");
    assert!(r.contains("sender domain not configured"), "{r:?}");
}

#[tokio::test]
async fn a_missing_from_header_is_550_5_6_0_when_a_rule_needs_it() {
    // §5.4: "A From: header that is absent, unparseable, or contains a group
    // syntax with no addresses causes 550 5.6.0 when match_on requires it."
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "").replace("match_on: envelope", "match_on: from_header");
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;

    let r = c
        .deliver(
            "jane@oldbrand.com",
            "bob@gmail.com",
            "Subject: no From header at all\r\n\r\nbody\r\n",
        )
        .await;
    assert_eq!(r.code, 550, "{r:?}");
    assert!(r.contains("5.6.0"), "{r:?}");
}

#[tokio::test]
async fn from_header_routing_decides_at_the_final_dot() {
    // The other half of §5.4's consequence: with a from_header rule the decision
    // cannot be made until the body has arrived, so RCPT TO is accepted first.
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "").replace("match_on: envelope", "match_on: from_header");
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 250, "{r:?}");
    assert!(down.last().is_some());
}

// ---------------------------------------------------------------------------
// End to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_message_on_the_same_connection_works() {
    let (down, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;

    assert_eq!(
        c.deliver("jane@oldbrand.com", "a@gmail.com", BODY)
            .await
            .code,
        250
    );
    assert_eq!(
        c.deliver("jane@oldbrand.com", "b@gmail.com", BODY)
            .await
            .code,
        250
    );
    assert_eq!(c.command("QUIT").await.code, 221);

    assert_eq!(down.messages().len(), 2);
    assert_eq!(
        down.messages()[0].recipients,
        vec!["a@gmail.com".to_string()]
    );
    assert_eq!(
        down.messages()[1].recipients,
        vec!["b@gmail.com".to_string()]
    );
}

#[tokio::test]
async fn every_downstream_transaction_carries_exactly_one_recipient() {
    // D-047, as the downstream sees it. Two recipients means two messages, and
    // each arrives on its own transaction with one RCPT TO — which is what makes
    // the reply the client gets unambiguously about the recipient it names.
    let (down, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;

    assert_eq!(
        c.deliver("jane@oldbrand.com", "a@gmail.com", BODY)
            .await
            .code,
        250
    );
    assert_eq!(
        c.deliver("jane@oldbrand.com", "b@yahoo.com", BODY)
            .await
            .code,
        250
    );

    let messages = down.messages();
    assert_eq!(messages.len(), 2);
    for m in &messages {
        assert_eq!(m.recipients.len(), 1, "{:?}", m.recipients);
    }
}

#[tokio::test]
async fn the_null_sender_relays() {
    let (down, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;

    assert_eq!(c.command("MAIL FROM:<>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<bob@gmail.com>").await.code, 250);
    assert_eq!(c.command("DATA").await.code, 354);
    c.send_raw(BODY.as_bytes()).await;
    c.send(".").await;
    assert_eq!(c.read_reply().await.code, 250);

    assert_eq!(down.last().expect("received").mail_from, None);
}

#[tokio::test]
async fn body_8bitmime_is_carried_through_to_the_downstream() {
    let (down, simmer) = stack("").await;
    let mut c = simmer.connect().await;
    c.hello().await;

    assert_eq!(
        c.command("MAIL FROM:<jane@oldbrand.com> BODY=8BITMIME")
            .await
            .code,
        250
    );
    assert_eq!(c.command("RCPT TO:<bob@gmail.com>").await.code, 250);
    assert_eq!(c.command("DATA").await.code, 354);
    c.send_raw(BODY.as_bytes()).await;
    c.send(".").await;
    assert_eq!(c.read_reply().await.code, 250);

    let got = down.last().expect("received");
    assert!(
        got.mail_from_params.contains("BODY=8BITMIME"),
        "params were {:?}",
        got.mail_from_params
    );
}
