//! §5.1 inbound TLS, per-listener AUTH policy, and the §5.3 sender ACL — D-070
//! and D-071 — end to end against the real listener.
//!
//! Every TLS handshake here is **verified**: the client trusts a CA minted for
//! the test and nothing else, and checks the leaf against `simmer.test`. A test
//! that skipped verification would prove bytes were encrypted, not that the
//! configured certificate was the one served.

mod support;

use std::net::SocketAddr;

use support::{config_for, without_received, Client, FakeDownstream, Script, Simmer, TestPki};

/// argon2id of "local-dev-password", as in `tests/smtp_ingress.rs`.
const HASH: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$iYj0sVhRvzAWM9kzsFBKyg$V1tnVXTGEO+BD0x9q1uccuo91w84jrz+qDKA3qElLFE";

const BODY: &str = "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n";

/// Two users granted the same identity, so D-071's "the ACL never routes" can
/// be tested as byte equality.
fn users() -> String {
    format!(
        "    users:\n\
         \x20     - username: \"cfapp\"\n\
         \x20       password_hash: \"{HASH}\"\n\
         \x20       grants: {{ send_as: [\"oldbrand.com\"] }}\n\
         \x20     - username: \"crm\"\n\
         \x20       password_hash: \"{HASH}\"\n\
         \x20       grants: {{ send_as: [\"oldbrand.com\"] }}"
    )
}

/// A config for a downstream at `down`, with `listeners` (YAML list entries at
/// four spaces) in place of the fixture's single plaintext one, `pki`'s
/// certificate, and plaintext AUTH allowed or not.
fn cfg(down: SocketAddr, pki: &TestPki, listeners: &str, insecure_auth: bool) -> String {
    let base = config_for(down, "");
    let fixture_listener = "  listeners:\n    - address: \"127.0.0.1:0\"\n";
    assert!(base.contains(fixture_listener), "config_for drifted");
    base.replace(
        fixture_listener,
        &format!("  listeners:\n{listeners}{}", pki.yaml()),
    )
    .replace(
        "    allow_insecure_auth: true",
        &format!("    allow_insecure_auth: {insecure_auth}\n{}", users()),
    )
}

async fn stack(listeners: &str, insecure_auth: bool) -> (FakeDownstream, Simmer, TestPki) {
    let pki = TestPki::new(&["simmer.test"]);
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&cfg(down.addr, &pki, listeners, insecure_auth)).await;
    (down, simmer, pki)
}

fn b64(s: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

fn plain(user: &str, password: &str) -> String {
    format!("AUTH PLAIN {}", b64(&format!("\0{user}\0{password}")))
}

async fn login(c: &mut Client, user: &str) {
    let r = c.command(&plain(user, "local-dev-password")).await;
    assert_eq!(r.code, 235, "AUTH as {user}: {r:?}");
}

const IMPLICIT: &str = "    - { address: \"127.0.0.1:0\", tls: implicit, auth: required }\n";
const STARTTLS_OPTIONAL: &str =
    "    - { address: \"127.0.0.1:0\", tls: starttls, auth: optional }\n";
const STARTTLS_REQUIRED: &str =
    "    - { address: \"127.0.0.1:0\", tls: starttls_required, auth: required }\n";

// ---------------------------------------------------------------------------
// RFC 8314 implicit TLS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn implicit_tls_completes_before_the_banner_and_carries_a_message() {
    let (down, simmer, pki) = stack(IMPLICIT, false).await;
    let mut c = Client::connect_implicit_tls(simmer.addr, &pki).await;
    assert!(c.is_encrypted());

    let r = c.hello().await;
    assert_eq!(r.code, 250);
    // RFC 8314 §3.3: never on an implicit-TLS port — the session already is.
    assert!(!r.contains("STARTTLS"), "{r:?}");
    assert!(r.contains("AUTH PLAIN LOGIN"), "{r:?}");

    login(&mut c, "cfapp").await;
    let r = c
        .deliver("jane@oldbrand.com", "bob@example.net", BODY)
        .await;
    assert_eq!(r.code, 250, "{r:?}");

    // RFC 3848: encrypted and authenticated is `ESMTPSA`.
    let got = String::from_utf8(down.last().expect("delivered").body).unwrap();
    assert!(got.contains(" with ESMTPSA id "), "{got}");
}

