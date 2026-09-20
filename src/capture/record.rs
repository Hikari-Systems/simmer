//! The JSONL line: one self-contained JSON object per received message.
//!
//! ## What a record is, and what it structurally cannot be
//!
//! A record describes **what a client sent**, and nothing else. It is built
//! before the route walk and before the downstream conversation, so at the
//! moment it exists there is no route, no domain group, no day index, no
//! rewritten identity and no reply — and therefore no field for any of them.
//!
//! That is not tidiness, it is the property that keeps §2.2 true. A spool's
//! minimum schema is "the message, and what we still owe it": an outcome, an
//! attempt count, a next-retry instant, a completion flag. This format cannot
//! express the second half, so nothing can mistake a capture directory for a
//! queue and no later change can quietly turn it into one.
//! [`tests::the_field_set_is_exactly_the_schema`] asserts that mechanically.
//!
//! The outcome is not lost, it is elsewhere: [`Record::id`] is the session's
//! `correlation_id`, which §9.5 puts on every log line, including the one
//! carrying the downstream reply code. Joining the two is one `jq`.
//!
//! ## What is deliberately absent
//!
//! - **The AUTH exchange, in any form** — no password, no base64 blob, no
//!   mechanism, no failed attempt. Only the username that resulted. The one
//!   credential Simmer handles itself never lands in a file it writes, which is
//!   `hash_password`'s stdin rule generalised.
//! - **Anything Simmer derived** — the `Received:` header it would add, the
//!   rewritten body, the §7.3 recipient hash. The plaintext address is already
//!   in the record; adding its hash would only correlate the file to the
//!   database.
//!
//! ## What `body_b64` decodes to
//!
//! The §8.1 buffer's contents: the **unstuffed, CRLF-normalised** message.
//! Transparency dots are already removed and a bare `LF` has already been
//! promoted to `CRLF`, so this is the canonical RFC 5322 message rather than the
//! literal bytes that crossed the socket. That is the right level — it is
//! exactly what the rewrite engine parsed and what the downstream would have
//! received — but it means a replay of a message sent with bare `LF` line
//! endings sends `CRLF`. Said here because "byte-identical replay" would
//! otherwise be read as a wire-level claim it does not make.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The schema version. A reader refuses a version it does not know rather than
/// guessing which fields still mean what they used to.
pub const VERSION: u16 = 1;

/// How much of a `Subject:` is kept.
///
/// A header line may be 65 536 bytes (`session::MAX_DATA_LINE`), and a subject
/// that long as the fourth field would defeat the entire reason it is the fourth
/// field. Truncation is marked with a `…` so a reader can tell a cut subject
/// from a short one; nothing reads the field back, so losing the tail costs
/// nothing.
pub const MAX_SUBJECT_CHARS: usize = 200;

/// One received message, as the client presented it.
///
/// **Field order is the serialised order**, and it is chosen for the human
/// reading the file rather than for the machine parsing it. The first four are
/// *when, to whom, from whom, about what* — so `cut -c1-160`, a terminal window
/// or an editor with wrapping off shows the four things that identify a message
/// to a person before any of the machinery. JSON object order carries no meaning
/// to a parser, so nothing downstream depends on it; `serde_json` preserves
/// declaration order, which is why the intent is expressed here rather than in a
/// formatter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// When the final dot was read. This buckets the record and it is what a
    /// replay's `--from`/`--to` filter on.
    pub at: DateTime<Utc>,
    /// D-047 pins this at one, but the wire form is a list and the record keeps
    /// the wire form.
    pub rcpt_to: Vec<String>,
    /// The envelope sender. `Some("")` is the null sender `<>`; `None` means no
    /// address was given at all.
    pub mail_from: Option<String>,
    /// The `Subject:` header, RFC 2047-decoded and truncated to
    /// [`MAX_SUBJECT_CHARS`] — a **label for a human**, not part of the
    /// transaction.
    ///
    /// It is the one field here that is a projection of the body rather than
    /// something the transaction itself carried, and it earns that twice over:
    /// it is what makes a bucket file scannable by eye, and when `body_omitted`
    /// is true it is the only human handle the record has left.
    ///
    /// **Empty, never null**, when the message carried no `Subject:` or none
    /// that parsed. The four leading fields are what a person reads the file
    /// for, and `""` keeps that row of four uniform — `jq -r .subject` and a
    /// column-aligned dump both behave, where a `null` in the middle of the
    /// preview does not.
    ///
    /// Because it is decoded and truncated it is **not** byte-faithful, and
    /// nothing ever reads it back: a replay sends `body_b64` and ignores this
    /// entirely.
    pub subject: String,
    /// [`VERSION`].
    pub v: u16,
    /// The session's `correlation_id` — a uuid v4, re-minted at every
    /// `MAIL FROM`, and the join key to the log stream.
    pub id: String,
    /// `ip:port`. The port is kept: it is what distinguishes two sessions from
    /// one application host in the same millisecond.
    pub peer: String,
    /// The `EHLO`/`HELO` name the client gave.
    pub helo: String,
    /// Whether the session was encrypted when the message was received.
    pub tls: bool,
    /// The authenticated username, or `None` on an unauthenticated session.
    /// Never a credential.
    pub auth_user: Option<String>,
    /// The ESMTP parameters on `MAIL FROM`. Not cosmetic: a replay that does not
    /// re-present `SMTPUTF8` and `BODY=8BITMIME` is not replaying the same
    /// transaction.
    pub params: Params,
    /// Length of the raw body in bytes, before base64. Present even when the
    /// body itself is not.
    pub size: u64,
    /// Lowercase hex SHA-256 over the raw body. Present even when the body is
    /// not, so an omitted record still identifies its message.
    pub sha256: String,
    /// True when the body was over `capture.max_body_bytes` and was not stored.
    pub body_omitted: bool,
    /// Standard base64, no line breaks. Absent exactly when `body_omitted`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_b64: Option<String>,
}

