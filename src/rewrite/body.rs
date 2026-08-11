//! `SPEC.md` §6.4 — body rewriting.
//!
//! > Scope is `text/*` parts only. Attachments and non-text parts are never
//! > touched. For each `text/*` part: decode according to
//! > `Content-Transfer-Encoding` (handling `quoted-printable` and `base64`),
//! > decode the charset to UTF-8, apply each `body_rewrites` entry in order as a
//! > regex replacement, re-encode, and fix up `Content-Transfer-Encoding` and
//! > any length-bearing headers.
//!
//! ## The one property this module has to keep
//!
//! Phase 4 made "the body arrives byte for byte" structurally true: `headers.rs`
//! handed the body back as an opaque slice and nothing touched it (D-039). §6.4
//! ends that, and the temptation is to replace it with a test. It is replaced
//! instead with a **narrower structural guarantee**: a part that does not match
//! is never decoded-and-re-encoded, and a body in which no part matched is
//! returned as `None` — the caller splices the original slice back. Only the
//! spans that actually changed are rebuilt.
//!
//! That matters more than it sounds. Re-encoding is not the identity: a
//! quoted-printable part re-wrapped by our encoder rather than the client's is
//! *equivalent* but not *equal*, and §12.3 compares bytes. Confining re-encoding
//! to the parts a `body_rewrites` entry actually hit means the difference exists
//! only where the operator asked for one.
//!
//! ## Two things Simmer will not do to a part (D-045)
//!
//! §6.4 says to "fix up `Content-Transfer-Encoding`", which anticipates changing
//! it. This module never does, and never changes a part's charset either. If the
//! rewritten text cannot be written under the encoding and charset the part
//! arrived with — which takes a `body_rewrites` replacement containing a
//! character the part's charset has no room for — the part is left untouched,
//! logged and counted, exactly as §6.4 treats a part it cannot decode.
//!
//! The reason is §1.1. Simmer's output has to stay "exactly expressible as
//! application-side configuration", and re-framing a part from `7bit` to
//! `quoted-printable` is not something the operator could hand back to the
//! application as a setting. Not needing the fix-up is a better answer than
//! performing it well.
//!
//! Length-bearing headers *are* fixed up, because those are statements about the
//! part that a rewrite can falsify.

use std::ops::Range;

use crate::config::BodyRewrite;

use super::headers::{self, HeaderBlock};
use super::{charset, mime, transfer};

/// Why a part §6.4 would otherwise have rewritten was left alone.
///
/// The label on `simmer_body_rewrite_skipped_total`. Two of these are policy —
/// `signed` and `encrypted` are §6.4 protecting a signature — and the rest are
/// the part being unreadable. All six are worth counting for the same reason:
/// they are the cases where an operator's configured rewrite silently does not
/// happen, and nothing else in the mail flow would show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// `multipart/signed` or `application/pkcs7-*`. Rewriting invalidates it.
    Signed,
    /// `multipart/encrypted`.
    Encrypted,
    /// A `Content-Transfer-Encoding` this build does not implement.
    UnsupportedEncoding,
    /// A `quoted-printable` or `base64` part that does not decode.
    MalformedEncoding,
    /// A `charset` this build does not implement (D-044).
    UnsupportedCharset,
    /// The rewritten text cannot be written back in the part's own charset or
    /// transfer encoding, and Simmer does not change either (D-045).
    Unrepresentable,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::Signed => "signed",
            SkipReason::Encrypted => "encrypted",
            SkipReason::UnsupportedEncoding => "unsupported_encoding",
            SkipReason::MalformedEncoding => "malformed_encoding",
            SkipReason::UnsupportedCharset => "unsupported_charset",
            SkipReason::Unrepresentable => "unrepresentable",
        }
    }

    /// What the `WARN` §6.4 asks for should say.
    pub fn describe(self) -> &'static str {
        match self {
            SkipReason::Signed => "the part is signed; rewriting it would invalidate the signature",
            SkipReason::Encrypted => "the part is encrypted",
            SkipReason::UnsupportedEncoding => {
                "the part's Content-Transfer-Encoding is not one Simmer can decode"
            }
            SkipReason::MalformedEncoding => "the part's transfer encoding does not decode",
            SkipReason::UnsupportedCharset => "the part's charset is not one Simmer can decode",
            SkipReason::Unrepresentable => {
                "the rewritten text does not fit the part's own charset or transfer encoding, \
                 and Simmer does not change either"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// compiled form
// ---------------------------------------------------------------------------

/// A route's `body_rewrites`, compiled.
///
/// Compiled at startup for the same reason the templates are: a pattern that
/// does not compile is a configuration error, and §4.2 already names it as one.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    pattern: regex::Regex,
    replacement: String,
}

/// One rule's verdict against a sample, for §9.4.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RuleMatch {
    /// Position in `body_rewrites`, which is also application order (§6.4).
    pub index: usize,
    pub pattern: String,
    pub replacement: String,
    pub count: usize,
}

