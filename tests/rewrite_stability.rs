//! `SPEC.md` §6.6, as the property test the spec asks for.
//!
//! > Enforced by a property test over generated messages and by startup
//! > validation against a synthetic probe.
//!
//! Startup validation is the probe half and lives in `src/config/validate.rs`.
//! This is the other half, and it exists because the probe is *one* message. A
//! rewrite can be stable against a tidy synthetic message and unstable against a
//! real one — a missing `From:`, a display name that is already an encoded word,
//! two `Message-ID` headers, a body that looks like a header block. Generating
//! messages is how those get found without anyone having thought of them first.
//!
//! ## What "equal" means here
//!
//! §6.6 excludes the volatile template variables from the comparison. They are
//! excluded by being **pinned**: both passes render at one instant with one
//! UUID, which is stronger than skipping the headers that use them (see
//! `rewrite::stability`'s module comment).
//!
//! The one genuine exclusion is `Received:`. §6.1 step 8 prepends one per pass
//! by design, so the second pass carries the first pass's header *and* its own.
//! Removing the topmost from the second pass — and nothing from the first —
//! is what lines the two up; stripping one from each would compare a message
//! that never had a Received: against one that still has one.

use proptest::prelude::*;
use simmer::config::Identity;
use simmer::rewrite::{rewrite, Inbound, Received, Rewritten, RouteRewrite};

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

fn compile(yaml: &str) -> RouteRewrite {
    let identity: Identity = serde_yaml_ng::from_str(yaml).expect("fixture parses");
    RouteRewrite::compile(&identity).expect("fixture compiles")
}

/// One pass, with everything volatile pinned.
fn pass(route: &RouteRewrite, raw: &[u8], envelope_from: Option<&str>) -> Rewritten {
    let recipients = ["bob@example.net".to_string()];
    let uuid = || "00000000-0000-4000-8000-000000000000".to_string();
    rewrite(
        route,
        &Inbound {
            raw,
            envelope_from,
            recipients: &recipients,
            route_name: "warming",
            correlation_id: "fixed-correlation-id",
            received: Received {
                helo: "app.internal",
                peer: "10.1.2.3",
                by: "simmer.test",
                authenticated: true,
            },
            now: chrono::DateTime::from_timestamp(1_767_225_600, 0).expect("valid instant"),
            uuid: &uuid,
        },
    )
}

/// Drop the topmost `Received:` field, continuations included.
fn strip_top_received(raw: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw).to_string();
    if !text.starts_with("Received: ") {
        return raw.to_vec();
    }
    let mut rest = &text[..];
    loop {
        let Some(nl) = rest.find("\r\n") else {
            return Vec::new();
        };
        rest = &rest[nl + 2..];
        // A continuation line belongs to the field we are dropping.
        if !rest.starts_with(' ') && !rest.starts_with('\t') {
            return rest.as_bytes().to_vec();
        }
    }
}

