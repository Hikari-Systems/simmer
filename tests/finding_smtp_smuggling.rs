//! **SMTP smuggling** — an end-of-data marker with a bare line ending must not
//! end DATA, and the message carrying one must not be relayed.
//!
//! `Session::read_data_inner` read lines with `read_until(b'\n')` and stripped
//! an optional CR, so `<LF>.<LF>` ended DATA. A sender that honours only
//! `CRLF.CRLF` writes that sequence *inside* one message; Simmer read everything
//! after it as new commands and relayed a second transaction, with recipients the
//! client's envelope never named, through the route's warming identity.
//!
//! Each case below sends the smuggled envelope and then checks two things: the
//! downstream received nothing, and the session is still in step — a fresh,
//! ordinary transaction on the same connection goes through. The second is what
//! proves the smuggled `MAIL FROM` was consumed as data rather than executed: had
//! it run, the next `MAIL FROM` would be `503` and `deliver` would fail.

mod support;

use support::{config_for, FakeDownstream, Script, Simmer};

const HEAD: &str = "From: app@example.com\r\nTo: victim@example.org\r\nSubject: one\r\n\r\nhello";

/// A second envelope, pipelined as if the first message had ended.
const SMUGGLED: &str = "MAIL FROM:<app@example.com>\r\n\
                        RCPT TO:<other@example.net>\r\n\
                        DATA\r\n\
                        From: app@example.com\r\n\
                        Subject: smuggled\r\n\
                        \r\n\
                        smuggled body";

async fn stack() -> (FakeDownstream, Simmer) {
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config_for(down.addr, "")).await;
    (down, simmer)
}

/// Send `HEAD`, the bare terminator `marker`, `SMUGGLED` and a real terminator,
/// then assert the whole thing was refused once and the session survived.
async fn refused_whole(marker: &str) {
    let (down, simmer) = stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;

    assert_eq!(c.command("MAIL FROM:<app@example.com>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<victim@example.org>").await.code, 250);
    assert_eq!(c.command("DATA").await.code, 354);

    let payload = format!("{HEAD}{marker}{SMUGGLED}\r\n.\r\n");
    c.send_raw(payload.as_bytes()).await;

    let r = c.read_reply().await;
    assert_eq!(r.code, 554, "marker {marker:?}: {r:?}");
    assert!(r.contains("5.6.0"), "{r:?}");
    assert!(
        down.messages().is_empty(),
        "marker {marker:?}: nothing may be relayed, got {:?}",
        down.messages()
            .iter()
            .map(|m| m.recipients.clone())
            .collect::<Vec<_>>()
    );

    // Still in step: an ordinary message on the same connection is delivered,
    // and it is the only thing the downstream ever saw.
    let r = c
        .deliver(
            "app@example.com",
            "victim@example.org",
            "From: app@example.com\r\nSubject: two\r\n\r\nfine\r\n",
        )
        .await;
    assert_eq!(r.code, 250, "{r:?}");
    let seen = down.messages();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].recipients, vec!["victim@example.org".to_string()]);
}

#[tokio::test]
async fn lf_dot_lf_is_not_a_terminator() {
    refused_whole("\n.\n").await;
}

#[tokio::test]
async fn lf_dot_crlf_is_not_a_terminator() {
    refused_whole("\n.\r\n").await;
}

#[tokio::test]
async fn crlf_dot_lf_is_not_a_terminator() {
    refused_whole("\r\n.\n").await;
}

/// Simmer never ended DATA on `<CR>.<CR>`, but it forwarded the bytes, and a
/// downstream that honours them would split the message on the way out.
#[tokio::test]
async fn cr_dot_cr_is_not_forwarded() {
    refused_whole("\r.\r").await;
}

/// The same attack spelled with the line's own ending, which is how the published
/// one is spelled. `strip_eol` takes that last CR before the content is scanned,
/// so a scan over the content does not see the marker at all — and the bytes go
/// out re-joined with the CRLF transmission restores, byte-identical to what came
/// in. Scanning the whole line is what closes it (D-095).
#[tokio::test]
async fn cr_dot_crlf_is_not_forwarded() {
    refused_whole("\r.\r\n").await;
}

/// An over-long message that also smuggles is still §5.5's `552` with the
/// connection closed (D-020) — the size is the fact the reader can state
/// reliably, since an over-long line's discarded remainder was never inspected.
/// Nothing is relayed either way. The counter is raised from inside the reader
/// rather than from the reply, so padding a line past the cap does not mute it.
#[tokio::test]
async fn over_long_and_smuggling_is_still_too_large() {
    let (down, simmer) = stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;

    assert_eq!(c.command("MAIL FROM:<app@example.com>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<victim@example.org>").await.code, 250);
    assert_eq!(c.command("DATA").await.code, 354);

    // `config_for` sets max_message_bytes: 100000.
    let padding = "x".repeat(120_000);
    let payload = format!("{HEAD}\n.\n{SMUGGLED}\r\n{padding}\r\n.\r\n");
    c.send_raw(payload.as_bytes()).await;

    let r = c.read_reply().await;
    assert_eq!(r.code, 552, "{r:?}");
    assert!(down.messages().is_empty(), "nothing may be relayed");
    assert!(c.is_closed().await, "552 closes the connection (D-020)");
}

/// The guard for §8.1's normalisation: bare LFs *inside* a body that ends with a
/// proper `CRLF.CRLF` are still accepted and promoted to CRLF, exactly as before.
#[tokio::test]
async fn bare_lf_in_the_body_is_still_normalised() {
    let (down, simmer) = stack().await;
    let mut c = simmer.connect().await;
    c.hello().await;

    assert_eq!(c.command("MAIL FROM:<app@example.com>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<victim@example.org>").await.code, 250);
    assert_eq!(c.command("DATA").await.code, 354);
    c.send_raw(b"From: app@example.com\nSubject: lf\n\nline one\nline two\r\n.\r\n")
        .await;

    let r = c.read_reply().await;
    assert_eq!(r.code, 250, "{r:?}");
    let body = support::without_received(&down.last().expect("delivered").body);
    assert!(body.contains("line one\r\nline two\r\n"), "{body:?}");
    assert!(!body.contains("one\nline"), "a bare LF survived: {body:?}");
}