impl Rules {
    /// `Err` names the index of each pattern that does not compile, so §4.2 can
    /// report all of them at once rather than the first.
    pub fn compile(specs: &[BodyRewrite]) -> Result<Rules, Vec<(usize, regex::Error)>> {
        let mut rules = Vec::with_capacity(specs.len());
        let mut errors = Vec::new();

        for (i, spec) in specs.iter().enumerate() {
            match regex::Regex::new(&spec.pattern) {
                Ok(pattern) => rules.push(Rule {
                    pattern,
                    replacement: spec.replacement.clone(),
                }),
                Err(e) => errors.push((i, e)),
            }
        }

        if errors.is_empty() {
            Ok(Rules { rules })
        } else {
            Err(errors)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Apply every rule in order. `None` means the text came out unchanged.
    ///
    /// `None` is not an optimisation, it is the guarantee in the module comment:
    /// a part that did not change is never re-encoded, so it keeps its original
    /// bytes.
    pub fn apply(&self, text: &str) -> Option<String> {
        // If no rule matches the text as it stands, no rule can match after the
        // others have run either — rule 1 leaves the text alone, so rule 2 sees
        // exactly what it was just shown. Checking first avoids copying a
        // multi-megabyte part that nothing is going to touch.
        if !self.rules.iter().any(|r| r.pattern.is_match(text)) {
            return None;
        }

        let mut current = text.to_string();
        for rule in &self.rules {
            if let std::borrow::Cow::Owned(next) = rule
                .pattern
                .replace_all(&current, rule.replacement.as_str())
            {
                current = next;
            }
        }

        // A rule can match and replace with what was already there.
        (current != text).then_some(current)
    }

    /// §9.4 — which rules match this text, and how many times, changing nothing.
    ///
    /// Sequential, exactly as [`apply`](Self::apply) is: each rule is counted
    /// against the text the rules before it produced, because that is the text
    /// it will actually be shown. Counting every rule against the original would
    /// over-report a rule whose input an earlier rule consumes, and dry run
    /// exists to answer "will my rewrite fire", not "would it have fired first".
    ///
    /// Every rule is reported, including the ones that matched nothing — a
    /// pattern with a zero count is the single most useful thing this endpoint
    /// can tell an operator, and omitting it would leave them reading a list to
    /// work out what is not on it.
    ///
    /// Deliberately not folded into `apply`: `apply` is on the message path and
    /// counting costs a second scan per rule.
    pub fn match_report(&self, text: &str) -> Vec<RuleMatch> {
        let mut current = std::borrow::Cow::Borrowed(text);
        let mut out = Vec::with_capacity(self.rules.len());

        for (index, rule) in self.rules.iter().enumerate() {
            let count = rule.pattern.find_iter(&current).count();
            out.push(RuleMatch {
                index,
                pattern: rule.pattern.as_str().to_string(),
                replacement: rule.replacement.clone(),
                count,
            });
            if count > 0 {
                current = std::borrow::Cow::Owned(
                    rule.pattern
                        .replace_all(&current, rule.replacement.as_str())
                        .into_owned(),
                );
            }
        }

        out
    }

    /// §6.6 applied to the body: does running the rules over their own output
    /// change it again?
    ///
    /// Returns the two successive results when it does. A rule like `s/a/aa/`
    /// matches what it just produced, so every pass through Simmer grows the
    /// body — which is §1.1 constraint 1's "append `.new` to the sending domain"
    /// in a different place, and corrupts traffic from an application that has
    /// already been cut over.
    ///
    /// The probe is built from the rules' own **replacements**, because that is
    /// what an unstable rule re-matches. Comparing `apply(probe)` with
    /// `apply(apply(probe))` rather than with `probe` is deliberate: a chain
    /// where rule 2 consumes rule 1's output is stable and must not be reported.
    pub fn fixed_point_violation(&self) -> Option<(String, String)> {
        if self.rules.is_empty() {
            return None;
        }

        let probe = self.probe_text();
        let once = self.apply(&probe).unwrap_or(probe);
        let twice = self.apply(&once).unwrap_or_else(|| once.clone());

        (once != twice).then_some((once, twice))
    }

    fn probe_text(&self) -> String {
        let mut lines: Vec<String> = self.rules.iter().map(|r| r.replacement.clone()).collect();
        // Adjacent, too: a rule can be stable on its own output and unstable on
        // its output next to another's.
        lines.push(lines.concat());
        lines.join("\r\n")
    }
}

// ---------------------------------------------------------------------------
// the engine
// ---------------------------------------------------------------------------

/// What §6.1 step 7 produced.
#[derive(Debug, Default)]
pub struct Outcome {
    /// `None` means no part changed, so the caller keeps the original slice.
    pub body: Option<Vec<u8>>,
    /// Length-bearing headers of the *message's own* header block that a rewrite
    /// falsified. Nested parts' headers are patched inside `body` directly;
    /// these cannot be, because §6.1 steps 4–6 own that block.
    pub header_fixups: Vec<(&'static str, String)>,
    /// One per part §6.4 would have rewritten and did not.
    pub skipped: Vec<SkipReason>,
}

/// §6.1 step 7.
///
/// `message_headers` is the block *as steps 4–6 left it*, so a route that sets
/// `Content-Type` is read the way it will be sent.
pub fn rewrite(rules: &Rules, message_headers: &HeaderBlock, body: &[u8]) -> Outcome {
    let mut outcome = Outcome::default();
    if rules.is_empty() || body.is_empty() {
        return outcome;
    }

    let mut edits: Vec<(Range<usize>, Vec<u8>)> = Vec::new();

    for node in mime::walk(message_headers, body) {
        let part = match node {
            mime::Node::Excluded(reason) => {
                outcome.skipped.push(reason);
                continue;
            }
            mime::Node::Text(part) => part,
        };

        let content = match rewrite_part(rules, &part, body) {
            Ok(None) => continue,
            Ok(Some(content)) => content,
            Err(reason) => {
                outcome.skipped.push(reason);
                continue;
            }
        };

        // §6.4's "length-bearing headers", which a rewrite can falsify.
        if !part.length_headers.is_empty() {
            let fixups = length_fixups(&part.length_headers, &content);
            match &part.headers {
                Some(span) => {
                    edits.push((span.clone(), patch_headers(&body[span.clone()], &fixups)))
                }
                None => outcome.header_fixups = fixups,
            }
        }

        edits.push((part.content.clone(), content));
    }

    if edits.is_empty() {
        return outcome;
    }

    edits.sort_by_key(|(span, _)| span.start);
    let mut out = Vec::with_capacity(body.len());
    let mut cursor = 0;
    for (span, replacement) in edits {
        // The walk yields disjoint, ordered spans; this is the assertion that
        // keeps a future change to it from silently corrupting a message.
        debug_assert!(span.start >= cursor, "overlapping spans in the MIME walk");
        out.extend_from_slice(&body[cursor..span.start]);
        out.extend_from_slice(&replacement);
        cursor = span.end;
    }
    out.extend_from_slice(&body[cursor..]);

    outcome.body = Some(out);
    outcome
}

/// Decode, rewrite, re-encode one part. `Ok(None)` is "nothing matched".
fn rewrite_part(
    rules: &Rules,
    part: &mime::TextPart,
    body: &[u8],
) -> Result<Option<Vec<u8>>, SkipReason> {
    let raw = &body[part.content.clone()];

    let encoding = transfer::Encoding::parse(part.transfer_encoding.as_deref())
        .ok_or(SkipReason::UnsupportedEncoding)?;
    let charset = charset::Charset::parse(part.content_type.param("charset"))
        .ok_or(SkipReason::UnsupportedCharset)?;

    let decoded = encoding
        .decode(raw)
        .map_err(|_| SkipReason::MalformedEncoding)?;
    let text = charset
        .decode(&decoded)
        .map_err(|_| SkipReason::UnsupportedCharset)?;

    let Some(rewritten) = rules.apply(&text) else {
        return Ok(None);
    };

    let bytes = charset
        .encode(&rewritten)
        .map_err(|_| SkipReason::Unrepresentable)?;

    // D-045: the transfer encoding is never changed, so a part that was
    // genuinely 7-bit clean must stay that way. A part that already carried high
    // bytes under a `7bit` label is not held to a rule it was already breaking.
    let must_stay_seven_bit = transfer::declares_seven_bit(part.transfer_encoding.as_deref())
        && raw.iter().all(|b| *b < 128);
    if !encoding.can_represent(&bytes, must_stay_seven_bit) {
        return Err(SkipReason::Unrepresentable);
    }

    let mut encoded = encoding.encode(&bytes);

    // For base64 the line endings are framing rather than content, so our
    // encoder cannot know whether the span it is replacing ended with one. For
    // the other encodings the terminator is inside the content and is already
    // there.
    if encoding == transfer::Encoding::Base64 {
        if raw.ends_with(b"\r\n") {
            encoded.extend_from_slice(b"\r\n");
        } else if raw.ends_with(b"\n") {
            encoded.push(b'\n');
        }
    }

    Ok(Some(encoded))
}

fn length_fixups(present: &[&'static str], content: &[u8]) -> Vec<(&'static str, String)> {
    present
        .iter()
        .map(|name| match *name {
            "Lines" => (*name, count_lines(content).to_string()),
            _ => (*name, content.len().to_string()),
        })
        .collect()
}

fn count_lines(content: &[u8]) -> usize {
    let breaks = content.iter().filter(|b| **b == b'\n').count();
    if content.is_empty() || content.ends_with(b"\n") {
        breaks
    } else {
        breaks + 1
    }
}

/// Rewrite named headers in a part's own header block, leaving the rest as the
/// bytes it arrived as.
fn patch_headers(block: &[u8], fixups: &[(&'static str, String)]) -> Vec<u8> {
    let mut headers = headers::split(block).headers;
    for (name, value) in fixups {
        // Only ever a header the part already carries: `set` would otherwise
        // append one, and adding a header is D-037's business, not §6.4's.
        if headers.contains(name) {
            headers.set(name, value.clone());
        }
    }
    headers.to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(pairs: &[(&str, &str)]) -> Rules {
        let specs: Vec<BodyRewrite> = pairs
            .iter()
            .map(|(pattern, replacement)| BodyRewrite {
                pattern: pattern.to_string(),
                replacement: replacement.to_string(),
            })
            .collect();
        Rules::compile(&specs).expect("fixture compiles")
    }

    /// The shipped rule, from `simmer.yaml`.
    fn shipped() -> Rules {
        rules(&[(r"https://oldbrand\.com/", "https://newbrand.com/")])
    }

    fn run(rules: &Rules, headers: &str, body: &str) -> Outcome {
        rewrite(
            rules,
            &headers::split(headers.as_bytes()).headers,
            body.as_bytes(),
        )
    }

    fn body_of(outcome: &Outcome, original: &str) -> String {
        match &outcome.body {
            Some(b) => String::from_utf8(b.clone()).expect("utf-8 in these fixtures"),
            None => original.to_string(),
        }
    }

    // -- the property the module is built around ---------------------------

    #[test]
    fn a_body_no_rule_matches_is_returned_untouched_not_rebuilt() {
        // The replacement for D-039's structural guarantee. `None` is the whole
        // claim: the caller splices the original slice back, so nothing can have
        // shifted.
        let body = "Nothing here matches.\r\nVisit https://newbrand.com/x\r\n";
        let out = run(&shipped(), "Content-Type: text/plain\r\n", body);
        assert!(out.body.is_none(), "{:?}", out.body.map(String::from_utf8));
        assert!(out.skipped.is_empty());
    }

    #[test]
    fn a_route_with_no_body_rewrites_never_looks_at_the_body() {
        let out = run(
            &Rules::default(),
            "Content-Type: text/plain\r\n",
            "https://oldbrand.com/x\r\n",
        );
        assert!(out.body.is_none());
    }

    #[test]
    fn only_the_matching_part_is_rebuilt_the_rest_is_copied() {
        let headers = "Content-Type: multipart/mixed; boundary=b1\r\n";
        let body = concat!(
            "preamble\r\n",
            "--b1\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "Go to https://oldbrand.com/x\r\n",
            "--b1\r\n",
            "Content-Type: application/pdf\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "\r\n",
            "aHR0cHM6Ly9vbGRicmFuZC5jb20v\r\n",
            "--b1--\r\n",
            "epilogue\r\n",
        );
        let out = run(&shipped(), headers, body);
        let got = body_of(&out, body);

        assert!(got.contains("Go to https://newbrand.com/x"), "{got}");
        // The attachment encodes the same URL and must not be touched — it is
        // not text/*, and decoding it to find out would be exactly what §6.4
        // says not to do.
        assert!(got.contains("aHR0cHM6Ly9vbGRicmFuZC5jb20v"), "{got}");
        assert!(got.starts_with("preamble\r\n--b1\r\n"), "{got}");
        assert!(got.ends_with("--b1--\r\nepilogue\r\n"), "{got}");
    }

    // -- §6.4's headline case ---------------------------------------------

    #[test]
    fn a_url_split_across_a_soft_line_break_matches() {
        // §6.4's own example, and its stated reason for rejecting raw-byte
        // matching: "which is the common case in real mail rather than an edge
        // case".
        let body = "Visit https://old=\r\nbrand.com/x today\r\n";
        let out = run(
            &rules(&[(r"https://oldbrand\.com/", "https://newbrand.com/")]),
            "Content-Type: text/plain\r\nContent-Transfer-Encoding: quoted-printable\r\n",
            body,
        );
        let got = body_of(&out, body);
        assert!(got.contains("https://newbrand.com/x"), "{got}");
        // And it comes back encoded, not decoded.
        assert!(!got.contains("=\r\n") || got.contains("=3D"), "{got}");
    }

    #[test]
    fn a_base64_part_is_decoded_rewritten_and_re_encoded() {
        let inner = "Track it at https://oldbrand.com/track\r\n";
        let encoded =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, inner.as_bytes());
        let body = format!("{encoded}\r\n");
        let out = run(
            &shipped(),
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n",
            &body,
        );
        let got = body_of(&out, &body);
        let decoded = transfer::Encoding::Base64
            .decode(got.trim_end().as_bytes())
            .expect("still base64");
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            "Track it at https://newbrand.com/track\r\n"
        );
        assert!(
            got.ends_with("\r\n"),
            "the framing terminator is kept: {got:?}"
        );
    }

    #[test]
    fn a_latin1_part_comes_back_as_latin1() {
        // D-045: the charset is not changed. Byte 0xe9 in, byte 0xe9 out.
        let body = b"caf\xe9: https://oldbrand.com/x\r\n";
        let out = rewrite(
            &shipped(),
            &headers::split(b"Content-Type: text/plain; charset=iso-8859-1\r\n").headers,
            body,
        );
        let got = out.body.expect("the URL matched");
        assert_eq!(got, b"caf\xe9: https://newbrand.com/x\r\n".to_vec());
    }

    // -- §6.4's exclusions -------------------------------------------------

    #[test]
    fn a_signed_message_is_skipped_and_counted() {
        let headers = "Content-Type: multipart/signed; boundary=sig\r\n";
        let body = concat!(
            "--sig\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "https://oldbrand.com/x\r\n",
            "--sig--\r\n",
        );
        let out = run(&shipped(), headers, body);
        assert!(out.body.is_none(), "a signature must survive");
        assert_eq!(out.skipped, vec![SkipReason::Signed]);
    }

    #[test]
    fn a_part_whose_encoding_we_cannot_read_is_skipped_and_counted() {
        let out = run(
            &shipped(),
            "Content-Type: text/plain\r\nContent-Transfer-Encoding: x-uuencode\r\n",
            "https://oldbrand.com/x\r\n",
        );
        assert!(out.body.is_none());
        assert_eq!(out.skipped, vec![SkipReason::UnsupportedEncoding]);
    }

    #[test]
    fn a_part_whose_charset_we_cannot_read_is_skipped_and_counted() {
        // D-044. §6.4: "If a part cannot be decoded (unknown charset, malformed
        // encoding), leave it untouched, log at WARN, and increment
        // simmer_body_rewrite_skipped_total."
        let out = run(
            &shipped(),
            "Content-Type: text/plain; charset=shift_jis\r\n",
            "https://oldbrand.com/x\r\n",
        );
        assert!(out.body.is_none());
        assert_eq!(out.skipped, vec![SkipReason::UnsupportedCharset]);
    }

    #[test]
    fn a_malformed_quoted_printable_part_is_skipped_and_counted() {
        let out = run(
            &shipped(),
            "Content-Type: text/plain\r\nContent-Transfer-Encoding: quoted-printable\r\n",
            "https://oldbrand.com/x =ZZ\r\n",
        );
        assert!(out.body.is_none());
        assert_eq!(out.skipped, vec![SkipReason::MalformedEncoding]);
    }

    #[test]
    fn a_replacement_that_does_not_fit_the_part_is_skipped_rather_than_mangled() {
        // D-045. The alternative is re-framing the part, which is a change the
        // operator could not express as application-side configuration.
        let out = run(
            &rules(&[("oldbrand", "nöubrand")]),
            "Content-Type: text/plain; charset=us-ascii\r\nContent-Transfer-Encoding: 7bit\r\n",
            "https://oldbrand.com/x\r\n",
        );
        assert!(out.body.is_none());
        assert_eq!(out.skipped, vec![SkipReason::Unrepresentable]);
    }

    #[test]
    fn a_part_that_was_already_breaking_its_own_7bit_label_is_still_rewritten() {
        // Refusing here would skip a rewrite over a rule the *client* broke, and
        // §6.4's exclusions are about what Simmer would damage, not about
        // policing the sender.
        let body = "caf\u{e9} https://oldbrand.com/x\r\n";
        let out = run(
            &shipped(),
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 7bit\r\n",
            body,
        );
        assert!(body_of(&out, body).contains("https://newbrand.com/x"));
    }

    // -- length-bearing headers -------------------------------------------

    #[test]
    fn a_length_bearing_header_on_the_message_is_fixed_up() {
        // §6.4: "fix up … any length-bearing headers." The message's own block
        // belongs to §6.1 steps 4–6, so the fix-up goes back to the caller.
        let body = "https://oldbrand.com/\r\n";
        let out = run(
            &shipped(),
            "Content-Type: text/plain\r\nContent-Length: 23\r\nLines: 1\r\n",
            body,
        );
        assert_eq!(
            out.header_fixups,
            vec![
                ("Content-Length", "23".to_string()),
                ("Lines", "1".to_string())
            ]
        );
        // 23 happens to be right for this pair; the point is that it is measured
        // rather than carried over. A longer replacement moves it:
        let out = run(
            &rules(&[("oldbrand", "a-much-longer-brand")]),
            "Content-Type: text/plain\r\nContent-Length: 23\r\n",
            body,
        );
        assert_eq!(
            out.header_fixups,
            vec![("Content-Length", "34".to_string())]
        );
    }

    #[test]
    fn a_length_bearing_header_on_a_nested_part_is_patched_in_place() {
        let headers = "Content-Type: multipart/mixed; boundary=b1\r\n";
        let body = concat!(
            "--b1\r\n",
            "Content-Type: text/plain\r\n",
            "Content-Length: 22\r\n",
            "X-Odd:   spacing  kept\r\n",
            "\r\n",
            "https://oldbrand.com/\r\n",
            "--b1--\r\n",
        );
        let out = run(&shipped(), headers, body);
        let got = body_of(&out, body);
        // 21, not 23: the CRLF before the boundary belongs to the boundary, so
        // it is not part of what this part's length describes.
        assert!(got.contains("Content-Length: 21\r\n"), "{got}");
        // Every other header of that part keeps its original bytes.
        assert!(got.contains("X-Odd:   spacing  kept\r\n"), "{got}");
        assert!(out.header_fixups.is_empty());
    }

    // -- rule semantics ----------------------------------------------------

    #[test]
    fn rules_are_applied_in_the_order_they_are_configured() {
        let out = rules(&[("a", "b"), ("b", "c")]).apply("a");
        assert_eq!(out.as_deref(), Some("c"));
    }

    #[test]
    fn every_occurrence_is_replaced_not_just_the_first() {
        let out = shipped().apply("https://oldbrand.com/a and https://oldbrand.com/b");
        assert_eq!(
            out.as_deref(),
            Some("https://newbrand.com/a and https://newbrand.com/b")
        );
    }

    #[test]
    fn capture_groups_are_available_in_a_replacement() {
        let out = rules(&[(r"https://oldbrand\.com/(\w+)", "https://newbrand.com/$1")])
            .apply("https://oldbrand.com/track");
        assert_eq!(out.as_deref(), Some("https://newbrand.com/track"));
    }

    #[test]
    fn a_rule_that_replaces_text_with_itself_reports_no_change() {
        assert_eq!(rules(&[("a", "a")]).apply("banana"), None);
    }

    #[test]
    fn every_pattern_that_does_not_compile_is_reported_not_just_the_first() {
        // §4.2: "Report all violations, not just the first."
        let specs = [
            BodyRewrite {
                pattern: "[unclosed".to_string(),
                replacement: "x".to_string(),
            },
            BodyRewrite {
                pattern: "fine".to_string(),
                replacement: "y".to_string(),
            },
            BodyRewrite {
                pattern: "(?P<".to_string(),
                replacement: "z".to_string(),
            },
        ];
        let errors = Rules::compile(&specs).unwrap_err();
        assert_eq!(
            errors.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![0, 2]
        );
    }

    // -- §6.6, in the body -------------------------------------------------

    #[test]
    fn a_rule_that_matches_its_own_output_is_a_fixed_point_violation() {
        // The phase 5 open question, settled as D-046: this is §1.1 constraint
        // 1 in the body, and it grows the message on every pass.
        let (once, twice) = rules(&[("a", "aa")])
            .fixed_point_violation()
            .expect("s/a/aa/ is unstable");
        assert_ne!(once, twice);
    }

    #[test]
    fn the_shipped_rule_is_a_fixed_point() {
        assert_eq!(shipped().fixed_point_violation(), None);
    }

    #[test]
    fn a_chain_where_one_rule_consumes_anothers_output_is_stable() {
        // The false positive the probe has to avoid: applying the *chain* twice
        // is a no-op even though the first rule's output is the second's input.
        assert_eq!(
            rules(&[("a", "b"), ("b", "c")]).fixed_point_violation(),
            None
        );
    }

    #[test]
    fn a_rule_that_appends_to_what_it_matches_is_caught() {
        // The realistic version of `s/a/aa/`: bolting a tracking parameter onto
        // a link. The pattern still matches after the parameter is on, so every
        // pass adds another one.
        let violation = rules(&[(r"https://newbrand\.com/x", "https://newbrand.com/x?ref=1")])
            .fixed_point_violation();
        let (once, twice) = violation.expect("appending to a match is unstable");
        assert!(twice.len() > once.len(), "{once} then {twice}");
    }

    #[test]
    fn a_pair_of_rules_that_undo_each_other_is_stable_and_not_reported() {
        // Odd configuration, but idempotent: whatever it is given, the chain
        // lands on the same answer and stays there. §6.6's property is about
        // repetition changing the result, not about the result being sensible.
        assert_eq!(
            rules(&[
                (r"oldbrand\.com", "newbrand.com"),
                (r"newbrand\.com", "oldbrand.com"),
            ])
            .fixed_point_violation(),
            None
        );
    }

    #[test]
    fn a_route_with_no_rules_has_nothing_to_be_unstable_about() {
        assert_eq!(Rules::default().fixed_point_violation(), None);
    }
}
