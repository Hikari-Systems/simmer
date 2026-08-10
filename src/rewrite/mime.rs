//! MIME structure, as byte ranges into the body.
//!
//! §6.4 scopes body rewriting to `text/*` parts and says "attachments and
//! non-text parts are never touched". Answering "which bytes of this body are a
//! `text/*` part" is this module's whole job, and it answers it as **ranges**
//! rather than as content.
//!
//! ## Why ranges, and why not `mail-parser`
//!
//! `headers.rs` carries an untouched header as its original bytes so that
//! nothing Simmer was not configured to change is changed (D-039). §6.4 ends
//! that guarantee for the body — a rewritten part cannot be its original bytes —
//! but only for the parts that actually matched. Ranges keep the rest: `body.rs`
//! rebuilds the body by copying every span it did not rewrite verbatim, so a
//! `multipart/mixed` whose one `text/plain` part matched keeps its boundaries,
//! its preamble, its epilogue, its attachment and its line endings exactly.
//!
//! `mail-parser` does expose `offset_body`/`offset_end` per part, so this could
//! have leaned on it. It is not used here for two reasons. It decodes every part
//! as it parses, which doubles the peak memory of a large message for structure
//! we could get by scanning; and it has to be handed the whole message, which
//! would mean re-serialising the header block §6.1 steps 4–6 has just edited
//! before step 7 could read a `Content-Type` those steps might have set.
//! Walking the edited [`HeaderBlock`] and the untouched body sidesteps both.
//!
//! ## What is deliberately not descended into
//!
//! `multipart/signed`, `multipart/encrypted` and `application/pkcs7-*` — §6.4:
//! "rewriting would invalidate them". These come back as [`Node::Excluded`],
//! because an operator whose `body_rewrites` silently does nothing to a signed
//! message deserves to see that in a counter.
//!
//! `message/rfc822` and any part marked `Content-Disposition: attachment` are
//! not descended into either, and produce no node at all. They are not
//! *excluded* from a scope they were in; §6.4's first sentence puts attachments
//! outside it, and counting every forwarded message would make the counter
//! useless.

use std::ops::Range;

use super::body::SkipReason;
use super::headers::{self, HeaderBlock};

/// How deep a nesting this module will follow before treating a part as opaque.
///
/// Real mail nests three or four deep. The cap is here because the input is
/// attacker-influenced: nothing else stops a message with ten thousand nested
/// `multipart/mixed` parts from recursing until the stack runs out.
const MAX_DEPTH: usize = 20;

/// Headers whose value states the size of the part they belong to. §6.4:
/// "fix up `Content-Transfer-Encoding` and any length-bearing headers."
pub const LENGTH_HEADERS: [&str; 2] = ["Content-Length", "Lines"];

/// One thing found in the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A `text/*` part §6.4 permits rewriting.
    Text(TextPart),
    /// A part §6.4 forbids rewriting, for a reason worth counting.
    Excluded(SkipReason),
}

/// A `text/*` leaf, located in the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPart {
    /// The part's content, as a range into the top-level body.
    pub content: Range<usize>,
    /// The part's own header block, as a range into the top-level body.
    ///
    /// `None` for a single-part message, whose part headers *are* the message
    /// headers — those are edited through [`HeaderBlock`] by §6.1 steps 4–6 and
    /// must not also be spliced.
    pub headers: Option<Range<usize>>,
    pub content_type: ContentType,
    /// The raw `Content-Transfer-Encoding` value, if the part carries one.
    pub transfer_encoding: Option<String>,
    /// Which of [`LENGTH_HEADERS`] the part carries, in that order.
    pub length_headers: Vec<&'static str>,
}

/// A parsed `Content-Type` value: a type, a subtype, and its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentType {
    /// Lowercased.
    pub kind: String,
    /// Lowercased.
    pub subtype: String,
    /// Names lowercased; values as written, less any quoting.
    params: Vec<(String, String)>,
}

impl ContentType {
    /// RFC 2045 §5.2: "Default RFC 822 messages without a MIME Content-Type
    /// header are taken by this protocol to be plain text in the US-ASCII
    /// character set."
    pub fn default_for_a_part() -> ContentType {
        ContentType {
            kind: "text".to_string(),
            subtype: "plain".to_string(),
            params: Vec::new(),
        }
    }