/// `rewrite(rewrite(m)) == rewrite(m)`, with §6.6's exclusions applied.
fn is_stable(route: &RouteRewrite, message: &str) -> Result<(), String> {
    let once = pass(route, message.as_bytes(), Some("sender@oldbrand.com"));
    let twice = pass(route, &once.raw, once.envelope_from.as_deref());

    if once.envelope_from != twice.envelope_from {
        return Err(format!(
            "envelope_from: {:?} then {:?}",
            once.envelope_from, twice.envelope_from
        ));
    }

    // See the module comment: only the second pass's own header comes off.
    let a = once.raw.clone();
    let b = strip_top_received(&twice.raw);
    if a != b {
        return Err(format!(
            "message differs:\n--- first ---\n{}\n--- second ---\n{}",
            String::from_utf8_lossy(&a),
            String::from_utf8_lossy(&b)
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// generators
// ---------------------------------------------------------------------------

/// Display names chosen for the ways they interact with RFC 2047 and with the
/// address grammar, not for variety's sake.
fn display_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("Jane Smith".to_string()),
        // Already an encoded word: pass 2 must not encode it a second time.
        Just("=?UTF-8?B?SsOkbmU=?=".to_string()),
        // Non-ASCII: pass 1 encodes it, pass 2 must recognise its own output.
        Just("Jäne Smith".to_string()),
        Just("Grüße aus München".to_string()),
        // Quoted, with the comma that makes address-list splitting non-trivial.
        Just("\"Smith, Jane\"".to_string()),
        Just("  leading and trailing  ".to_string()),
    ]
}

fn from_header() -> impl Strategy<Value = String> {
    (
        display_name(),
        prop_oneof![
            Just("jane@oldbrand.com"),
            Just("jane@newbrand.com"),
            Just("sales@newbrand.com"),
            Just("\"odd@local\"@oldbrand.com"),
        ],
    )
        .prop_map(|(name, addr)| {
            if name.trim().is_empty() {
                format!("From: {addr}\r\n")
            } else {
                format!("From: {name} <{addr}>\r\n")
            }
        })
}

/// Headers a real message carries, including ones the route also writes.
fn extra_header() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("To: bob@example.net\r\n".to_string()),
        Just("Subject: Your order has shipped\r\n".to_string()),
        Just("Subject: =?UTF-8?Q?Gr=C3=BC=C3=9Fe?=\r\n".to_string()),
        Just("Subject:\r\n".to_string()),
        Just("Message-ID: <abc.123@oldbrand.com>\r\n".to_string()),
        Just("Message-ID: <already@newbrand.com>\r\n".to_string()),
        Just("Reply-To: someone@elsewhere.com\r\n".to_string()),
        Just("Sender: relay@oldbrand.com\r\n".to_string()),
        Just("Return-Path: <bounces@oldbrand.com>\r\n".to_string()),
        Just("DKIM-Signature: v=1; a=rsa-sha256; d=oldbrand.com; b=xyz\r\n".to_string()),
        Just("Authentication-Results: mx.example.com; spf=pass\r\n".to_string()),
        Just("ARC-Seal: i=1; cv=none\r\n".to_string()),
        Just("Received: from upstream by app.internal\r\n".to_string()),
        Just("X-Campaign: spring-2026\r\n".to_string()),
        // Folded, with both flavours of continuation whitespace.
        Just("X-Long: first part\r\n second part\r\n".to_string()),
        Just("X-Tabbed: first\r\n\tsecond\r\n".to_string()),
        Just("MIME-Version: 1.0\r\n".to_string()),
        Just("Content-Type: text/plain; charset=utf-8\r\n".to_string()),
        // Lowercase, no space after the colon: byte-level quirks a normalising
        // re-serialiser would silently repair.
        Just("x-lower: value\r\n".to_string()),
        Just("X-Tight:value\r\n".to_string()),
    ]
}

fn body() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("Hello.\r\n".to_string()),
        // The phase 2 awkward body: a bare dot, a dot-prefixed line, a trailing
        // blank line.
        Just(".\r\n.hidden\r\n..two\r\nplain\r\n\r\n".to_string()),
        // A body that looks like a header block. Nothing below the separator may
        // be parsed as a header.
        Just("From: not-a-header@example.com\r\nSubject: also not\r\n\r\ntext\r\n".to_string()),
        Just("Grüße — non-ASCII in the body\r\n".to_string()),
        Just("line\r\n".repeat(50)),
        // -- §6.4, phase 5 -------------------------------------------------
        // A live match for the shipped rule, so pass 2 sees pass 1's rewritten
        // body rather than the one that arrived.
        Just("Track it at https://oldbrand.com/track\r\n".to_string()),
        // Already migrated: arrangement B of §1.1, in the body.
        Just("Track it at https://newbrand.com/track\r\n".to_string()),
        // The construct §6.4 is written about, and its own example.
        Just("Visit https://old=\r\nbrand.com/x today\r\n".to_string()),
        Just("Grüße =E2=80=94 https://oldbrand.com/x\r\n".to_string()),
        // A part whose bytes encode a match that must not be found.
        Just("aHR0cHM6Ly9vbGRicmFuZC5jb20vdHJhY2s=\r\n".to_string()),
        Just(BASE64_TEXT_BODY.to_string()),
        Just(MULTIPART_BODY.to_string()),
        Just(SIGNED_BODY.to_string()),
    ]
}