#[tokio::test]
async fn implicit_tls_refuses_a_client_that_does_not_trust_the_certificate() {
    // The other CA's client is the stand-in for "somebody else's certificate":
    // the handshake must fail on the client's side, which proves the served
    // certificate is the configured one and not merely *a* certificate.
    let (_d, simmer, _pki) = stack(IMPLICIT, false).await;
    let stranger = TestPki::new(&["simmer.test"]);
    let e = Client::implicit_tls_error(simmer.addr, &stranger).await;
    assert!(
        e.to_string().to_ascii_lowercase().contains("certificate"),
        "{e}"
    );
}

#[tokio::test]
async fn a_plaintext_client_on_the_implicit_port_gets_no_plaintext_reply() {
    // No 220 and no 554 in cleartext: the first bytes on this port belong to a
    // TLS handshake, and a plaintext client's EHLO is not one. What does come
    // back is at most a TLS alert record (content type 0x15), then the close.
    use tokio::io::AsyncReadExt;
    let (_d, simmer, _pki) = stack(IMPLICIT, false).await;
    let mut tcp = tokio::net::TcpStream::connect(simmer.addr).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut tcp, b"EHLO client.test\r\n")
        .await
        .unwrap();
    let mut got = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), tcp.read_to_end(&mut got))
        .await
        .expect("the server neither answered nor closed")
        .expect("read");
    assert!(
        got.is_empty() || got[0] == 0x15,
        "expected nothing or a TLS alert, got {:?}",
        String::from_utf8_lossy(&got)
    );
}

// ---------------------------------------------------------------------------
// RFC 3207 STARTTLS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn starttls_is_advertised_until_the_handshake_and_never_after() {
    let (_d, simmer, pki) = stack(STARTTLS_OPTIONAL, false).await;
    let mut c = simmer.connect().await;
    let r = c.hello().await;
    assert!(r.contains("STARTTLS"), "{r:?}");

    let r = c.starttls(&pki).await;
    assert_eq!(r.code, 220, "{r:?}");
    assert!(c.is_encrypted());

    // RFC 3207 §4.2: the client re-issues EHLO, and a second STARTTLS offer
    // would invite a loop.
    let r = c.command("EHLO client.test").await;
    assert_eq!(r.code, 250);
    assert!(!r.contains("STARTTLS"), "{r:?}");

    let r = c.command("STARTTLS").await;
    assert_eq!(r.code, 503, "{r:?}");
    assert!(r.contains("already active"), "{r:?}");
}

#[tokio::test]
async fn a_client_may_decline_starttls_on_a_starttls_listener() {
    // `starttls` offers; `starttls_required` insists. Here an unauthenticated
    // plaintext client is still served, as it would have been before D-070.
    let (down, simmer, _pki) = stack(STARTTLS_OPTIONAL, false).await;
    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c
        .deliver("jane@oldbrand.com", "bob@example.net", BODY)
        .await;
    assert_eq!(r.code, 250, "{r:?}");
    let got = String::from_utf8(down.last().unwrap().body).unwrap();
    assert!(got.contains(" with ESMTP id "), "{got}");
}

#[tokio::test]
async fn the_handshake_discards_the_greeting_and_the_authentication() {
    // RFC 3207 §4.2: "the server MUST discard any knowledge obtained from the
    // client ... which was not obtained from the TLS negotiation itself".
    let listeners = "    - { address: \"127.0.0.1:0\", tls: starttls, auth: required }\n";
    let (_d, simmer, pki) = stack(listeners, true).await;
    let mut c = simmer.connect().await;
    c.hello().await;
    login(&mut c, "cfapp").await;

    assert_eq!(c.starttls(&pki).await.code, 220);

    // The greeting is gone...
    let r = c.command("MAIL FROM:<jane@oldbrand.com>").await;
    assert_eq!(r.code, 503, "{r:?}");
    // ...and so is the authentication.
    c.command("EHLO client.test").await;
    let r = c.command("MAIL FROM:<jane@oldbrand.com>").await;
    assert_eq!(r.code, 530, "{r:?}");
    // Logging in again over the encrypted channel works.
    login(&mut c, "cfapp").await;
    let r = c.command("MAIL FROM:<jane@oldbrand.com>").await;
    assert_eq!(r.code, 250, "{r:?}");
}