/// The `MAIL FROM` extension parameters (`src/smtp/command.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Params {
    /// What the client declared, not what arrived — `size` above is what arrived.
    pub size: Option<u64>,
    pub body_8bitmime: bool,
    pub smtputf8: bool,
    /// RFC 4954's `AUTH=` parameter. Parsed and ignored by the session; kept
    /// here because it is something the client said.
    pub auth_identity: Option<String>,
}

/// Everything needed to build a [`Record`], borrowed from the session.
///
/// A separate struct so the call site in `session.rs` reads as a list of facts
/// rather than a dozen positional arguments.
pub struct Ingress<'a> {
    pub correlation_id: &'a str,
    pub at: DateTime<Utc>,
    pub peer: std::net::SocketAddr,
    pub helo: &'a str,
    pub tls: bool,
    pub auth_user: Option<&'a str>,
    pub mail_from: Option<&'a str>,
    pub rcpt_to: &'a [String],
    /// Already decoded and truncated — see [`subject_from_headers`]. Empty when
    /// there was none.
    pub subject: String,
    pub params: Params,
    /// The §8.1 buffer's contents.
    pub body: &'a [u8],
}

/// The `Subject:` from a header block, decoded and truncated for a human.
///
/// **Empty string when there is none**, rather than an `Option`: a message with
/// no subject and a message with an empty one are the same thing to the person
/// scanning the file, and collapsing them keeps the field a plain string
/// everywhere it is read.
///
/// `mail_parser` is already how `session::first_from_address` reads the same
/// block, and it handles RFC 2047 (`=?utf-8?B?…?=`) and folding for us — which
/// is the point, since an encoded-word subject is unreadable and an unreadable
/// subject is not worth putting fourth.
///
/// Takes the **header block**, never the whole message: a spilled 25 MiB body
/// must not be parsed to answer this.
pub fn subject_from_headers(header_block: &[u8]) -> String {
    let Some(parsed) = mail_parser::MessageParser::default().parse_headers(header_block) else {
        return String::new();
    };
    match parsed.subject() {
        None => String::new(),
        Some(subject) => truncate(subject.trim(), MAX_SUBJECT_CHARS),
    }
}

/// At most `max` characters, marked with `…` when anything was dropped.
///
/// Characters rather than bytes, and `char_indices` rather than slicing, so a
/// subject that is mostly CJK or emoji is cut on a boundary rather than
/// panicking.
fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((byte, _)) => {
            let mut out = String::with_capacity(byte + 3);
            out.push_str(&s[..byte]);
            out.push('\u{2026}');
            out
        }
    }
}

impl Record {
    /// Build a record, omitting the body if it is over `max_body_bytes`.
    ///
    /// The digest is taken over the real body either way, so an omitted record
    /// can still be matched against a message held somewhere else.
    pub fn build(i: Ingress<'_>, max_body_bytes: u64) -> Record {
        let omit = i.body.len() as u64 > max_body_bytes;
        Record {
            v: VERSION,
            id: i.correlation_id.to_string(),
            at: i.at,
            peer: i.peer.to_string(),
            helo: i.helo.to_string(),
            tls: i.tls,
            auth_user: i.auth_user.map(str::to_string),
            mail_from: i.mail_from.map(str::to_string),
            rcpt_to: i.rcpt_to.to_vec(),
            subject: i.subject,
            params: i.params,
            size: i.body.len() as u64,
            sha256: hex_digest(i.body),
            body_omitted: omit,
            body_b64: (!omit).then(|| B64.encode(i.body)),
        }
    }

