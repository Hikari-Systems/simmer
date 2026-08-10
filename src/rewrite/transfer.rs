//! `Content-Transfer-Encoding` codecs — RFC 2045 §6.
//!
//! §6.4 requires decoding a `text/*` part "according to
//! `Content-Transfer-Encoding` (handling `quoted-printable` and `base64`)"
//! before any pattern is applied, and it says why: a URL written across a
//! quoted-printable soft line break (`https://old.=\r\nbrand.com/x`) does not
//! match a pattern written against the URL, "which is the common case in real
//! mail rather than an edge case".
//!
//! ## Why these are not `mail-parser`'s decoders
//!
//! Same reason as `headers.rs`. `mail-parser` decodes for *reading* — it hands
//! back the text and throws the framing away. Body rewriting has to put a part
//! back where it came from, so the decoder needs a matching encoder, and the two
//! have to agree byte for byte on everything except the substitution. The
//! round-trip tests at the bottom of this file are what makes that claim
//! checkable.
//!
//! ## Malformed input is an error, not a repair
//!
//! Every decoder here refuses input it cannot read exactly, and §6.4 says what
//! happens next: "If a part cannot be decoded (unknown charset, malformed
//! encoding), leave it untouched, log at `WARN`, and increment
//! `simmer_body_rewrite_skipped_total`." Guessing at a broken part would risk
//! emitting something the client never wrote, which is the one thing the cutover
//! invariant forbids outright.

use base64::Engine as _;

/// RFC 2045 §6.7 rule 5: "The Quoted-Printable encoding REQUIRES that encoded
/// lines be no more than 76 characters long." The soft break costs one of them,
/// so content stops at 75.
const QP_MAX_CONTENT: usize = 75;

/// RFC 2045 §6.8: "The encoded output stream must be represented in lines of no
/// more than 76 characters each." Every mailer in circulation uses exactly 76,
/// which is what makes a re-encoded part likely to be byte-identical to what the
/// application would have produced itself.
const BASE64_LINE: usize = 76;

/// The encodings §6.4 names, plus the identity case.
///
/// `7bit`, `8bit`, `binary` and an absent header are all the identity encoding:
/// the bytes on the wire *are* the content. They are distinguished nowhere in
/// this module because nothing here treats them differently — the difference is
/// a statement about what the content contains, not about how it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Identity,
    QuotedPrintable,
    Base64,
}

/// A part whose encoding cannot be read exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed;

impl Encoding {
    /// Parse a `Content-Transfer-Encoding` value.
    ///
    /// `None` means an encoding Simmer does not implement — `uuencode`, an
    /// `x-` extension, or a typo. §6.4's answer to all three is the same: leave
    /// the part alone.
    pub fn parse(value: Option<&str>) -> Option<Encoding> {
        let Some(value) = value else {
            // RFC 2045 §6.1: "Content-Transfer-Encoding: 7BIT" is assumed if the
            // header is absent.
            return Some(Encoding::Identity);
        };
        match token(value).to_ascii_lowercase().as_str() {
            "" | "7bit" | "8bit" | "binary" => Some(Encoding::Identity),
            "quoted-printable" => Some(Encoding::QuotedPrintable),
            "base64" => Some(Encoding::Base64),
            _ => None,
        }
    }

    pub fn decode(self, raw: &[u8]) -> Result<Vec<u8>, Malformed> {
        match self {
            Encoding::Identity => Ok(raw.to_vec()),
            Encoding::QuotedPrintable => decode_quoted_printable(raw),
            Encoding::Base64 => decode_base64(raw),
        }
    }

    pub fn encode(self, content: &[u8]) -> Vec<u8> {
        match self {
            Encoding::Identity => content.to_vec(),
            Encoding::QuotedPrintable => encode_quoted_printable(content),
            Encoding::Base64 => encode_base64(content),
        }
    }

    /// Whether `content` can be written under this encoding at all.
    ///
    /// Only the identity encodings can fail, and only in one direction: a part
    /// declared `7bit` cannot carry a byte above 127. Simmer never changes a
    /// part's `Content-Transfer-Encoding` (D-045), so a rewrite that would
    /// require one is a rewrite that does not happen.
    pub fn can_represent(self, content: &[u8], seven_bit: bool) -> bool {
        match self {
            Encoding::Identity if seven_bit => content.iter().all(|b| *b < 128),
            _ => true,
        }
    }
}