#[tokio::test]
async fn the_failed_auth_budget_survives_the_handshake() {
    // §5.3's three strikes are per connection. If the handshake reset them, an
    // attacker could buy unlimited guesses at one round trip per two.
    let (_d, simmer, pki) = stack(STARTTLS_OPTIONAL, true).await;
    let mut c = simmer.connect().await;
    c.hello().await;
    for _ in 0..2 {
        assert_eq!(c.command(&plain("cfapp", "wrong")).await.code, 535);
    }

    assert_eq!(c.starttls(&pki).await.code, 220);
    c.command("EHLO client.test").await;

    let r = c.command(&plain("cfapp", "wrong")).await;
    assert_eq!(r.code, 421, "the third failure must end the session: {r:?}");
    assert!(c.is_closed().await);
}

#[tokio::test]
async fn plaintext_pipelined_behind_starttls_drops_the_connection() {
    // The CVE-2011-0411 shape: a command riding in the same packet as STARTTLS
    // was sent in cleartext by whoever is on the wire, and would otherwise be
    // read as though it arrived encrypted. No 220, no reply at all — dropped.
    let (_d, simmer, _pki) = stack(STARTTLS_OPTIONAL, false).await;
    let mut c = simmer.connect().await;
    c.hello().await;
    c.send_raw(b"STARTTLS\r\nMAIL FROM:<attacker@oldbrand.com>\r\n")
        .await;
    assert!(
        c.is_closed().await,
        "the server said something after a pipelined STARTTLS"
    );
}

#[tokio::test]
async fn starttls_is_refused_where_it_is_not_offered_or_not_well_formed() {
    // A plaintext listener recognises the verb and refuses it: 502, as for BDAT,
    // so the client carries on without it.
    let (_d, simmer, _pki) = stack("    - { address: \"127.0.0.1:0\", tls: off }\n", false).await;
    let mut c = simmer.connect().await;
    let r = c.hello().await;
    assert!(!r.contains("STARTTLS"), "{r:?}");
    assert_eq!(c.command("STARTTLS").await.code, 502);

    let (_d, simmer, _pki) = stack(STARTTLS_OPTIONAL, false).await;
    let mut c = simmer.connect().await;
    c.hello().await;
    // RFC 3207 §4: a parameter is a syntax error.
    assert_eq!(c.command("STARTTLS please").await.code, 501);
    // Not mid-transaction.
    c.command("MAIL FROM:<jane@oldbrand.com>").await;
    assert_eq!(c.command("STARTTLS").await.code, 503);
}

#[tokio::test]
async fn starttls_required_serves_nothing_but_the_way_in_until_the_handshake() {
    let (down, simmer, pki) = stack(STARTTLS_REQUIRED, true).await;
    let mut c = simmer.connect().await;
    let r = c.hello().await;
    assert!(r.contains("STARTTLS"), "{r:?}");
    // Advertising AUTH here would invite a password in clear only to refuse it.
    assert!(!r.contains("AUTH"), "{r:?}");

    // RFC 3207 §4: 530 for everything but EHLO, NOOP, STARTTLS and QUIT — and
    // RSET, which is harmless.
    for cmd in [
        "MAIL FROM:<jane@oldbrand.com>",
        "RCPT TO:<bob@example.net>",
        "DATA",
        "VRFY bob",
        "HELO client.test",
        &plain("cfapp", "local-dev-password"),
    ] {
        let r = c.command(cmd).await;
        assert_eq!(r.code, 530, "{cmd}: {r:?}");
        assert!(r.contains("STARTTLS"), "{cmd}: {r:?}");
    }
    assert_eq!(c.command("NOOP").await.code, 250);
    assert_eq!(c.command("RSET").await.code, 250);

    assert_eq!(c.starttls(&pki).await.code, 220);
    let r = c.command("EHLO client.test").await;
    assert!(r.contains("AUTH PLAIN LOGIN"), "{r:?}");
    login(&mut c, "cfapp").await;
    let r = c
        .deliver("jane@oldbrand.com", "bob@example.net", BODY)
        .await;
    assert_eq!(r.code, 250, "{r:?}");
    let got = String::from_utf8(down.last().unwrap().body).unwrap();
    assert!(got.contains(" with ESMTPSA id "), "{got}");
}