    /// The raw body, or `None` when it was omitted.
    ///
    /// `Err` on a line whose base64 does not decode — a truncated file, or one
    /// edited by hand. The caller counts it and moves on; one unreadable record
    /// must not end a replay.
    pub fn body(&self) -> Result<Option<Vec<u8>>, String> {
        match &self.body_b64 {
            None => Ok(None),
            Some(b64) => B64
                .decode(b64)
                .map(Some)
                .map_err(|e| format!("body_b64 does not decode: {e}")),
        }
    }

    /// The serialised line, including its terminating newline.
    ///
    /// Infallible in practice: every field is a plain scalar, a `String` or a
    /// `Vec<String>`, none of which `serde_json` can fail on. A failure would
    /// still not be worth failing a message over, so it surfaces as an error the
    /// caller counts.
    pub fn to_line(&self) -> Result<Vec<u8>, String> {
        let mut line = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        line.push(b'\n');
        Ok(line)
    }

    /// Parse one line, refusing a version this build does not understand.
    pub fn from_line(line: &[u8]) -> Result<Record, ParseError> {
        // The version is read first, from a shape that ignores everything else,
        // so a future v2 with a changed field type is refused as a version
        // mismatch rather than reported as a type error in some field.
        #[derive(Deserialize)]
        struct JustTheVersion {
            v: u16,
        }
        let probe: JustTheVersion =
            serde_json::from_slice(line).map_err(|e| ParseError::Malformed(e.to_string()))?;
        if probe.v != VERSION {
            return Err(ParseError::Version(probe.v));
        }
        serde_json::from_slice(line).map_err(|e| ParseError::Malformed(e.to_string()))
    }
}

