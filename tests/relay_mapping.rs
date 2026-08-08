//! §10.1 end to end, against a scripted downstream, plus §10.2.
//!
//! `src/downstream/outcome.rs` unit-tests the mapping as a pure function. This
//! file asserts the same table survives a real conversation — that the stage a
//! failure is attributed to is the stage it actually happened at, which is what
//! `DECISIONS.md` D-008 turns on.

mod support;

use support::{config_for, Act, FakeDownstream, Script, Simmer, Tls};

const BODY: &str = "From: jane@oldbrand.com\r\nSubject: hello\r\n\r\nbody text\r\n";

/// Start a Simmer in front of a downstream running `script`.
async fn stack(script: Script) -> (FakeDownstream, Simmer) {
    let down = FakeDownstream::start(script).await;
    let simmer = Simmer::start(&config_for(down.addr, "")).await;
    (down, simmer)
}

// ---------------------------------------------------------------------------
// The success row
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_2xx_on_the_final_dot_becomes_250_accepted() {
    let (down, simmer) = stack(Script::default()).await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 250, "{r:?}");
    assert!(r.contains("accepted"));

    let got = down.last().expect("downstream received the message");
    assert_eq!(got.mail_from.as_deref(), Some("jane@oldbrand.com"));
    assert_eq!(got.recipients, vec!["bob@gmail.com".to_string()]);
}

#[tokio::test]
async fn phase_2_forwards_the_body_byte_for_byte() {
    // Phase 2 does no rewriting, so this is the baseline phase 4 must preserve:
    // whatever the client sent is what the downstream saw.
    let (down, simmer) = stack(Script::default()).await;
    let mut c = simmer.connect().await;
    c.hello().await;

    // Deliberately awkward: a line that is a bare dot, a line beginning with a
    // dot, and a trailing blank line — every place dot-stuffing can go wrong.
    let body =
        "From: jane@oldbrand.com\r\nSubject: dots\r\n\r\n.\r\n.hidden\r\n..two\r\nplain\r\n\r\n";
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", body).await;
    assert_eq!(r.code, 250, "{r:?}");

    let got = down.last().expect("received");
    assert_eq!(
        String::from_utf8_lossy(&got.body),
        body,
        "body was not forwarded verbatim"
    );
}

#[tokio::test]
async fn a_message_larger_than_the_spill_threshold_still_arrives_intact() {
    // §8.1: above 1 MiB the buffer moves to a temporary file. The seam between
    // memory and file is where a truncation would hide.
    let (down, simmer) = stack(Script::default()).await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let mut body = String::from("From: jane@oldbrand.com\r\nSubject: big\r\n\r\n");
    // Under max_message_bytes (100000 in the fixture) but far over any small
    // internal buffer.
    for i in 0..2000 {
        body.push_str(&format!("line {i} of a fairly long message body\r\n"));
    }

    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", &body).await;
    assert_eq!(r.code, 250, "{r:?}");
    let got = down.last().expect("received");
    assert_eq!(String::from_utf8_lossy(&got.body), body);
}

// ---------------------------------------------------------------------------
// D-008: the 5xx split by stage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_5xx_at_rcpt_to_is_the_one_permitted_550() {
    // The only place a permanent failure is genuinely about the recipient.
    let (_down, simmer) = stack(Script::with(|s| {
        s.rcpt_to = Act::Reply(550, "5.1.1 no such user");
    }))
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    let r = c.command("RCPT TO:<nobody@gmail.com>").await;
    // Phase 2 does not talk to the downstream until the final dot, so the
    // rejection surfaces there.
    assert_eq!(r.code, 250, "RCPT is accepted locally: {r:?}");
    assert_eq!(c.command("DATA").await.code, 354);
    c.send_raw(BODY.as_bytes()).await;
    c.send(".").await;

    let r = c.read_reply().await;
    assert_eq!(
        r.code, 550,
        "a downstream 5xx at RCPT TO must stay 550: {r:?}"
    );
    assert!(r.contains("550"), "the downstream code is quoted: {r:?}");
    assert!(
        r.contains("no such user"),
        "the downstream text is quoted: {r:?}"
    );
}