// ---------------------------------------------------------------------------
// Per-listener AUTH policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plaintext_auth_is_refused_with_538_unless_allowed() {
    // D-070 inverts §4.2's old rule: allow_insecure_auth now defaults false and
    // means what it says. The reply is 538, which tells the client encryption is
    // what is missing — 530 would say it had not authenticated.
    let (_d, simmer, pki) = stack(STARTTLS_OPTIONAL, false).await;
    let mut c = simmer.connect().await;
    let r = c.hello().await;
    assert!(!r.contains("AUTH"), "AUTH advertised over plaintext: {r:?}");
    let r = c.command(&plain("cfapp", "local-dev-password")).await;
    assert_eq!(r.code, 538, "{r:?}");

    assert_eq!(c.starttls(&pki).await.code, 220);
    let r = c.command("EHLO client.test").await;
    assert!(r.contains("AUTH PLAIN LOGIN"), "{r:?}");
    login(&mut c, "cfapp").await;
}

#[tokio::test]
async fn a_538_does_not_count_as_a_failed_password() {
    // Nobody's credentials were checked, so §5.3's budget is untouched: a
    // client that tried AUTH before STARTTLS three times is not disconnected.
    let (_d, simmer, _pki) = stack(STARTTLS_OPTIONAL, false).await;
    let mut c = simmer.connect().await;
    c.hello().await;
    for _ in 0..4 {
        assert_eq!(c.command(&plain("cfapp", "x")).await.code, 538);
    }
    assert_eq!(c.command("NOOP").await.code, 250);
}

#[tokio::test]
async fn auth_disabled_neither_advertises_nor_accepts_auth() {
    let (_d, simmer, _pki) =
        stack("    - { address: \"127.0.0.1:0\", auth: disabled }\n", true).await;
    let mut c = simmer.connect().await;
    let r = c.hello().await;
    assert!(!r.contains("AUTH"), "{r:?}");
    let r = c.command(&plain("cfapp", "local-dev-password")).await;
    assert_eq!(r.code, 503, "{r:?}");
    assert!(r.contains("not available"), "{r:?}");
}

#[tokio::test]
async fn listeners_share_one_session_bound() {
    // §5.1's max_concurrent_sessions is about the process, not the port: a
    // session held on one listener counts against the other.
    let pki = TestPki::new(&["simmer.test"]);
    let down = FakeDownstream::start(Script::default()).await;
    let yaml = cfg(
        down.addr,
        &pki,
        &format!("{STARTTLS_OPTIONAL}    - {{ address: \"127.0.0.1:0\", tls: off }}\n"),
        true,
    )
    .replace("max_concurrent_sessions: 16", "max_concurrent_sessions: 1");
    let simmer = Simmer::start(&yaml).await;
    assert_eq!(simmer.addrs.len(), 2);

    let mut first = Client::connect(simmer.addrs[0]).await;
    first.hello().await;

    let mut second = Client::connect(simmer.addrs[1]).await;
    let r = second.read_reply().await;
    assert_eq!(r.code, 421, "{r:?}");
}

// ---------------------------------------------------------------------------
// The §5.3 sender ACL (D-071)
// ---------------------------------------------------------------------------

async fn authenticated(simmer: &Simmer, pki: &TestPki, user: &str) -> Client {
    let mut c = Client::connect_implicit_tls(simmer.addr, pki).await;
    c.hello().await;
    login(&mut c, user).await;
    c
}

#[tokio::test]
async fn an_envelope_sender_outside_the_grant_is_refused_at_mail_from() {
    let (down, simmer, pki) = stack(IMPLICIT, false).await;
    let mut c = authenticated(&simmer, &pki, "cfapp").await;
    let r = c.command("MAIL FROM:<sales@newbrand.com>").await;
    assert_eq!(r.code, 550, "{r:?}");
    assert!(r.contains("5.7.1 sender not permitted"), "{r:?}");
    // Refused before any body crossed, and the session is still usable.
    let r = c
        .deliver("jane@oldbrand.com", "bob@example.net", BODY)
        .await;
    assert_eq!(r.code, 250, "{r:?}");
    assert_eq!(down.messages().len(), 1);
}