/// A `text/plain` part carrying a match, base64 encoded. Paired with
/// `base64_headers()` below; on its own it is just an odd-looking body, which is
/// also worth generating.
const BASE64_TEXT_BODY: &str = "VHJhY2sgaXQgYXQgaHR0cHM6Ly9vbGRicmFuZC5jb20vdHJhY2sNCg==\r\n";

/// One matching text part, one attachment that encodes the same match, a
/// preamble and an epilogue. Everything but the text part must survive.
const MULTIPART_BODY: &str = concat!(
    "preamble\r\n",
    "--b1\r\n",
    "Content-Type: text/plain; charset=utf-8\r\n",
    "Content-Transfer-Encoding: quoted-printable\r\n",
    "\r\n",
    "Track it at https://old=\r\nbrand.com/track\r\n",
    "--b1\r\n",
    "Content-Type: application/pdf\r\n",
    "Content-Disposition: attachment; filename=receipt.pdf\r\n",
    "Content-Transfer-Encoding: base64\r\n",
    "\r\n",
    "aHR0cHM6Ly9vbGRicmFuZC5jb20vdHJhY2s=\r\n",
    "--b1--\r\n",
    "epilogue\r\n",
);

/// §6.4's never-rewrite case, with a live match inside it.
const SIGNED_BODY: &str = concat!(
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

/// `Content-Type` headers that make the generated bodies mean something.
///
/// The generator pairs these freely with the bodies above, so most combinations
/// are a mislabelled message — a `multipart/mixed` header over a plain body, a
/// `base64` label over text that is not base64. That is deliberate: §6.4 has to
/// survive a message whose headers lie about its body, and those are the inputs
/// that reach the decode-failure paths.
fn mime_header() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("Content-Type: text/plain; charset=utf-8\r\n".to_string()),
        Just("Content-Type: text/plain; charset=iso-8859-1\r\n".to_string()),
        Just("Content-Type: text/plain; charset=shift_jis\r\n".to_string()),
        Just("Content-Type: text/html\r\n".to_string()),
        Just("Content-Type: application/octet-stream\r\n".to_string()),
        Just("Content-Type: multipart/mixed; boundary=\"b1\"\r\n".to_string()),
        Just("Content-Type: multipart/signed; boundary=\"sig\"\r\n".to_string()),
        Just("Content-Type: multipart/mixed\r\n".to_string()),
        Just("Content-Transfer-Encoding: quoted-printable\r\n".to_string()),
        Just("Content-Transfer-Encoding: base64\r\n".to_string()),
        Just("Content-Transfer-Encoding: 7bit\r\n".to_string()),
        Just("Content-Transfer-Encoding: x-uuencode\r\n".to_string()),
        Just("Content-Length: 42\r\n".to_string()),
        Just("Lines: 3\r\n".to_string()),
    ]
}

prop_compose! {
    fn message()(
        from in from_header(),
        extras in prop::collection::vec(extra_header(), 0..7),
        mime in prop::collection::vec(mime_header(), 0..3),
        body in body(),
    ) -> String {
        format!("{from}{}{}\r\n{body}", extras.concat(), mime.concat())
    }
}

