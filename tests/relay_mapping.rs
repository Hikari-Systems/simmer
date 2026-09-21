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
    // §6.1 step 9: the envelope sender is the route's, not the client's. The
    // fixture route sets `envelope_from: "b@example.com"`.
    assert_eq!(got.mail_from.as_deref(), Some("b@example.com"));
    assert_eq!(got.recipients, vec!["bob@gmail.com".to_string()]);
}

#[tokio::test]
async fn everything_the_route_does_not_name_is_forwarded_byte_for_byte() {
    // Phase 2's `phase_2_forwards_the_body_byte_for_byte`, sharpened rather than
    // deleted. Phase 4 prepends a Received: header (§6.1 step 8) and rewrites the
    // envelope sender, so "the whole message is verbatim" is no longer true —
    // but everything the route was not configured to touch still is, and the
    // awkward body is the part that would break first.
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
        support::without_received(&got.body),
        body,
        "everything below the Received: header should be untouched"
    );
}

// ---------------------------------------------------------------------------
// §6.4 through the relay
// ---------------------------------------------------------------------------

/// A route that rewrites links in the body, as `simmer.yaml` does.
const WITH_BODY_REWRITES: &str = concat!(
    "      body_rewrites:\n",
    "        - pattern: 'https://oldbrand\\.com/'\n",
    "          replacement: \"https://newbrand.com/\"\n",
);

async fn stack_rewriting_bodies() -> (FakeDownstream, Simmer) {
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_for(down.addr, WITH_BODY_REWRITES)).await;
    (down, simmer)
}

#[tokio::test]
async fn a_body_no_rule_matches_still_arrives_byte_for_byte() {
    // The end-to-end half of what D-039 used to guarantee structurally. §6.4
    // means the body is no longer opaque, so this is the property that replaces
    // it: a configured route that finds nothing to change changes nothing —
    // including the MIME boundaries, the transfer encodings and the trailing
    // whitespace.
    let (down, simmer) = stack_rewriting_bodies().await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let body = concat!(
        "From: jane@oldbrand.com\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/mixed; boundary=\"b1\"\r\n",
        "\r\n",
        "preamble text\r\n",
        "--b1\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "Content-Transfer-Encoding: quoted-printable\r\n",
        "\r\n",
        "Nothing here matches. Gr=C3=BC=C3=9Fe, and a soft=20\r\n",
        "break.\r\n",
        "--b1\r\n",
        "Content-Type: application/pdf\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "JVBERi0xLjQK\r\n",
        "--b1--\r\n",
        "epilogue\r\n",
        "\r\n",
    );
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", body).await;
    assert_eq!(r.code, 250, "{r:?}");

    let got = down.last().expect("received");
    assert_eq!(support::without_received(&got.body), body);
}

#[tokio::test]
async fn a_link_in_a_text_part_is_rewritten_on_the_way_through() {
    let (down, simmer) = stack_rewriting_bodies().await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let body = concat!(
        "From: jane@oldbrand.com\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "Track it at https://oldbrand.com/track\r\n",
    );
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", body).await;
    assert_eq!(r.code, 250, "{r:?}");

    let got = support::without_received(&down.last().expect("received").body);
    assert!(got.contains("https://newbrand.com/track"), "{got}");
    assert!(!got.contains("oldbrand.com/track"), "{got}");
}

// ---------------------------------------------------------------------------
// D-089 through the relay
// ---------------------------------------------------------------------------

/// D-089's case: the body links and the unsubscribe header both move to the
/// proxy host, and `set_headers` still wins over a rewrite of the same header.
const WITH_HEADER_REWRITES: &str = concat!(
    "      set_headers:\n",
    "        X-Set: \"explicit\"\n",
    "      body_rewrites:\n",
    "        - pattern: 'https://www\\.meddoc\\.net/'\n",
    "          replacement: \"https://link-pmps.healthcarematch.com/\"\n",
    "      header_rewrites:\n",
    "        - header: List-Unsubscribe\n",
    "          pattern: '<https://www\\.meddoc\\.net/'\n",
    "          replacement: '<https://link-pmps.healthcarematch.com/'\n",
    "        - header: X-Set\n",
    "          pattern: 'old'\n",
    "          replacement: 'new'\n",
);