#[tokio::test]
async fn a_from_header_outside_the_grant_is_refused_at_the_final_dot() {
    // The case the ACL exists for: an envelope inside the grant, and a From:
    // somebody else's. Checked where From: first exists.
    let (down, simmer, pki) = stack(IMPLICIT, false).await;
    let mut c = authenticated(&simmer, &pki, "cfapp").await;
    let body = "From: ceo@newbrand.com\r\nSubject: wire the money\r\n\r\nnow\r\n";
    let r = c
        .deliver("jane@oldbrand.com", "bob@example.net", body)
        .await;
    assert_eq!(r.code, 550, "{r:?}");
    assert!(r.contains("sender not permitted"), "{r:?}");
    assert!(
        down.messages().is_empty(),
        "the message reached the downstream"
    );
}

#[tokio::test]
async fn a_message_with_no_from_header_is_refused_for_an_authenticated_user() {
    // Default deny: an identity that cannot be found is not inside the grant.
    let (down, simmer, pki) = stack(IMPLICIT, false).await;
    let mut c = authenticated(&simmer, &pki, "cfapp").await;
    let r = c
        .deliver(
            "jane@oldbrand.com",
            "bob@example.net",
            "Subject: no from\r\n\r\nx\r\n",
        )
        .await;
    assert_eq!(r.code, 550, "{r:?}");
    assert!(down.messages().is_empty());
}

#[tokio::test]
async fn the_null_sender_passes_mail_from_and_is_judged_by_its_from_header() {
    // A bounce has no envelope identity to grant, so MAIL FROM:<> is accepted;
    // its From: is still checked at the dot.
    let (down, simmer, pki) = stack(IMPLICIT, false).await;
    let mut c = authenticated(&simmer, &pki, "cfapp").await;
    let r = c.deliver("", "bob@example.net", BODY).await;
    assert_eq!(r.code, 250, "{r:?}");
    assert_eq!(down.messages().len(), 1);

    let body = "From: postmaster@newbrand.com\r\nSubject: bounce\r\n\r\nx\r\n";
    let r = c.deliver("", "bob@example.net", body).await;
    assert_eq!(r.code, 550, "{r:?}");
    assert_eq!(down.messages().len(), 1);
}

#[tokio::test]
async fn an_unauthenticated_session_is_not_subject_to_the_acl() {
    // D-071's stated limit: grants gate *authenticated* sessions. On an
    // `auth: optional` listener a client that never authenticates may present
    // any identity allowed_cidrs lets through — the pre-ACL trust model, and
    // the reason §4.2 warns about `optional` once users exist.
    let (down, simmer, _pki) = stack(STARTTLS_OPTIONAL, false).await;
    let mut c = simmer.connect().await;
    c.hello().await;
    let body = "From: ceo@newbrand.com\r\nSubject: hi\r\n\r\nx\r\n";
    let r = c.deliver("ceo@newbrand.com", "bob@example.net", body).await;
    assert_eq!(r.code, 250, "{r:?}");
    assert_eq!(down.messages().len(), 1);
}

#[tokio::test]
async fn two_users_granted_one_identity_produce_byte_identical_output() {
    // §5.3 and §1.1 together (D-071): the ACL gates acceptance and never
    // routing, so *who* authenticated cannot change a byte of what is sent.
    // If it could, the outbound identity would depend on something no
    // application-side configuration can express.
    let (down, simmer, pki) = stack(IMPLICIT, false).await;
    for user in ["cfapp", "crm"] {
        let mut c = authenticated(&simmer, &pki, user).await;
        let r = c
            .deliver("jane@oldbrand.com", "bob@example.net", BODY)
            .await;
        assert_eq!(r.code, 250, "{user}: {r:?}");
    }

    let got = down.messages();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].mail_from, got[1].mail_from);
    assert_eq!(got[0].recipients, got[1].recipients);
    assert_eq!(
        without_received(&got[0].body),
        without_received(&got[1].body),
        "the body differs by who authenticated"
    );
    // The Received: line differs only in its id and date — never a username.
    for m in &got {
        let raw = String::from_utf8(m.body.clone()).unwrap();
        let received = raw.lines().next().unwrap();
        assert!(received.contains(" with ESMTPSA id "), "{received}");
        assert!(
            !received.contains("cfapp") && !received.contains("crm"),
            "{received}"
        );
    }
}