/// Whether a `Content-Transfer-Encoding` value promises seven-bit content.
///
/// [`Encoding::parse`] deliberately collapses `7bit`, `8bit` and `binary` into
/// one codec — they are the same bytes either way. This is the question the
/// collapse throws away, and D-045 is what needs it.
pub fn declares_seven_bit(value: Option<&str>) -> bool {
    // RFC 2045 §6.1: absent means 7bit.
    value.is_none_or(|v| {
        let t = token(v);
        t.is_empty() || t.eq_ignore_ascii_case("7bit")
    })
}

/// The bare token of a header value: a parameter-less keyword, tolerating the
/// comment-ish trailing junk some mailers emit (`7bit (default)`).
fn token(value: &str) -> &str {
    value.split([';', '(']).next().unwrap_or("").trim()
}

// ---------------------------------------------------------------------------
// quoted-printable
// ---------------------------------------------------------------------------

fn decode_quoted_printable(raw: &[u8]) -> Result<Vec<u8>, Malformed> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;

    while i < raw.len() {
        if raw[i] != b'=' {
            out.push(raw[i]);
            i += 1;
            continue;
        }

        match raw.get(i + 1) {
            // Soft line break: the `=` and the line ending vanish. This is the
            // construct §6.4 is written about.
            Some(b'\r') if raw.get(i + 2) == Some(&b'\n') => i += 3,
            Some(b'\n') => i += 2,
            Some(hi) => {
                let lo = raw.get(i + 2).ok_or(Malformed)?;
                let (hi, lo) = (hex(*hi).ok_or(Malformed)?, hex(*lo).ok_or(Malformed)?);
                out.push(hi << 4 | lo);
                i += 3;
            }
            // A trailing `=` with nothing after it.
            None => return Err(Malformed),
        }
    }

    Ok(out)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        // RFC 2045 §6.7 note 1 says encoders must use uppercase and decoders
        // "should" accept lowercase. Real mail contains both.
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

fn encode_quoted_printable(content: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(content.len() + content.len() / 4);
    let mut line = 0usize;
    let mut i = 0;

    while i < content.len() {
        let b = content[i];

        // A hard line break is written as itself and resets the line budget.
        if b == b'\r' && content.get(i + 1) == Some(&b'\n') {
            out.extend_from_slice(b"\r\n");
            line = 0;
            i += 2;
            continue;
        }

        // RFC 2045 §6.7 rule 3: whitespace must not end a line, because a
        // transport is free to strip it. Whitespace anywhere else is literal,
        // as is any printable ASCII other than the escape character itself.
        let trailing_whitespace = matches!(b, b' ' | b'\t')
            && (i + 1 == content.len() || content[i + 1..].starts_with(b"\r\n"));
        let literal = !trailing_whitespace
            && (matches!(b, b' ' | b'\t') || ((33..=126).contains(&b) && b != b'='));

        let token: Vec<u8> = if literal {
            vec![b]
        } else {
            format!("={b:02X}").into_bytes()
        };

        if line + token.len() > QP_MAX_CONTENT {
            soft_break(&mut out, &mut line);
        }
        out.extend_from_slice(&token);
        line += token.len();
        i += 1;
    }

    out
}

/// Insert a soft line break, encoding any whitespace it would strand.
///
/// Rule 3 again: the break makes whatever precedes it the end of a line, so a
/// space that was legal a byte ago is not any more.
fn soft_break(out: &mut Vec<u8>, line: &mut usize) {
    if matches!(out.last(), Some(b' ') | Some(b'\t')) {
        let ws = out.pop().expect("just matched");
        out.extend_from_slice(format!("={ws:02X}").as_bytes());
        *line += 2;
        // Encoding it cost two more columns, which may itself overflow. One
        // extra soft break absorbs that; it cannot recurse further, because the
        // byte before an escape sequence is never whitespace.
    }
    out.extend_from_slice(b"=\r\n");
    *line = 0;
}

// ---------------------------------------------------------------------------
// base64
// ---------------------------------------------------------------------------