#[tokio::test]
async fn the_unsubscribe_header_moves_host_with_the_body_links_and_keeps_its_token() {
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_for(down.addr, WITH_HEADER_REWRITES)).await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let body = concat!(
        "From: MedDoc <news@oldbrand.com>\r\n",
        "subject:  Two  spaces\r\n",
        "X-Folded: first\r\n\tsecond\r\n",
        "X-Set: old\r\n",
        "List-Unsubscribe: <mailto:u@meddoc.net>,\r\n",
        " <https://www.meddoc.net/unsub.cfm?13323193_418550_3_9011119906_90535>\r\n",
        "List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "Unsubscribe: https://www.meddoc.net/unsub.cfm?13323193_418550_3_9011119906_90535\r\n",
    );
    let r = c.deliver("news@oldbrand.com", "bob@gmail.com", body).await;
    assert_eq!(r.code, 250, "{r:?}");

    let got = support::without_received(&down.last().expect("received").body);
    let expected = body
        .replace(
            "List-Unsubscribe: <mailto:u@meddoc.net>,\r\n <https://www.meddoc.net/",
            "List-Unsubscribe: <mailto:u@meddoc.net>, <https://link-pmps.healthcarematch.com/",
        )
        .replace("X-Set: old", "X-Set: explicit")
        .replace(
            "Unsubscribe: https://www.meddoc.net/",
            "Unsubscribe: https://link-pmps.healthcarematch.com/",
        );
    // Byte for byte: the rewritten header unfolded, the token untouched, and
    // every header no rule changed exactly as it arrived.
    assert_eq!(got, expected);

    // And a message the application already sends to the proxy host — §1.1's
    // arrangement B — passes through byte for byte.
    let already = expected.clone();
    let r = c
        .deliver("news@oldbrand.com", "bob@gmail.com", &already)
        .await;
    assert_eq!(r.code, 250, "{r:?}");
    let got = support::without_received(&down.last().expect("received").body);
    assert_eq!(got, already);
}

#[tokio::test]
async fn a_link_split_across_a_soft_line_break_is_rewritten_and_the_rest_is_left_alone() {
    // §6.4's stated reason for decoding first: a URL written across a
    // quoted-printable soft break "is the common case in real mail rather than
    // an edge case". The attachment beside it encodes the same URL and must
    // survive — decoding it to find out is exactly what §6.4 forbids.
    let (down, simmer) = stack_rewriting_bodies().await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let body = concat!(
        "From: jane@oldbrand.com\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/mixed; boundary=\"b1\"\r\n",
        "\r\n",
        "--b1\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "Content-Transfer-Encoding: quoted-printable\r\n",
        "\r\n",
        "Track it at https://old=\r\nbrand.com/track today\r\n",
        "--b1\r\n",
        "Content-Type: application/octet-stream\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "aHR0cHM6Ly9vbGRicmFuZC5jb20vdHJhY2s=\r\n",
        "--b1--\r\n",
    );
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", body).await;
    assert_eq!(r.code, 250, "{r:?}");

    let got = support::without_received(&down.last().expect("received").body);
    assert!(got.contains("https://newbrand.com/track"), "{got}");
    assert!(
        got.contains("aHR0cHM6Ly9vbGRicmFuZC5jb20vdHJhY2s="),
        "the attachment was touched:\n{got}"
    );
    // And the framing around the rewritten part is still the framing that
    // arrived.
    assert!(got.contains("--b1--\r\n"), "{got}");
}