    pub fn parse(value: &str) -> ContentType {
        let (mime, rest) = match value.split_once(';') {
            Some((m, r)) => (m, r),
            None => (value, ""),
        };
        let mime = mime.trim().to_ascii_lowercase();
        let (kind, subtype) = match mime.split_once('/') {
            Some((k, s)) => (k.trim().to_string(), s.trim().to_string()),
            // A `Content-Type:` with no slash is malformed. RFC 2045 §5.2 says
            // to treat an unrecognised type as `application/octet-stream`, which
            // here means "not text, so not in scope".
            None => ("application".to_string(), "octet-stream".to_string()),
        };

        ContentType {
            kind,
            subtype,
            params: parse_parameters(rest),
        }
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn is_text(&self) -> bool {
        self.kind == "text"
    }
}

/// Split `; name=value; name="value"` into pairs.
///
/// RFC 2231's `name*0=`/`name*=` continuations are not reassembled: the two
/// parameters this module reads are `boundary` and `charset`, and neither is
/// ever written that way in practice. A parameter spelled that way simply is not
/// found, which lands the part in §6.4's "cannot be decoded" case rather than
/// producing a wrong answer.
fn parse_parameters(rest: &str) -> Vec<(String, String)> {
    let mut params = Vec::new();
    let bytes: Vec<char> = rest.chars().collect();
    let mut i = 0;

    while i < bytes.len() {
        // name
        let name_start = i;
        while i < bytes.len() && bytes[i] != '=' && bytes[i] != ';' {
            i += 1;
        }
        let name: String = bytes[name_start..i].iter().collect();
        let name = name.trim().to_ascii_lowercase();

        if i >= bytes.len() || bytes[i] == ';' {
            i += 1;
            continue;
        }
        i += 1; // past '='

        // value, quoted or not
        let mut value = String::new();
        while i < bytes.len() && bytes[i].is_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == '"' {
            i += 1;
            while i < bytes.len() && bytes[i] != '"' {
                // RFC 2045 quoted-string escaping.
                if bytes[i] == '\\' && i + 1 < bytes.len() {
                    i += 1;
                }
                value.push(bytes[i]);
                i += 1;
            }
            i += 1; // past the closing quote
            while i < bytes.len() && bytes[i] != ';' {
                i += 1;
            }
            i += 1;
        } else {
            while i < bytes.len() && bytes[i] != ';' {
                value.push(bytes[i]);
                i += 1;
            }
            i += 1;
            value = value.trim().to_string();
        }

        if !name.is_empty() {
            params.push((name, value));
        }
    }

    params
}

// ---------------------------------------------------------------------------
// the walk
// ---------------------------------------------------------------------------

/// Locate every `text/*` part in `body`, given the message's own headers.
///
/// Ranges are into `body`, not into the whole message.
pub fn walk(message_headers: &HeaderBlock, body: &[u8]) -> Vec<Node> {
    let mut out = Vec::new();
    walk_part(message_headers, None, 0..body.len(), body, 0, &mut out);
    out
}

fn walk_part(
    part_headers: &HeaderBlock,
    headers_span: Option<Range<usize>>,
    content: Range<usize>,
    body: &[u8],
    depth: usize,
    out: &mut Vec<Node>,
) {
    let content_type = part_headers
        .get("Content-Type")
        .map(|v| ContentType::parse(&v))
        .unwrap_or_else(ContentType::default_for_a_part);

    // §6.4: "Signed or encrypted parts … are never rewritten; rewriting would
    // invalidate them." Checked before anything else, so no path reaches inside
    // one.
    if let Some(reason) = protected(&content_type) {
        out.push(Node::Excluded(reason));
        return;
    }

    if content_type.kind == "multipart" {
        if depth >= MAX_DEPTH {
            return;
        }
        let Some(boundary) = content_type.param("boundary") else {
            // A multipart with no boundary cannot be split. Opaque.
            return;
        };
        for part in split_multipart(body, content, boundary) {
            let bytes = &body[part.clone()];
            let split = headers::split(bytes);
            let header_len = bytes.len() - split.body.len();
            let child_headers = part.start..part.start + header_len;
            let child_content = child_headers.end..part.end;
            walk_part(
                &split.headers,
                Some(child_headers),
                child_content,
                body,
                depth + 1,
                out,
            );
        }
        return;
    }

    // Out of scope rather than excluded — see the module comment. A nested
    // message and anything the sender marked as an attachment are what §6.4's
    // "attachments … are never touched" is about.
    if content_type.kind == "message" || is_attachment(part_headers) {
        return;
    }

    if !content_type.is_text() {
        return;
    }

    out.push(Node::Text(TextPart {
        content,
        headers: headers_span,
        content_type,
        transfer_encoding: part_headers.get("Content-Transfer-Encoding"),
        length_headers: LENGTH_HEADERS
            .into_iter()
            .filter(|h| part_headers.contains(h))
            .collect(),
    }));
}

/// §6.4's never-rewrite list.
fn protected(ct: &ContentType) -> Option<SkipReason> {
    match (ct.kind.as_str(), ct.subtype.as_str()) {
        ("multipart", "signed") => Some(SkipReason::Signed),
        ("multipart", "encrypted") => Some(SkipReason::Encrypted),
        // `application/pkcs7-mime` wraps a whole message, signed or enveloped;
        // `application/pkcs7-signature` is the detached signature itself.
        ("application", s) if s.starts_with("pkcs7-") || s.starts_with("x-pkcs7-") => {
            Some(SkipReason::Signed)
        }
        _ => None,
    }
}

/// RFC 2183 `Content-Disposition: attachment; filename=…`.
///
/// Parsed by hand rather than through [`ContentType::parse`]: a disposition is a
/// bare token with parameters, not a `type/subtype`, so the content-type parser
/// would read `attachment` as malformed and fall back to
/// `application/octet-stream`.
fn is_attachment(headers: &HeaderBlock) -> bool {
    headers
        .get("Content-Disposition")
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("attachment")
        })
        .unwrap_or(false)
}