/// Bodies that contain no match for the shipped rule under **any** decoding.
///
/// Filtering the general generator would not do: `aHR0cHM6…` is a match once
/// base64 is undone, so "the raw text has no match" is not the same question as
/// "the rule finds nothing". These are chosen so that both answers are no.
fn harmless_body() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("Hello.\r\n".to_string()),
        Just(".\r\n.hidden\r\n..two\r\nplain\r\n\r\n".to_string()),
        Just("Grüße — non-ASCII in the body\r\n".to_string()),
        Just("Grüße =E2=80=94 and a soft=20\r\nbreak\r\n".to_string()),
        Just("SGVsbG8sIHdvcmxkCg==\r\n".to_string()),
        Just("Track it at https://newbrand.com/track\r\n".to_string()),
        Just("line\r\n".repeat(50)),
        Just(
            concat!(
                "preamble\r\n",
                "--b1\r\n",
                "Content-Type: text/plain; charset=utf-8\r\n",
                "\r\n",
                "Already migrated: https://newbrand.com/track\r\n",
                "--b1\r\n",
                "Content-Type: application/pdf\r\n",
                "Content-Transfer-Encoding: base64\r\n",
                "\r\n",
                "JVBERi0xLjQK\r\n",
                "--b1--\r\n",
                "epilogue\r\n",
            )
            .to_string()
        ),
    ]
}

prop_compose! {
    fn harmless_message()(
        from in from_header(),
        extras in prop::collection::vec(extra_header(), 0..5),
        mime in prop::collection::vec(mime_header(), 0..3),
        body in harmless_body(),
    ) -> String {
        format!("{from}{}{}\r\n{body}", extras.concat(), mime.concat())
    }
}

// A multipart whose headers describe its body truthfully, so the parts really
// are the parts. `message()` pairs headers and bodies freely on purpose — a
// message whose headers lie about its body is what reaches the decode-failure
// paths — but a claim about which part was rewritten needs a message where the
// question has an answer.
prop_compose! {
    fn well_formed_multipart()(
        from in from_header(),
        extras in prop::collection::vec(extra_header(), 0..5),
    ) -> String {
        // A second `Content-Type` would make the message malformed and the
        // first one wins, which is a different test than this one.
        let extras: String = extras
            .iter()
            .filter(|h| !h.starts_with("Content-Type:"))
            .cloned()
            .collect();
        format!(
            "{from}{extras}MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n{MULTIPART_BODY}",
        )
    }
}

// A message that does not carry a `From:` at all. Kept separate so the common
// generator stays representative rather than being half edge case.
prop_compose! {
    fn message_without_from()(
        extras in prop::collection::vec(extra_header(), 0..5),
        mime in prop::collection::vec(mime_header(), 0..3),
        body in body(),
    ) -> String {
        format!("{}{}\r\n{body}", extras.concat(), mime.concat())
    }
}

// ---------------------------------------------------------------------------
// the properties
// ---------------------------------------------------------------------------

/// The shipped idiom: everything `simmer.yaml` does on the warming route,
/// §6.4's body rewrites included.
const SHIPPED: &str = r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  From: "{{original.from.display_name}} <sales@newbrand.com>"
  Sender: "sales@newbrand.com"
  Message-ID: "<{{uuid}}@newbrand.com>"
  List-Unsubscribe: "<mailto:unsub@newbrand.com>, <https://newbrand.com/u/{{uuid}}>"
  List-Unsubscribe-Post: "List-Unsubscribe=One-Click"
  X-Original-Subject: "{{original.subject}}"
remove_headers: ["Return-Path", "X-Mailer"]
body_rewrites:
  - pattern: 'https://oldbrand\.com/'
    replacement: "https://newbrand.com/"
"#;

/// The migration-only construct, declared as §6.6 requires.
const WITH_REPLY_TO: &str = r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  From: "{{original.from.display_name}} <sales@newbrand.com>"
  Reply-To: "{{original.from.address}}"
unstable_headers: ["Reply-To"]
"#;