#[tokio::test]
async fn a_signed_message_passes_through_unrewritten() {
    // §6.4: "Signed or encrypted parts … are never rewritten; rewriting would
    // invalidate them." The link inside is a live match, which is the point.
    let (down, simmer) = stack_rewriting_bodies().await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let body = concat!(
        "From: jane@oldbrand.com\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/signed; boundary=\"sig\"; ",
        "protocol=\"application/pkcs7-signature\"\r\n",
        "\r\n",
        "--sig\r\n",
        "Content-Type: text/plain\r\n",
        "\r\n",
        "Track it at https://oldbrand.com/track\r\n",
        "--sig\r\n",
        "Content-Type: application/pkcs7-signature\r\n",
        "\r\n",
        "MIIFnotarealsignature\r\n",
        "--sig--\r\n",
    );
    let r = c.deliver("jane@oldbrand.com", "bob@gmail.com", body).await;
    assert_eq!(r.code, 250, "{r:?}");

    assert_eq!(
        support::without_received(&down.last().expect("received").body),
        body,
        "a signed message must arrive exactly as it was sent"
    );
}

#[tokio::test]
async fn exactly_one_received_header_is_added() {
    // §6.1 step 8, and the phase 4 call that Simmer adds nothing else: one
    // header, at the top, naming the client and this hop.
    let (down, simmer) = stack(Script::default()).await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let body = "From: jane@oldbrand.com\r\nSubject: t\r\n\r\nhi\r\n";
    assert_eq!(
        c.deliver("jane@oldbrand.com", "bob@gmail.com", body)
            .await
            .code,
        250
    );

    let got = down.last().expect("received");
    let text = String::from_utf8_lossy(&got.body).to_string();
    assert_eq!(text.matches("Received:").count(), 1, "{text}");
    assert!(text.starts_with("Received: from "), "{text}");
    assert!(text.contains("by simmer.test with ESMTP id "), "{text}");
    // No X-Simmer-* headers: every header Simmer adds is one the recipient sees
    // that would vanish when Simmer is unplugged.
    assert!(!text.contains("X-Simmer"), "{text}");
}

#[tokio::test]
async fn authentication_artefacts_are_stripped_on_the_way_through() {
    // §6.5, end to end. Simmer holds no key material, and a *failing* signature
    // is treated more harshly by filters than an absent one.
    let (down, simmer) = stack(Script::default()).await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let body = "From: jane@oldbrand.com\r\n\
                DKIM-Signature: v=1; a=rsa-sha256; d=oldbrand.com; b=abc\r\n\
                Authentication-Results: mx.example.com; spf=pass\r\n\
                ARC-Seal: i=1; cv=none\r\n\
                Subject: t\r\n\r\nhi\r\n";
    assert_eq!(
        c.deliver("jane@oldbrand.com", "bob@gmail.com", body)
            .await
            .code,
        250
    );

    let text = String::from_utf8_lossy(&down.last().expect("received").body).to_string();
    for artefact in ["DKIM-Signature", "Authentication-Results", "ARC-Seal"] {
        assert!(!text.contains(artefact), "{artefact} survived:\n{text}");
    }
    assert!(text.contains("Subject: t\r\n"), "{text}");
}

#[tokio::test]
async fn a_null_sender_survives_the_rewrite() {
    // D-035: MAIL FROM:<> identifies a bounce and RFC 5321 §6.1 requires it.
    // The fixture route sets envelope_from unconditionally; the null sender is
    // the one input it must not apply to.
    let (down, simmer) = stack(Script::default()).await;
    let mut c = simmer.connect().await;
    c.hello().await;

    let body = "From: jane@oldbrand.com\r\nSubject: bounce\r\n\r\nfailed\r\n";
    assert_eq!(c.deliver("", "bob@gmail.com", body).await.code, 250);

    let got = down.last().expect("received");
    assert_eq!(got.mail_from.as_deref(), None);
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
    assert_eq!(support::without_received(&got.body), body);
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