#[tokio::test]
async fn a_5xx_at_mail_from_becomes_451_not_550() {
    // D-008's central case: the §6.5 provisioning risk. The downstream rejects
    // our envelope sender because domain authentication is unfinished. A 550
    // here would permanently suppress a perfectly deliverable recipient.
    let (_down, simmer) = stack(Script::with(|s| {
        s.mail_from = Act::Reply(550, "5.7.1 sender domain not authenticated");
    }))
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;

    assert_eq!(r.code, 451, "must not be 550: {r:?}");
    assert!(
        r.contains("550"),
        "the real downstream code is still shown: {r:?}"
    );
    assert!(r.contains("not authenticated"));
}

#[tokio::test]
async fn a_5xx_at_data_and_at_the_final_dot_become_451() {
    for (stage, script) in [
        (
            "DATA",
            Script::with(|s| s.data = Act::Reply(554, "5.7.1 message refused")),
        ),
        (
            "final dot",
            Script::with(|s| s.final_dot = Act::Reply(552, "5.2.2 mailbox full")),
        ),
    ] {
        let (_down, simmer) = stack(script).await;
        let mut c = simmer.connect().await;
        c.hello().await;
        let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
        assert_eq!(r.code, 451, "5xx at {stage} must be 451: {r:?}");
    }
}

#[tokio::test]
async fn a_5xx_at_the_downstream_greeting_or_ehlo_becomes_451() {
    for (stage, script) in [
        (
            "greeting",
            Script::with(|s| s.greeting = Act::Reply(554, "5.3.2 not accepting mail")),
        ),
        (
            "EHLO",
            Script::with(|s| s.ehlo = Act::Reply(502, "5.5.1 go away")),
        ),
    ] {
        let (_down, simmer) = stack(script).await;
        let mut c = simmer.connect().await;
        c.hello().await;
        let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
        assert_eq!(r.code, 451, "5xx at {stage} must be 451: {r:?}");
    }
}

// ---------------------------------------------------------------------------
// 4xx at every stage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_4xx_at_any_stage_becomes_451_with_the_downstream_text() {
    let cases: Vec<(&str, Script)> = vec![
        (
            "greeting",
            Script::with(|s| s.greeting = Act::Reply(421, "4.3.2 try later")),
        ),
        (
            "mail from",
            Script::with(|s| s.mail_from = Act::Reply(451, "4.7.1 greylisted")),
        ),
        (
            "rcpt to",
            Script::with(|s| s.rcpt_to = Act::Reply(452, "4.2.2 over quota")),
        ),
        (
            "data",
            Script::with(|s| s.data = Act::Reply(451, "4.3.0 not now")),
        ),
        (
            "final dot",
            Script::with(|s| s.final_dot = Act::Reply(451, "4.3.0 deferred")),
        ),
    ];

    for (stage, script) in cases {
        let (_down, simmer) = stack(script).await;
        let mut c = simmer.connect().await;
        c.hello().await;
        let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
        assert_eq!(r.code, 451, "4xx at {stage}: {r:?}");
        assert!(r.contains("downstream said"), "{stage}: {r:?}");
    }
}

// ---------------------------------------------------------------------------
// Connect, timeout, TLS, protocol
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_connect_failure_is_451_4_4_1() {
    let dead = FakeDownstream::unreachable().await;
    let simmer = Simmer::start(&config_for(dead, "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 451, "{r:?}");
    assert!(r.contains("4.4.1"), "{r:?}");
    assert!(r.contains("unavailable"), "{r:?}");
}

#[tokio::test]
async fn a_stall_before_the_final_dot_is_451_4_4_2_timeout() {
    let (_down, simmer) = stack(Script::with(|s| s.mail_from = Act::Stall)).await;
    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 451, "{r:?}");
    assert!(r.contains("4.4.2"), "{r:?}");
    assert!(r.contains("timeout"), "{r:?}");
}

#[tokio::test]
async fn refused_starttls_under_required_verify_is_451_4_7_0() {
    // §12.3's "refuse TLS". The downstream advertises STARTTLS and then says no.
    let down = FakeDownstream::start(Script::with(|s| s.tls = Tls::Refuse)).await;
    let cfg = config_for(down.addr, "").replace("tls: off", "tls: required_verify");
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 451, "{r:?}");
    assert!(r.contains("4.7.0"), "{r:?}");
    assert!(r.contains("TLS"), "{r:?}");
}

#[tokio::test]
async fn a_downstream_that_does_not_offer_starttls_fails_under_required_verify() {
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "").replace("tls: off", "tls: required_verify");
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 451, "{r:?}");
    assert!(r.contains("4.7.0"), "{r:?}");
}