/// RFC 2046 §5.1.1 — the ranges of the parts between the boundary delimiters.
///
/// The preamble (before the first delimiter) and the epilogue (after the closing
/// one) are not parts and are not returned, which is what leaves them untouched.
fn split_multipart(body: &[u8], within: Range<usize>, boundary: &str) -> Vec<Range<usize>> {
    let dash_boundary = format!("--{boundary}");
    let close = format!("--{boundary}--");

    let mut parts = Vec::new();
    let mut open: Option<usize> = None;
    let mut offset = within.start;

    while offset < within.end {
        let line_end = match body[offset..within.end].iter().position(|b| *b == b'\n') {
            Some(i) => offset + i + 1,
            None => within.end,
        };
        // RFC 2046: "transport padding" — trailing whitespace — is allowed after
        // the boundary and is not part of it.
        let line = trim_line(&body[offset..line_end]);

        let is_close = line == close.as_bytes();
        let is_delimiter = is_close || line == dash_boundary.as_bytes();

        if is_delimiter {
            if let Some(start) = open.take() {
                parts.push(start..strip_boundary_crlf(body, start, offset));
            }
            if is_close {
                return parts;
            }
            open = Some(line_end);
        }

        offset = line_end;
    }

    // No closing delimiter. The last part runs to the end of the body — RFC 2046
    // calls this malformed, but a message that arrived is a message that has to
    // leave, and truncating it would lose content.
    if let Some(start) = open {
        if start < within.end {
            parts.push(start..within.end);
        }
    }
    parts
}

/// A line without its terminator or any transport padding.
fn trim_line(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && matches!(line[end - 1], b'\r' | b'\n' | b' ' | b'\t') {
        end -= 1;
    }
    &line[..end]
}