/// Pass-through: §1.1's "degenerate case where the target identity already
/// equals the incoming one".
const PASS_THROUGH: &str = r#"envelope_from: "sender@oldbrand.com""#;

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn the_shipped_identity_is_stable_over_generated_messages(m in message()) {
        let route = compile(SHIPPED);
        if let Err(why) = is_stable(&route, &m) {
            return Err(TestCaseError::fail(format!("{why}\n--- input ---\n{m}")));
        }
    }

    #[test]
    fn a_pass_through_route_is_stable_over_generated_messages(m in message()) {
        let route = compile(PASS_THROUGH);
        if let Err(why) = is_stable(&route, &m) {
            return Err(TestCaseError::fail(format!("{why}\n--- input ---\n{m}")));
        }
    }

    #[test]
    fn stability_does_not_depend_on_the_message_having_a_from(m in message_without_from()) {
        // §5.4 would refuse to route this when a rule needs the header, but the
        // rewrite engine must not become unstable on it — `envelope`-only rules
        // reach here with no `From:` at all.
        let route = compile(SHIPPED);
        if let Err(why) = is_stable(&route, &m) {
            return Err(TestCaseError::fail(format!("{why}\n--- input ---\n{m}")));
        }
    }

    #[test]
    fn a_body_no_rule_matches_is_never_altered(m in message()) {
        // What is left of D-039 once §6.4 exists, and the reason `body.rs`
        // returns `None` rather than a rebuilt body: a route whose
        // `body_rewrites` find nothing changes nothing at all — not the MIME
        // boundaries, not the transfer encodings, not the trailing whitespace.
        //
        // A pass-through route has no rules, so nothing can match whatever the
        // generator produced.
        let route = compile(PASS_THROUGH);
        let out = pass(&route, m.as_bytes(), Some("sender@oldbrand.com"));
        let sent_body = m.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
        let got = String::from_utf8_lossy(&out.raw).to_string();
        let got_body = got.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
        prop_assert_eq!(got_body, sent_body);
    }

    #[test]
    fn a_body_the_shipped_rule_does_not_match_is_never_altered(m in harmless_message()) {
        // The same property with rules loaded, which is the case that can go
        // wrong: the engine walks the MIME structure, decides nothing matched,
        // and has to put every span back exactly as it found it.
        let route = compile(SHIPPED);
        let out = pass(&route, m.as_bytes(), Some("sender@oldbrand.com"));
        let sent_body = m.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
        let got = String::from_utf8_lossy(&out.raw).to_string();
        let got_body = got.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
        prop_assert_eq!(got_body, sent_body);
    }

    #[test]
    fn nothing_outside_a_text_part_is_ever_rewritten(m in well_formed_multipart()) {
        // The base64 attachment in `MULTIPART_BODY` encodes the very string the
        // shipped rule matches. §6.4: "Attachments and non-text parts are never
        // touched" — so if it comes back changed, the engine decoded something
        // it had no business decoding.
        let route = compile(SHIPPED);
        let out = pass(&route, m.as_bytes(), Some("sender@oldbrand.com"));
        let got = String::from_utf8_lossy(&out.raw).to_string();
        prop_assert!(
            got.contains("aHR0cHM6Ly9vbGRicmFuZC5jb20vdHJhY2s="),
            "the attachment was rewritten:\n{}",
            got
        );
        // The text part beside it, though, is exactly what the rule is for —
        // soft line break and all.
        prop_assert!(
            got.contains("https://newbrand.com/track"),
            "the text part was not rewritten:\n{}",
            got
        );
        // And the framing is the framing that arrived.
        prop_assert!(got.contains("preamble\r\n--b1\r\n"), "{}", got);
        prop_assert!(got.ends_with("--b1--\r\nepilogue\r\n"), "{}", got);
    }

    #[test]
    fn authentication_artefacts_never_survive(m in message()) {
        // §6.5, unconditionally, whatever the message looks like.
        let route = compile(SHIPPED);
        let out = pass(&route, m.as_bytes(), Some("sender@oldbrand.com"));
        let header_block = String::from_utf8_lossy(&out.raw)
            .split("\r\n\r\n")
            .next()
            .unwrap_or_default()
            .to_string();
        for artefact in ["DKIM-Signature:", "Authentication-Results:", "ARC-Seal:"] {
            prop_assert!(!header_block.contains(artefact), "{artefact} survived in:\n{header_block}");
        }
    }

    #[test]
    fn the_declared_migration_only_route_is_stable_in_everything_else(m in message()) {
        // Reply-To is declared unstable, so it is expected to differ. Nothing
        // else may.
        let route = compile(WITH_REPLY_TO);
        let once = pass(&route, m.as_bytes(), Some("sender@oldbrand.com"));
        let twice = pass(&route, &once.raw, once.envelope_from.as_deref());

        let drop_reply_to = |raw: &[u8]| -> String {
            String::from_utf8_lossy(raw)
                .split("\r\n")
                .filter(|l| !l.starts_with("Reply-To:"))
                .collect::<Vec<_>>()
                .join("\r\n")
        };
        prop_assert_eq!(
            drop_reply_to(&once.raw),
            drop_reply_to(&strip_top_received(&twice.raw))
        );
        prop_assert_eq!(once.envelope_from, twice.envelope_from);
    }
}