#[tokio::test]
async fn opportunistic_tls_falls_back_to_plaintext_when_starttls_fails() {
    // §8.2: "STARTTLS if advertised, continue in plaintext if not or if it
    // fails". The message must still be delivered.
    let down = FakeDownstream::start(Script::with(|s| s.tls = Tls::AcceptThenFail)).await;
    let cfg = config_for(down.addr, "").replace("tls: off", "tls: opportunistic");
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(
        r.code, 250,
        "opportunistic must not fail the message: {r:?}"
    );
    assert!(down.last().is_some(), "the message must still arrive");
}

#[tokio::test]
async fn opportunistic_tls_delivers_when_starttls_is_not_offered_at_all() {
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "").replace("tls: off", "tls: opportunistic");
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 250, "{r:?}");
}

#[tokio::test]
async fn a_protocol_violation_is_451_4_3_0() {
    // A reply that is not a reply at all.
    let (_down, simmer) = stack(Script::with(|s| {
        s.mail_from = Act::Reply(250, "ok");
        s.rcpt_to = Act::Reply(250, "ok");
        s.data = Act::Drop;
    }))
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 451, "{r:?}");
    assert!(r.contains("4.3.0"), "{r:?}");
}

// ---------------------------------------------------------------------------
// §10.2 — the ambiguous final dot
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_drop_after_the_terminating_dot_is_451_never_250() {
    // §10.2: "the message may or may not have been delivered. Simmer replies
    // 451". A 250 here would make Simmer vouch for a delivery it cannot confirm;
    // the duplicate a client retry may cause is the smaller harm.
    let (_down, simmer) = stack(Script::with(|s| s.final_dot = Act::Drop)).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;

    assert_eq!(r.code, 451, "must never be 250: {r:?}");
    assert!(r.contains("4.3.0"), "{r:?}");
}

#[tokio::test]
async fn a_drop_mid_data_is_451() {
    let (_down, simmer) = stack(Script::with(|s| s.drop_mid_data = true)).await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let mut big = String::from("From: jane@oldbrand.com\r\n\r\n");
    for i in 0..5000 {
        big.push_str(&format!("filler line {i}\r\n"));
    }

    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", &big).await;
    assert_eq!(r.code, 451, "{r:?}");
}

#[tokio::test]
async fn a_stall_at_the_final_dot_is_ambiguous_and_still_451() {
    let (_down, simmer) = stack(Script::with(|s| s.final_dot = Act::Stall)).await;
    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 451, "{r:?}");
}

// ---------------------------------------------------------------------------
// Sanitisation (§10.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_downstream_cannot_inject_a_reply_line_through_its_text() {
    // The fake cannot literally emit a CRLF inside one reply line, so this
    // asserts the milder property the wire permits: whatever text arrives is
    // reflected on exactly one line.
    let (_down, simmer) = stack(Script::with(|s| {
        s.final_dot = Act::Reply(451, "4.0.0 unusual\ttext\twith\ttabs");
    }))
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;

    assert_eq!(r.code, 451);
    assert_eq!(r.lines.len(), 1, "must be a single reply line: {r:?}");
    assert!(!r.text().contains('\t'), "tabs must be stripped: {r:?}");
}

// ---------------------------------------------------------------------------
// Downstream AUTH
// ---------------------------------------------------------------------------

#[tokio::test]
async fn simmer_authenticates_to_the_downstream_when_configured() {
    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "").replace(
        "      timeouts: { connect: 2s, command: 2s, data: 2s }",
        "      timeouts: { connect: 2s, command: 2s, data: 2s }\n      auth: { username: \"u\", password: \"p\" }",
    );
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 250, "{r:?}");
    assert!(
        down.last().expect("received").auth_seen,
        "AUTH was not sent"
    );
}

#[tokio::test]
async fn a_downstream_auth_rejection_is_451_not_550() {
    // A downstream refusing *our* credentials says nothing about the recipient.
    let down = FakeDownstream::start(Script::with(|s| {
        s.auth = Act::Reply(535, "5.7.8 bad credentials");
    }))
    .await;
    let cfg = config_for(down.addr, "").replace(
        "      timeouts: { connect: 2s, command: 2s, data: 2s }",
        "      timeouts: { connect: 2s, command: 2s, data: 2s }\n      auth: { username: \"u\", password: \"p\" }",
    );
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", BODY).await;
    assert_eq!(r.code, 451, "{r:?}");
}