/// Why a captured line could not be read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Written by a build with a different schema.
    Version(u16),
    /// Not JSON, or not this shape. A file truncated by a crash ends this way.
    Malformed(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version(v) => write!(
                f,
                "schema version {v}, but this build understands {VERSION}"
            ),
            Self::Malformed(e) => write!(f, "malformed record: {e}"),
        }
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params {
            size: Some(11),
            body_8bitmime: true,
            smtputf8: false,
            auth_identity: None,
        }
    }

    fn ingress<'a>(body: &'a [u8], rcpt: &'a [String]) -> Ingress<'a> {
        Ingress {
            correlation_id: "7b2f4a6c-1d9e-4b21-9f0a-3c5e8d1a2b44",
            at: DateTime::parse_from_rfc3339("2026-09-20T14:23:07.412Z")
                .unwrap()
                .with_timezone(&Utc),
            peer: "10.0.3.17:52344".parse().unwrap(),
            helo: "app-7.internal",
            tls: true,
            auth_user: Some("marketing"),
            mail_from: Some("news@oldbrand.com"),
            rcpt_to: rcpt,
            subject: "a subject".to_string(),
            params: params(),
            body,
        }
    }

    fn a_record(body: &[u8], max: u64) -> Record {
        let rcpt = vec!["alice@example.com".to_string()];
        Record::build(ingress(body, &rcpt), max)
    }

    /// The load-bearing test in this module: the serialised object's keys are
    /// exactly the schema's. Adding `reply`, `route`, `attempts`, `state` or
    /// `next_retry_at` — the fields that would make this a spool's journal —
    /// breaks it, which is the point.
    #[test]
    fn the_field_set_is_exactly_the_schema() {
        let value: serde_json::Value =
            serde_json::from_slice(&a_record(b"hello world", 1 << 20).to_line().unwrap()).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "at",
                "auth_user",
                "body_b64",
                "body_omitted",
                "helo",
                "id",
                "mail_from",
                "params",
                "peer",
                "rcpt_to",
                "sha256",
                "size",
                "subject",
                "tls",
                "v",
            ]
        );
    }

    /// The file is read by people, and these four are what identify a message to
    /// one. Asserted on the raw line rather than through `serde_json::Value`,
    /// whose map is sorted and would throw the order away.
    #[test]
    fn the_first_four_fields_are_when_to_from_and_about_what() {
        let line = String::from_utf8(a_record(b"hello world", 1 << 20).to_line().unwrap()).unwrap();

        assert!(
            line.starts_with("{\"at\":"),
            "the timestamp must come first: {line}"
        );

        let order: Vec<usize> = ["\"at\":", "\"rcpt_to\":", "\"mail_from\":", "\"subject\":"]
            .iter()
            .map(|k| {
                line.find(k)
                    .unwrap_or_else(|| panic!("{k} missing from {line}"))
            })
            .collect();
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "expected at < rcpt_to < mail_from < subject, got {order:?} in {line}"
        );

        // And all four come before everything else, so a truncated view shows
        // the whole of what identifies the message.
        let after = line.find("\"subject\":").expect("subject");
        for later in [
            "\"v\":",
            "\"id\":",
            "\"peer\":",
            "\"sha256\":",
            "\"body_b64\":",
        ] {
            assert!(
                line.find(later).expect(later) > after,
                "{later} should come after subject: {line}"
            );
        }
    }

    #[test]
    fn the_subject_is_decoded_and_unfolded() {
        // RFC 2047, because an encoded-word subject is unreadable and an
        // unreadable subject is not worth putting fourth.
        let head = b"From: a@b\r\nSubject: =?utf-8?B?Q2Fmw6kgY3LDqG1l?=\r\n\r\n";
        assert_eq!(subject_from_headers(head), "Caf\u{e9} cr\u{e8}me");

        // A folded header is one logical value.
        let folded = b"Subject: a long one\r\n continued here\r\n\r\n";
        assert_eq!(subject_from_headers(folded), "a long one continued here");
    }

    #[test]
    fn a_missing_or_empty_subject_is_an_empty_string_rather_than_null() {
        // A message with no subject and one with an empty subject are the same
        // thing to the person reading the file.
        for head in [
            &b"From: a@b\r\n\r\n"[..],
            b"Subject:\r\n\r\n",
            b"Subject:    \r\n\r\n",
            b"",
        ] {
            assert_eq!(
                subject_from_headers(head),
                "",
                "{:?}",
                String::from_utf8_lossy(head)
            );
        }

        // And it serialises as "" — never null, which would break the uniform
        // row of four the field order exists for.
        let rcpt = vec!["a@b".to_string()];
        let mut i = ingress(b"x", &rcpt);
        i.subject = String::new();
        let line = String::from_utf8(Record::build(i, 1 << 20).to_line().unwrap()).unwrap();
        assert!(line.contains(r#""subject":"""#), "{line}");
        assert!(!line.contains("\"subject\":null"), "{line}");
    }

    #[test]
    fn a_very_long_subject_is_cut_on_a_character_boundary_and_marked() {
        let head = format!("Subject: {}\r\n\r\n", "x".repeat(500));
        let got = subject_from_headers(head.as_bytes());
        assert_eq!(
            got.chars().count(),
            MAX_SUBJECT_CHARS + 1,
            "the cap plus the mark"
        );
        assert!(got.ends_with('\u{2026}'), "a cut subject says so: {got}");

        // Multi-byte throughout: a byte-wise cut would panic or leave mojibake.
        let head = format!("Subject: {}\r\n\r\n", "\u{6f22}".repeat(500));
        let got = subject_from_headers(head.as_bytes());
        assert_eq!(got.chars().count(), MAX_SUBJECT_CHARS + 1);
        assert!(got.starts_with('\u{6f22}'));

        // Exactly at the cap is not truncated.
        let exact = "y".repeat(MAX_SUBJECT_CHARS);
        let head = format!("Subject: {exact}\r\n\r\n");
        assert_eq!(subject_from_headers(head.as_bytes()), exact);
    }

    #[test]
    fn the_subject_survives_an_omitted_body_because_that_is_when_it_matters_most() {
        let rcpt = vec!["a@b".to_string()];
        let mut i = ingress(b"0123456789", &rcpt);
        i.subject = "the only handle left".to_string();
        let r = Record::build(i, 5);
        assert!(r.body_omitted);
        assert_eq!(r.subject, "the only handle left");
    }

    #[test]
    fn a_record_carries_no_outcome_and_no_derived_state() {
        // The same claim as above, said in the vocabulary a future reader would
        // reach for. §2.2 stays true because these have nowhere to go.
        let line = String::from_utf8(a_record(b"hello", 1 << 20).to_line().unwrap()).unwrap();
        for forbidden in [
            "reply",
            "code",
            "route",
            "domain_group",
            "day_index",
            "attempts",
            "state",
            "next_retry",
            "delivered",
            "password",
            "secret",
        ] {
            assert!(
                !line.contains(forbidden),
                "a record must not contain '{forbidden}': {line}"
            );
        }
    }

    #[test]
    fn a_record_round_trips_through_a_line() {
        let r = a_record(b"From: a@b\r\n\r\nbody\r\n", 1 << 20);
        let line = r.to_line().unwrap();
        assert!(line.ends_with(b"\n"));
        assert_eq!(Record::from_line(&line).unwrap(), r);
    }

    #[test]
    fn the_body_survives_base64_byte_for_byte() {
        // Including the three shapes that break a naive text round-trip: an 8-bit
        // non-UTF-8 byte, a NUL, and a line that begins with a dot.
        let body = b"From: a@b\r\n\r\n.leading dot\r\n\xe9\x00\xff\r\n";
        let r = a_record(body, 1 << 20);
        assert_eq!(r.body().unwrap().unwrap(), body.to_vec());
        assert_eq!(r.size, body.len() as u64);
        assert!(!r.body_omitted);
    }

    #[test]
    fn an_empty_body_is_stored_rather_than_omitted() {
        let r = a_record(b"", 1 << 20);
        assert!(!r.body_omitted);
        assert_eq!(r.body().unwrap().unwrap(), Vec::<u8>::new());
        assert_eq!(r.size, 0);
    }

    #[test]
    fn the_digest_is_sha256_of_the_raw_body() {
        // The well-known digest of the empty input, computed independently.
        assert_eq!(
            a_record(b"", 1 << 20).sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // And it is over the raw bytes, not the base64.
        let r = a_record(b"abc", 1 << 20);
        assert_eq!(
            r.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn an_oversized_body_is_omitted_but_still_identified() {
        let body = b"0123456789";
        let r = a_record(body, 9);
        assert!(r.body_omitted);
        assert!(r.body_b64.is_none());
        assert_eq!(r.body().unwrap(), None);
        // size and sha256 are of the real body, so the record still names it.
        assert_eq!(r.size, 10);
        assert_eq!(r.sha256, a_record(body, 1 << 20).sha256);
        // And the absent field is absent, not null.
        let line = String::from_utf8(r.to_line().unwrap()).unwrap();
        assert!(!line.contains("body_b64"), "{line}");
        assert!(line.contains("\"body_omitted\":true"), "{line}");
    }

    #[test]
    fn a_body_of_exactly_the_cap_is_kept() {
        // The boundary is "over", not "at".
        let r = a_record(b"0123456789", 10);
        assert!(!r.body_omitted);
    }

    #[test]
    fn a_future_schema_version_is_refused_rather_than_partially_read() {
        let line = br#"{"v":2,"id":"x","at":"2026-09-20T14:23:07.412Z","something":"else"}"#;
        assert_eq!(Record::from_line(line), Err(ParseError::Version(2)));
    }

    #[test]
    fn a_truncated_or_foreign_line_is_malformed_not_a_panic() {
        for line in [
            &b"{\"v\":1,\"id\":\"x\""[..], // truncated mid-object, as a crash leaves it
            b"",
            b"not json at all",
            b"{}",      // no version
            b"[1,2,3]", // json, wrong shape
        ] {
            assert!(
                matches!(Record::from_line(line), Err(ParseError::Malformed(_))),
                "{:?}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn a_body_whose_base64_does_not_decode_is_an_error_not_a_panic() {
        let mut r = a_record(b"hi", 1 << 20);
        r.body_b64 = Some("!!! not base64 !!!".to_string());
        assert!(r.body().is_err());
    }

    #[test]
    fn the_null_sender_is_distinguishable_from_no_sender_at_all() {
        let rcpt = vec!["a@b".to_string()];
        let mut i = ingress(b"x", &rcpt);
        i.mail_from = Some("");
        assert_eq!(Record::build(i, 1 << 20).mail_from, Some(String::new()));

        let mut i = ingress(b"x", &rcpt);
        i.mail_from = None;
        assert_eq!(Record::build(i, 1 << 20).mail_from, None);
    }

    #[test]
    fn an_unauthenticated_session_records_no_username() {
        let rcpt = vec!["a@b".to_string()];
        let mut i = ingress(b"x", &rcpt);
        i.auth_user = None;
        let line = String::from_utf8(Record::build(i, 1 << 20).to_line().unwrap()).unwrap();
        assert!(line.contains("\"auth_user\":null"), "{line}");
    }

    #[test]
    fn the_esmtp_parameters_are_kept_so_a_replay_can_re_present_them() {
        let rcpt = vec!["a@b".to_string()];
        let mut i = ingress(b"x", &rcpt);
        i.params = Params {
            size: Some(4211),
            body_8bitmime: true,
            smtputf8: true,
            auth_identity: Some("someone@example.com".to_string()),
        };
        let r = Record::build(i, 1 << 20);
        let back = Record::from_line(&r.to_line().unwrap()).unwrap();
        assert_eq!(back.params, r.params);
        assert!(back.params.smtputf8 && back.params.body_8bitmime);
    }
}