// ---------------------------------------------------------------------------
// the negative case — the property has to be able to fail
// ---------------------------------------------------------------------------

#[test]
fn the_migration_only_construct_really_is_unstable() {
    // If this passed, every other test in the file would be vacuous. §6.6's
    // worked example: Reply-To reads From:, which the same pass overwrites, so
    // reconfiguring the application silently changes what the recipient sees.
    let route = compile(WITH_REPLY_TO);
    let message = "From: Jane Smith <jane@oldbrand.com>\r\nTo: bob@example.net\r\n\r\nHello.\r\n";

    let once = pass(&route, message.as_bytes(), Some("sender@oldbrand.com"));
    let twice = pass(&route, &once.raw, once.envelope_from.as_deref());

    let text = |r: &Rewritten| String::from_utf8_lossy(&r.raw).to_string();
    assert!(
        text(&once).contains("Reply-To: jane@oldbrand.com\r\n"),
        "{}",
        text(&once)
    );
    assert!(
        text(&twice).contains("Reply-To: sales@newbrand.com\r\n"),
        "{}",
        text(&twice)
    );
    assert!(
        is_stable(&route, message).is_err(),
        "the harness must be able to detect instability"
    );
}

#[test]
fn a_relative_transformation_is_caught() {
    // §1.1 constraint 1: "Append `.new` to the sending domain is not permitted,
    // because applying it to already-migrated traffic corrupts it." This is the
    // construct SPEC.md's own §4.1 example configuration contains (D-036).
    let route = compile(r#"envelope_from: "bounce+{{original.envelope_from.local}}@newbrand.com""#);
    let message = "From: jane@oldbrand.com\r\n\r\nbody\r\n";

    let once = pass(&route, message.as_bytes(), Some("jane@oldbrand.com"));
    let twice = pass(&route, &once.raw, once.envelope_from.as_deref());

    assert_eq!(
        once.envelope_from.as_deref(),
        Some("bounce+jane@newbrand.com")
    );
    assert_eq!(
        twice.envelope_from.as_deref(),
        Some("bounce+bounce+jane@newbrand.com"),
        "each pass should prepend again — that is what makes it relative"
    );
}

#[test]
fn the_two_arrangements_of_the_cutover_invariant_agree() {
    // §1.1, as far as an in-process test can state it: the same logical message
    // sent before and after the application is reconfigured produces the same
    // bytes, once the declared-unstable headers and Received: are excluded
    // (D-002). The acceptance harness states it against real mail servers.
    let route = compile(SHIPPED);

    // Arrangement A — the app has not been updated yet.
    let a = pass(
        &route,
        b"From: Jane Smith <jane@oldbrand.com>\r\n\
          To: bob@example.net\r\n\
          Subject: Your order\r\n\
          \r\n\
          Hello.\r\n",
        Some("jane@oldbrand.com"),
    );

    // Arrangement B — the app already sends the target identity.
    let b = pass(
        &route,
        b"From: Jane Smith <sales@newbrand.com>\r\n\
          To: bob@example.net\r\n\
          Subject: Your order\r\n\
          \r\n\
          Hello.\r\n",
        Some("bounce@newbrand.com"),
    );

    assert_eq!(
        String::from_utf8_lossy(&strip_top_received(&a.raw)),
        String::from_utf8_lossy(&strip_top_received(&b.raw)),
    );
    assert_eq!(a.envelope_from, b.envelope_from);
}