/// RFC 2046: "The CRLF preceding the boundary delimiter line is conceptually
/// attached to the boundary." So a part's content stops before it, and a part
/// whose content is empty stays empty.
fn strip_boundary_crlf(body: &[u8], start: usize, delimiter_at: usize) -> usize {
    if delimiter_at >= start + 2 && &body[delimiter_at - 2..delimiter_at] == b"\r\n" {
        delimiter_at - 2
    } else if delimiter_at > start && body[delimiter_at - 1] == b'\n' {
        delimiter_at - 1
    } else {
        delimiter_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_of(raw: &str) -> HeaderBlock {
        headers::split(raw.as_bytes()).headers
    }

    fn text_parts(headers: &str, body: &str) -> Vec<TextPart> {
        walk(&headers_of(headers), body.as_bytes())
            .into_iter()
            .filter_map(|n| match n {
                Node::Text(p) => Some(p),
                Node::Excluded(_) => None,
            })
            .collect()
    }

    fn slice<'a>(body: &'a str, r: &Range<usize>) -> &'a str {
        &body[r.clone()]
    }

    // -- Content-Type parsing ---------------------------------------------

    #[test]
    fn a_content_type_parses_into_a_type_a_subtype_and_parameters() {
        let ct = ContentType::parse("text/plain; charset=utf-8");
        assert_eq!(ct.kind, "text");
        assert_eq!(ct.subtype, "plain");
        assert_eq!(ct.param("charset"), Some("utf-8"));
    }

    #[test]
    fn parameters_are_found_however_they_are_written() {
        let ct =
            ContentType::parse("Multipart/Mixed; BOUNDARY=\"a=b;c\"; charset = ISO-8859-1 ; x=1");
        assert_eq!(ct.kind, "multipart");
        assert_eq!(ct.subtype, "mixed");
        // The quoted boundary keeps the characters that would otherwise end it.
        assert_eq!(ct.param("boundary"), Some("a=b;c"));
        assert_eq!(ct.param("charset"), Some("ISO-8859-1"));
        assert_eq!(ct.param("x"), Some("1"));
    }

    #[test]
    fn a_malformed_content_type_is_not_text() {
        // RFC 2045 §5.2's fallback. "not text" is the safe answer: the part is
        // left alone.
        assert!(!ContentType::parse("nonsense").is_text());
    }

    #[test]
    fn an_absent_content_type_makes_a_part_text_plain() {
        // RFC 2045 §5.2. A plain RFC 822 message with no MIME headers at all is
        // still in §6.4's scope, and it is the commonest message there is.
        let parts = text_parts("From: a@b\r\n", "Hello.\r\n");
        assert_eq!(parts.len(), 1);
        assert!(parts[0].headers.is_none(), "the message's own header block");
        assert_eq!(parts[0].content, 0..8);
    }

    // -- the single-part case ---------------------------------------------

    #[test]
    fn a_single_text_part_is_the_whole_body() {
        let body = "Hello.\r\nTrack it at https://oldbrand.com/x\r\n";
        let parts = text_parts("Content-Type: text/plain; charset=utf-8\r\n", body);
        assert_eq!(parts.len(), 1);
        assert_eq!(slice(body, &parts[0].content), body);
        assert_eq!(parts[0].content_type.param("charset"), Some("utf-8"));
    }

    #[test]
    fn a_single_non_text_part_yields_nothing() {
        assert!(text_parts("Content-Type: application/pdf\r\n", "%PDF-1.4\r\n").is_empty());
    }

    #[test]
    fn text_html_is_in_scope_and_so_is_any_other_text_subtype() {
        assert_eq!(
            text_parts("Content-Type: text/html\r\n", "<p>hi</p>").len(),
            1
        );
        assert_eq!(
            text_parts("Content-Type: text/calendar\r\n", "BEGIN:VCALENDAR").len(),
            1
        );
    }

    // -- multipart ---------------------------------------------------------

    const MULTIPART_HEADERS: &str = "Content-Type: multipart/mixed; boundary=\"b1\"\r\n";
    const MULTIPART_BODY: &str = concat!(
        "This is the preamble.\r\n",
        "--b1\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "the text part\r\n",
        "--b1\r\n",
        "Content-Type: application/pdf\r\n",
        "Content-Disposition: attachment; filename=x.pdf\r\n",
        "\r\n",
        "%PDF-1.4\r\n",
        "--b1--\r\n",
        "This is the epilogue.\r\n",
    );

    #[test]
    fn a_multipart_yields_only_its_text_parts() {
        let parts = text_parts(MULTIPART_HEADERS, MULTIPART_BODY);
        assert_eq!(parts.len(), 1);
        assert_eq!(slice(MULTIPART_BODY, &parts[0].content), "the text part");
    }

    #[test]
    fn a_parts_own_header_block_is_located_separately_from_its_content() {
        let parts = text_parts(MULTIPART_HEADERS, MULTIPART_BODY);
        let headers = parts[0].headers.clone().expect("a nested part has its own");
        assert_eq!(
            slice(MULTIPART_BODY, &headers),
            "Content-Type: text/plain; charset=utf-8\r\n\r\n"
        );
    }

    #[test]
    fn the_crlf_before_a_boundary_belongs_to_the_boundary() {
        // RFC 2046. Getting this wrong by two bytes puts a blank line into every
        // rewritten part, which is exactly the kind of change §1.1 forbids.
        let parts = text_parts(MULTIPART_HEADERS, MULTIPART_BODY);
        assert!(!slice(MULTIPART_BODY, &parts[0].content).ends_with("\r\n"));
    }

    #[test]
    fn nested_multiparts_are_followed() {
        let body = concat!(
            "--outer\r\n",
            "Content-Type: multipart/alternative; boundary=inner\r\n",
            "\r\n",
            "--inner\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "plain version\r\n",
            "--inner\r\n",
            "Content-Type: text/html\r\n",
            "\r\n",
            "<p>html version</p>\r\n",
            "--inner--\r\n",
            "--outer--\r\n",
        );
        let parts = text_parts("Content-Type: multipart/mixed; boundary=outer\r\n", body);
        assert_eq!(parts.len(), 2);
        assert_eq!(slice(body, &parts[0].content), "plain version");
        assert_eq!(slice(body, &parts[1].content), "<p>html version</p>");
    }

    #[test]
    fn a_text_part_marked_as_an_attachment_is_out_of_scope() {
        // §6.4: "Attachments and non-text parts are never touched." An attached
        // .txt or .csv is an attachment even though it is text/*.
        let body = concat!(
            "--b1\r\n",
            "Content-Type: text/csv\r\n",
            "Content-Disposition: attachment; filename=report.csv\r\n",
            "\r\n",
            "a,b,c\r\n",
            "--b1--\r\n",
        );
        assert!(text_parts(MULTIPART_HEADERS, body).is_empty());
    }

    #[test]
    fn an_inline_text_part_is_still_in_scope() {
        let body = concat!(
            "--b1\r\n",
            "Content-Type: text/plain\r\n",
            "Content-Disposition: inline\r\n",
            "\r\n",
            "body text\r\n",
            "--b1--\r\n",
        );
        assert_eq!(text_parts(MULTIPART_HEADERS, body).len(), 1);
    }

    #[test]
    fn a_forwarded_message_is_an_attachment_and_is_not_descended_into() {
        let body = concat!(
            "--b1\r\n",
            "Content-Type: message/rfc822\r\n",
            "\r\n",
            "From: someone@else.com\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "the forwarded text\r\n",
            "--b1--\r\n",
        );
        assert!(text_parts(MULTIPART_HEADERS, body).is_empty());
    }

    #[test]
    fn a_part_with_no_headers_at_all_is_text_plain_by_default() {
        let body = "--b1\r\n\r\njust text\r\n--b1--\r\n";
        let parts = text_parts(MULTIPART_HEADERS, body);
        assert_eq!(parts.len(), 1);
        assert_eq!(slice(body, &parts[0].content), "just text");
    }

    #[test]
    fn an_empty_part_has_an_empty_range_rather_than_a_negative_one() {
        let body = "--b1\r\n--b1--\r\n";
        // No content at all: nothing to rewrite, and no panic locating it.
        let parts = text_parts(MULTIPART_HEADERS, body);
        assert!(parts.iter().all(|p| p.content.start <= p.content.end));
    }

    #[test]
    fn a_multipart_with_no_closing_delimiter_still_yields_its_last_part() {
        let body = "--b1\r\nContent-Type: text/plain\r\n\r\ntruncated\r\n";
        let parts = text_parts(MULTIPART_HEADERS, body);
        assert_eq!(parts.len(), 1);
        assert_eq!(slice(body, &parts[0].content), "truncated\r\n");
    }

    #[test]
    fn a_multipart_with_no_boundary_parameter_is_opaque() {
        assert!(text_parts(
            "Content-Type: multipart/mixed\r\n",
            "--x\r\nhi\r\n--x--\r\n"
        )
        .is_empty());
    }

    #[test]
    fn a_boundary_that_prefixes_another_is_not_confused_with_it() {
        // RFC 2046 allows this, and a `starts_with` test would split the message
        // in the wrong place.
        let body = concat!(
            "--b1\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "--b1x is not a delimiter\r\n",
            "--b1--\r\n",
        );
        let parts = text_parts(MULTIPART_HEADERS, body);
        assert_eq!(parts.len(), 1);
        assert_eq!(slice(body, &parts[0].content), "--b1x is not a delimiter");
    }

    #[test]
    fn transport_padding_after_a_boundary_is_tolerated() {
        let body = "--b1  \r\nContent-Type: text/plain\r\n\r\ntext\r\n--b1--\t\r\n";
        let parts = text_parts(MULTIPART_HEADERS, body);
        assert_eq!(parts.len(), 1);
        assert_eq!(slice(body, &parts[0].content), "text");
    }

    // -- §6.4's never-rewrite list -----------------------------------------

    #[test]
    fn a_signed_message_is_excluded_rather_than_descended_into() {
        // "Rewriting would invalidate them." The text part inside is real and
        // matchable; the point is that we do not reach it.
        let body = concat!(
            "--sig\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "signed text\r\n",
            "--sig\r\n",
            "Content-Type: application/pkcs7-signature\r\n",
            "\r\n",
            "MIIF...\r\n",
            "--sig--\r\n",
        );
        let nodes = walk(
            &headers_of("Content-Type: multipart/signed; boundary=sig; protocol=\"application/pkcs7-signature\"\r\n"),
            body.as_bytes(),
        );
        assert_eq!(nodes, vec![Node::Excluded(SkipReason::Signed)]);
    }

    #[test]
    fn an_encrypted_message_is_excluded() {
        let nodes = walk(
            &headers_of("Content-Type: multipart/encrypted; boundary=e\r\n"),
            b"--e\r\nContent-Type: text/plain\r\n\r\nx\r\n--e--\r\n",
        );
        assert_eq!(nodes, vec![Node::Excluded(SkipReason::Encrypted)]);
    }

    #[test]
    fn an_smime_wrapped_message_is_excluded() {
        let nodes = walk(
            &headers_of("Content-Type: application/pkcs7-mime; smime-type=signed-data\r\n"),
            b"MIIF...\r\n",
        );
        assert_eq!(nodes, vec![Node::Excluded(SkipReason::Signed)]);
    }

    #[test]
    fn a_signed_part_nested_inside_a_multipart_is_excluded_too() {
        let body = concat!(
            "--b1\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "the covering note\r\n",
            "--b1\r\n",
            "Content-Type: multipart/signed; boundary=sig\r\n",
            "\r\n",
            "--sig\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "signed text\r\n",
            "--sig--\r\n",
            "--b1--\r\n",
        );
        let nodes = walk(&headers_of(MULTIPART_HEADERS), body.as_bytes());
        assert_eq!(nodes.len(), 2);
        assert!(matches!(nodes[0], Node::Text(_)));
        assert_eq!(nodes[1], Node::Excluded(SkipReason::Signed));
    }

    // -- structural fidelity, which is the whole argument for ranges --------

    #[test]
    fn the_spans_of_a_message_reassemble_it_byte_for_byte() {
        // Every byte of the body is either inside exactly one part's ranges or
        // outside all of them, and copying the untouched spans back in order
        // reproduces the original. This is what lets body.rs claim that a part
        // which did not match is unchanged.
        for (headers, body) in [
            ("Content-Type: text/plain\r\n", "Hello.\r\n"),
            (MULTIPART_HEADERS, MULTIPART_BODY),
            (
                "Content-Type: multipart/mixed; boundary=outer\r\n",
                concat!(
                    "--outer\r\n",
                    "Content-Type: multipart/alternative; boundary=inner\r\n",
                    "\r\n",
                    "--inner\r\n\r\nplain\r\n",
                    "--inner\r\nContent-Type: text/html\r\n\r\n<p>h</p>\r\n",
                    "--inner--\r\n",
                    "--outer--\r\n",
                ),
            ),
        ] {
            let parts = text_parts(headers, body);
            let mut rebuilt = String::new();
            let mut cursor = 0;
            for part in &parts {
                assert!(part.content.start >= cursor, "parts must not overlap");
                rebuilt.push_str(&body[cursor..part.content.start]);
                rebuilt.push_str(slice(body, &part.content));
                cursor = part.content.end;
            }
            rebuilt.push_str(&body[cursor..]);
            assert_eq!(rebuilt, body, "spans do not reassemble:\n{body}");
        }
    }

    #[test]
    fn nesting_deeper_than_the_cap_is_treated_as_opaque_rather_than_recursing() {
        // The input is attacker-influenced; this is a stack, not a preference.
        let mut body = String::new();
        for i in 0..MAX_DEPTH + 5 {
            body.push_str(&format!(
                "--b{i}\r\nContent-Type: multipart/mixed; boundary=b{}\r\n\r\n",
                i + 1
            ));
        }
        let nodes = walk(
            &headers_of("Content-Type: multipart/mixed; boundary=b0\r\n"),
            body.as_bytes(),
        );
        assert!(nodes.is_empty());
    }
}