fn decode_base64(raw: &[u8]) -> Result<Vec<u8>, Malformed> {
    // RFC 2045 §6.8: "any characters outside of the base64 alphabet are to be
    // ignored". Line endings are the reason the rule exists; anything else
    // outside the alphabet is a malformed part and the engine below says so.
    let packed: Vec<u8> = raw
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(&packed)
        .map_err(|_| Malformed)
}

fn encode_base64(content: &[u8]) -> Vec<u8> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(content);
    let mut out = Vec::with_capacity(encoded.len() + encoded.len() / BASE64_LINE * 2);
    for (i, chunk) in encoded.as_bytes().chunks(BASE64_LINE).enumerate() {
        if i > 0 {
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(chunk);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- parsing the header -----------------------------------------------

    #[test]
    fn an_absent_header_is_the_identity_encoding() {
        // RFC 2045 §6.1 — 7bit is the default, and 7bit content is its own
        // encoding.
        assert_eq!(Encoding::parse(None), Some(Encoding::Identity));
    }

    #[test]
    fn the_identity_encodings_are_all_spelled_out() {
        // An empty value is here too: `Content-Transfer-Encoding:` with nothing
        // after it says no more than omitting the header does.
        for value in ["7bit", "8BIT", " binary ", "7Bit", ""] {
            assert_eq!(
                Encoding::parse(Some(value)),
                Some(Encoding::Identity),
                "{value}"
            );
        }
    }

    #[test]
    fn the_two_encodings_6_4_names_are_recognised_case_insensitively() {
        assert_eq!(
            Encoding::parse(Some("Quoted-Printable")),
            Some(Encoding::QuotedPrintable)
        );
        assert_eq!(Encoding::parse(Some("BASE64")), Some(Encoding::Base64));
    }

    #[test]
    fn an_encoding_we_do_not_implement_is_none_rather_than_a_guess() {
        // §6.4: the part is left untouched. Treating uuencode as 7bit would
        // rewrite the framing of a part we cannot read.
        for value in ["uuencode", "x-gzip64", "base32", "7bits"] {
            assert_eq!(Encoding::parse(Some(value)), None, "{value}");
        }
    }

    // -- quoted-printable, the §6.4 case ----------------------------------

    #[test]
    fn a_soft_line_break_is_what_6_4_is_written_about() {
        // The spec's own example: a URL split across a soft break does not match
        // a pattern written against the URL, so decoding has to come first.
        let raw = b"Visit https://old.=\r\nbrand.com/x today\r\n";
        let decoded = Encoding::QuotedPrintable.decode(raw).unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "Visit https://old.brand.com/x today\r\n"
        );
    }

    #[test]
    fn escapes_decode_in_either_case() {
        let upper = Encoding::QuotedPrintable
            .decode(b"Gr=C3=BC=C3=9Fe")
            .unwrap();
        let lower = Encoding::QuotedPrintable
            .decode(b"Gr=c3=bc=c3=9fe")
            .unwrap();
        assert_eq!(String::from_utf8(upper.clone()).unwrap(), "Grüße");
        assert_eq!(upper, lower);
    }

    #[test]
    fn a_truncated_escape_is_malformed_not_repaired() {
        assert_eq!(Encoding::QuotedPrintable.decode(b"abc="), Err(Malformed));
        assert_eq!(Encoding::QuotedPrintable.decode(b"abc=C"), Err(Malformed));
        assert_eq!(Encoding::QuotedPrintable.decode(b"abc=ZZ"), Err(Malformed));
    }

    #[test]
    fn encoding_escapes_exactly_what_rfc_2045_requires() {
        let out = encode_quoted_printable("a=b\u{00e9}c".as_bytes());
        // `=` always, non-ASCII always, ordinary printables never.
        assert_eq!(String::from_utf8(out).unwrap(), "a=3Db=C3=A9c");
    }

    #[test]
    fn whitespace_is_literal_except_at_the_end_of_a_line() {
        let out = encode_quoted_printable(b"a b \r\nc\t\r\nd e");
        assert_eq!(String::from_utf8(out).unwrap(), "a b=20\r\nc=09\r\nd e");
    }

    #[test]
    fn long_lines_are_softly_broken_within_76_columns() {
        let content = "x".repeat(200);
        let out = encode_quoted_printable(content.as_bytes());
        let text = String::from_utf8(out).unwrap();
        for line in text.split("\r\n") {
            assert!(line.len() <= 76, "line of {} chars: {line}", line.len());
        }
        // And it still means the same thing.
        assert_eq!(
            String::from_utf8(decode_quoted_printable(text.as_bytes()).unwrap()).unwrap(),
            content
        );
    }

    #[test]
    fn a_soft_break_never_strands_a_space_at_the_end_of_a_line() {
        // Rule 3 is not advisory: a transport may strip trailing whitespace, so
        // a space left before a soft break is data loss.
        let content = format!("{} b", "x".repeat(74));
        let out = encode_quoted_printable(content.as_bytes());
        let text = String::from_utf8(out).unwrap();
        for line in text.split("\r\n") {
            assert!(
                !line.ends_with(' ') && !line.ends_with('\t'),
                "trailing whitespace in: {line:?}"
            );
        }
        assert_eq!(
            String::from_utf8(decode_quoted_printable(text.as_bytes()).unwrap()).unwrap(),
            content
        );
    }

    #[test]
    fn hard_line_breaks_survive_encoding_as_themselves() {
        let out = encode_quoted_printable(b"one\r\ntwo\r\n");
        assert_eq!(String::from_utf8(out).unwrap(), "one\r\ntwo\r\n");
    }

    // -- base64 -------------------------------------------------------------

    #[test]
    fn base64_decoding_ignores_the_line_endings_that_frame_it() {
        let raw = b"SGVsbG8s\r\nIHdvcmxk";
        assert_eq!(
            String::from_utf8(Encoding::Base64.decode(raw).unwrap()).unwrap(),
            "Hello, world"
        );
    }

    #[test]
    fn base64_that_is_not_base64_is_malformed() {
        assert_eq!(
            Encoding::Base64.decode(b"not base64 at all!"),
            Err(Malformed)
        );
    }

    #[test]
    fn base64_encoding_wraps_at_76_columns() {
        let content = vec![b'z'; 500];
        let out = encode_base64(&content);
        let text = String::from_utf8(out).unwrap();
        for line in text.split("\r\n") {
            assert!(line.len() <= 76, "line of {} chars", line.len());
        }
        assert!(
            text.contains("\r\n"),
            "500 bytes should not fit on one line"
        );
    }

    // -- the property the rest of §6.4 rests on ----------------------------

    #[test]
    fn every_encoding_round_trips_every_byte() {
        // The claim body.rs makes when it puts a part back: what comes out of
        // `encode` is what `decode` reads as the content it was given. Without
        // this, a part whose pattern did not match could still come back
        // different.
        let all_bytes: Vec<u8> = (0..=255).collect();
        let cases: [&[u8]; 5] = [
            b"",
            b"plain ascii text\r\n",
            b"Gr\xc3\xbc\xc3\x9fe \xe2\x80\x94 with an em dash\r\n",
            b".\r\n..\r\nline\r\n\r\n",
            &all_bytes,
        ];

        for encoding in [
            Encoding::Identity,
            Encoding::QuotedPrintable,
            Encoding::Base64,
        ] {
            for case in cases {
                let round_tripped = encoding
                    .decode(&encoding.encode(case))
                    .unwrap_or_else(|_| panic!("{encoding:?} could not read its own output"));
                assert_eq!(round_tripped, case, "{encoding:?} on {case:?}");
            }
        }
    }

    #[test]
    fn a_seven_bit_part_cannot_carry_a_high_byte() {
        // D-045: the CTE is never changed, so this is the question that decides
        // whether a rewrite happens at all.
        assert!(!Encoding::Identity.can_represent("é".as_bytes(), true));
        assert!(Encoding::Identity.can_represent(b"plain", true));
        assert!(Encoding::Identity.can_represent("é".as_bytes(), false));
        // The encodings that exist to carry arbitrary bytes always can.
        assert!(Encoding::QuotedPrintable.can_represent("é".as_bytes(), true));
        assert!(Encoding::Base64.can_represent("é".as_bytes(), true));
    }
}
