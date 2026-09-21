//! `header_rewrites` (D-089) — a regex replacement over one named header's value.
//!
//! **Not in `SPEC.md`.** §6.2 gives headers two mechanisms, `set_headers` (an
//! absolute template, §6.3) and `remove_headers`, and neither can move the host
//! of a `List-Unsubscribe` URL while keeping the per-message token after it.
//! This is the third, with `body_rewrites`' shape. O-16 in `DECISIONS.md` is
//! the question it puts to the spec's author.
//!
//! ## Where it runs
//!
//! After `remove_headers` and before `set_headers` (§6.1 steps 5 and 6), so an
//! explicit `set_headers` value still wins and a removed header is simply not
//! there to rewrite. Templates keep reading the message **as it arrived**: a
//! `set_headers` value built from `{{original.header["List-Unsubscribe"]}}`
//! sees the inbound value, not this module's output, for the same reason
//! `set_headers` entries cannot see each other's (`rewrite/mod.rs`).
//!
//! ## Matching is on the decoded value, not the raw bytes
//!
//! §6.4 rejects raw-byte matching for bodies because one text has many
//! encodings, and the one the pattern was written against is not the one that
//! arrives. Headers have the same problem in two forms: **folding** (RFC 5322
//! §2.2.3 — a long value arrives split across lines at a point the sender
//! chose) and **RFC 2047 encoded-words** (`Subject: =?UTF-8?Q?Gr=C3=BC=C3=9Fe?=`,
//! or one word split into two encoded-words at a byte boundary). So a value is
//! unfolded, and every whitespace-delimited token that is exactly an
//! encoded-word is decoded — with the whitespace between two adjacent ones
//! dropped, per RFC 2047 §6.2 — before any pattern sees it. The decoding itself
//! is `mail-parser`'s, as it is everywhere else in this engine: decoding
//! RFC 2047 by hand is where the bugs live.
//!
//! Only whitespace-delimited tokens are decoded, which is RFC 2047 §5's own
//! rule for where an encoded-word may appear. `=?…?=` inside a URL in angle
//! brackets is not an encoded-word and is left as text.
//!
//! ## Nothing it does not change is touched (D-039)
//!
//! A header no entry names is never looked at. A header an entry names but no
//! pattern changes keeps its **original bytes** — folding, spelling and
//! encoded-words included — because "unchanged" is decided on the decoded text
//! and the field is only replaced when that text differs. Re-encoding is not
//! the identity (D-043), so it happens only to a value that actually changed.
//!
//! ## Writing it back
//!
//! A changed value is re-serialised: ASCII tokens as themselves, and each run
//! of tokens carrying non-ASCII or control characters — or text that would read
//! back as an encoded-word — as `encode::encode_words`' deterministic B-encoded
//! words. `decode(encode(t)) == t`, which is what lets §6.6's property on the
//! *text* carry over to the bytes. A result that still cannot be written as a
//! conformant field (a single token past RFC 5322's 998-octet line) is not
//! emitted: the header is left as it arrived, logged and counted, exactly as
//! §6.4 treats a part it cannot write back (D-045).
//!
//! ## Escaping (D-038)
//!
//! Operator text is validated at startup: a replacement's literal text must be
//! printable ASCII or tab. Message-derived text reaches the output only through
//! `$1`-style capture references, and each capture is neutralised **as it is
//! placed** — a CR, LF or other control character becomes a space — never by
//! scrubbing the finished value, where it could no longer be told apart from
//! the operator's text.

use mail_parser::decoders::charsets::map::charset_decoder;
use mail_parser::parsers::MessageStream;

use crate::config::HeaderRewrite;

use super::encode;

/// Why a header an entry names was left as it arrived.
///
/// The `reason` label on `simmer_header_rewrite_skipped_total`. Every one is a
/// case where the operator's configured rewrite silently did not happen, which
/// is the thing worth counting (D-043's argument, for headers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Not UTF-8, or a token shaped like an encoded-word that does not decode.
    MalformedEncoding,
    /// An encoded-word in a charset `mail-parser` does not know.
    UnsupportedCharset,
    /// Raw non-ASCII bytes (RFC 6532). Writing them back as RFC 2047 would
    /// change the header's encoding, which §1.1 cannot hand back to the
    /// application any more than D-045 can a part's transfer encoding.
    Raw8bit,
    /// The rewritten value cannot be written as a conformant RFC 5322 field.
    Unrepresentable,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::MalformedEncoding => "malformed_encoding",
            SkipReason::UnsupportedCharset => "unsupported_charset",
            SkipReason::Raw8bit => "raw_8bit",
            SkipReason::Unrepresentable => "unrepresentable",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            SkipReason::MalformedEncoding => {
                "the header's value is not UTF-8 or carries an encoded-word that does not decode"
            }
            SkipReason::UnsupportedCharset => {
                "the header carries an encoded-word in a charset Simmer cannot decode"
            }
            SkipReason::Raw8bit => {
                "the header carries raw non-ASCII bytes, and Simmer does not change a header's \
                 encoding"
            }
            SkipReason::Unrepresentable => {
                "the rewritten value cannot be written as an RFC 5322-conformant header field"
            }
        }
    }
}

/// What a route's `header_rewrites` did to one instance of a header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// No pattern changed the decoded value: the field keeps its bytes.
    Unchanged,
    /// The new value, ready to write — encoded, conformant, unfolded.
    Rewritten(String),
    Skipped(SkipReason),
}

// ---------------------------------------------------------------------------
// compiled form
// ---------------------------------------------------------------------------

/// A route's `header_rewrites`, compiled.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    header: String,
    pattern: regex::Regex,
    replacement: Replacement,
}

/// A replacement string, parsed once. Same syntax as `body_rewrites` — the
/// regex crate's `$1`, `$name`, `${name}` and `$$` — but expanded here, because
/// the crate's own expansion gives no hook at the substitution boundary.
#[derive(Debug, Clone)]
struct Replacement {
    source: String,
    pieces: Vec<Piece>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Literal(String),
    /// Resolved to an index at compile time; see [`Replacement::parse`].
    Group(usize),
}

/// Why an entry did not compile. `field` is relative to the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryError {
    pub index: usize,
    /// `header`, `pattern` or `replacement`.
    pub field: &'static str,
    pub message: String,
}

impl Rules {
    /// Every problem in every entry, so §4.2 can report all of them at once.
    pub fn compile(specs: &[HeaderRewrite]) -> Result<Rules, Vec<EntryError>> {
        let mut rules = Vec::with_capacity(specs.len());
        let mut errors = Vec::new();

        for (index, spec) in specs.iter().enumerate() {
            if let Some(problem) = field_name_problem(&spec.header) {
                errors.push(EntryError {
                    index,
                    field: "header",
                    message: problem.to_string(),
                });
            }

            let pattern = match regex::Regex::new(&spec.pattern) {
                Ok(p) => p,
                Err(e) => {
                    errors.push(EntryError {
                        index,
                        field: "pattern",
                        message: super::CompileErrorKind::pattern(&e).to_string(),
                    });
                    continue;
                }
            };

            match Replacement::parse(&spec.replacement, &pattern, &spec.header) {
                Ok(replacement) => rules.push(Rule {
                    header: spec.header.clone(),
                    pattern,
                    replacement,
                }),
                Err(message) => errors.push(EntryError {
                    index,
                    field: "replacement",
                    message,
                }),
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

    /// The distinct headers named, in first-mention order and the operator's
    /// spelling.
    pub fn headers(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for rule in &self.rules {
            if !out.iter().any(|h| h.eq_ignore_ascii_case(&rule.header)) {
                out.push(&rule.header);
            }
        }
        out
    }

    pub fn names(&self, header: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.header.eq_ignore_ascii_case(header))
    }

    fn for_header<'a>(&'a self, header: &'a str) -> impl Iterator<Item = &'a Rule> + 'a {
        self.rules
            .iter()
            .filter(move |r| r.header.eq_ignore_ascii_case(header))
    }

    /// Rewrite one instance of `header`, given its raw value — everything
    /// after the colon, folds and line ending included.
    pub fn apply(&self, header: &str, raw_value: &[u8]) -> Edit {
        if !self.names(header) {
            return Edit::Unchanged;
        }
        let Ok(raw) = std::str::from_utf8(raw_value) else {
            return Edit::Skipped(SkipReason::MalformedEncoding);
        };
        if !raw.is_ascii() {
            return Edit::Skipped(SkipReason::Raw8bit);
        }

        let text = match decode(&unfold(raw)) {
            Ok(text) => text,
            Err(reason) => return Edit::Skipped(reason),
        };
        let Some(rewritten) = self.apply_text(header, &text) else {
            return Edit::Unchanged;
        };

        match write_back(header, rewritten.trim()) {
            Some(value) => Edit::Rewritten(value),
            None => Edit::Skipped(SkipReason::Unrepresentable),
        }
    }

    /// Every rule for `header`, in order, over decoded text. `None` means the
    /// text came out unchanged — which is what keeps the original bytes.
    fn apply_text(&self, header: &str, text: &str) -> Option<String> {
        if !self.for_header(header).any(|r| r.pattern.is_match(text)) {
            return None;
        }
        let mut current = text.to_string();
        for rule in self.for_header(header) {
            let next = rule
                .pattern
                .replace_all(&current, |caps: &regex::Captures<'_>| {
                    rule.replacement.expand(caps)
                });
            if let std::borrow::Cow::Owned(next) = next {
                current = next;
            }
        }
        (current != text).then_some(current)
    }

    /// §6.6 applied to the rules themselves, one header at a time: does running
    /// them over their own output change it again? D-046's check, for headers,
    /// and for the same reason fatal with no override — there is no
    /// migration-only reading of a rule that matches what it just wrote.
    ///
    /// Returns `(header, once, twice)` for each header whose rules are not a
    /// fixed point.
    pub fn fixed_point_violations(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for header in self.headers() {
            let probe = self.probe_text(header);
            let once = self.apply_text(header, &probe).unwrap_or(probe);
            let twice = self
                .apply_text(header, &once)
                .unwrap_or_else(|| once.clone());
            if once != twice {
                out.push((header.to_string(), once, twice));
            }
        }
        out
    }

    /// What an unstable rule re-matches: its own replacement — as written, and
    /// with capture references dropped — alone and adjacent to the others'.
    /// `body::Rules::probe_text`'s construction, on one line.
    fn probe_text(&self, header: &str) -> String {
        let mut words: Vec<String> = Vec::new();
        for rule in self.for_header(header) {
            words.push(rule.replacement.source.clone());
            words.push(rule.replacement.literal_text());
        }
        words.push(words.concat());
        words.join(" ")
    }

    /// A raw header value for the startup probe (`stability::probe_message`):
    /// the probe text, plus one encoded-word so the RFC 2047 path runs too.
    pub fn probe_value(&self, header: &str) -> String {
        format!("{} =?UTF-8?B?w6k=?=", self.probe_text(header))
    }
}

impl Replacement {
    /// Parse with the regex crate's interpolation rules
    /// (`regex_automata::util::interpolate::string`), refusing two things the
    /// crate accepts silently:
    ///
    /// - a reference to a group the pattern does not have — `$1a` is the group
    ///   *named* `1a`, and the crate expands it to nothing. D-034's reasoning:
    ///   a typo that renders empty is invisible until someone reads the mail;
    /// - literal text that cannot appear in a header value, or a run of it with
    ///   no whitespace to fold at that is longer than RFC 5322's line limit —
    ///   every result would carry it, so every result would be refused.
    fn parse(source: &str, pattern: &regex::Regex, header: &str) -> Result<Replacement, String> {
        let mut pieces = Vec::new();
        let mut literal = String::new();
        let mut rest = source;

        while let Some(i) = rest.find('$') {
            literal.push_str(&rest[..i]);
            rest = &rest[i..];

            if rest.as_bytes().get(1) == Some(&b'$') {
                literal.push('$');
                rest = &rest[2..];
                continue;
            }
            let Some((name, len)) = cap_ref(rest) else {
                literal.push('$');
                rest = &rest[1..];
                continue;
            };
            let index = match name.parse::<usize>() {
                Ok(i) if i < pattern.captures_len() => i,
                Ok(i) => {
                    return Err(format!(
                        "refers to group {i}, but the pattern has {} (§4.2, D-089)",
                        pattern.captures_len() - 1
                    ))
                }
                Err(_) => pattern
                    .capture_names()
                    .position(|n| n == Some(name))
                    .ok_or_else(|| unknown_group(name))?,
            };
            if !literal.is_empty() {
                pieces.push(Piece::Literal(std::mem::take(&mut literal)));
            }
            pieces.push(Piece::Group(index));
            rest = &rest[len..];
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            pieces.push(Piece::Literal(literal));
        }

        for piece in &pieces {
            if let Piece::Literal(text) = piece {
                if let Some(c) = text.chars().find(|c| !is_field_char(*c)) {
                    return Err(format!(
                        "contains {c:?}, which cannot appear in a header value: the rewrite \
                         would emit a field that is not RFC 5322-conformant. Replacement text \
                         must be printable ASCII (D-089)"
                    ));
                }
            }
        }

        let longest_run = pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Literal(text) => text.split([' ', '\t']).map(str::len).max(),
                Piece::Group(_) => None,
            })
            .max()
            .unwrap_or(0);
        if header.len() + 2 + longest_run > 998 {
            return Err(format!(
                "contains {longest_run} characters with no space to fold at, so no result \
                 could fit RFC 5322's 998-octet line (D-089)"
            ));
        }

        Ok(Replacement {
            source: source.to_string(),
            pieces,
        })
    }

    /// The substitution boundary. A capture is message text, so it is
    /// neutralised as it is placed; literal text was validated at startup.
    fn expand(&self, caps: &regex::Captures<'_>) -> String {
        let mut out = String::new();
        for piece in &self.pieces {
            match piece {
                Piece::Literal(text) => out.push_str(text),
                Piece::Group(i) => {
                    if let Some(m) = caps.get(*i) {
                        out.push_str(&neutralise(m.as_str()));
                    }
                }
            }
        }
        out
    }

    fn literal_text(&self) -> String {
        self.pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Literal(t) => Some(t.as_str()),
                Piece::Group(_) => None,
            })
            .collect()
    }
}

/// The D-034-style refusal, with the regex crate's classic trap spelled out:
/// `$1a` is the group named `1a`, not group 1 followed by `a`.
fn unknown_group(name: &str) -> String {
    let short = |t: &str| -> String {
        if t.chars().count() > 16 {
            format!("{}…", t.chars().take(16).collect::<String>())
        } else {
            t.to_string()
        }
    };
    let digits = name.len() - name.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let mut message = format!(
        "refers to group '{}', which the pattern does not define",
        short(name)
    );
    if digits > 0 {
        let (n, rest) = name.split_at(digits);
        message.push_str(&format!(
            ". For group {n} followed by text, write ${{{n}}}{}",
            short(rest)
        ));
    }
    message.push_str(" (D-089)");
    message
}

/// `$name` or `${name}` at the start of `rest` (which starts with `$`), as the
/// regex crate reads it. Returns the name and the reference's length.
fn cap_ref(rest: &str) -> Option<(&str, usize)> {
    let bytes = rest.as_bytes();
    if bytes.len() <= 1 {
        return None;
    }
    if bytes[1] == b'{' {
        let close = rest[2..].find('}')?;
        return Some((&rest[2..2 + close], close + 3));
    }
    let end = 1 + bytes[1..]
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
        .count();
    (end > 1).then(|| (&rest[1..end], end))
}

/// A capture's CR, LF and other control characters become one space each, a
/// CRLF pair one space — `encode::sanitise`'s treatment of a rendered template,
/// without its trim, since a capture sits inside a value rather than ending one.
fn neutralise(text: &str) -> String {
    text.replace("\r\n", " ")
        .chars()
        .map(|c| if c.is_control() && c != '\t' { ' ' } else { c })
        .collect()
}

/// RFC 5322 `field-name`: printable ASCII except the colon.
fn field_name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("must name a header");
    }
    if !name.bytes().all(|b| (33..=126).contains(&b) && b != b':') {
        return Some("is not a header field name: printable ASCII, no spaces, no colon");
    }
    None
}

/// What may appear in an unstructured header value as written: VCHAR and WSP.
fn is_field_char(c: char) -> bool {
    c == '\t' || (' '..='~').contains(&c)
}

// ---------------------------------------------------------------------------
// the value, as text and back
// ---------------------------------------------------------------------------

/// Unfolding per RFC 5322 §2.2.3: the line break goes, the whitespace after it
/// stays. `headers::unfold`'s rule, applied to a value.
fn unfold(raw: &str) -> String {
    raw.replace("\r\n", "").replace('\n', "").trim().to_string()
}

/// Splits into alternating whitespace and non-whitespace runs, keeping both.
fn tokens(text: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_ws = None;
    for (i, c) in text.char_indices() {
        let ws = c == ' ' || c == '\t';
        match in_ws {
            Some(prev) if prev != ws => {
                out.push((prev, &text[start..i]));
                start = i;
            }
            _ => {}
        }
        in_ws = Some(ws);
    }
    if let Some(ws) = in_ws {
        out.push((ws, &text[start..]));
    }
    out
}

fn looks_encoded(token: &str) -> bool {
    token.len() >= 8 && token.starts_with("=?") && token.ends_with("?=")
}

/// RFC 2047 decoding of whitespace-delimited encoded-words.
fn decode(value: &str) -> Result<String, SkipReason> {
    if !value.contains("=?") {
        return Ok(value.to_string());
    }

    let mut out = String::with_capacity(value.len());
    let mut pending_ws = "";
    let mut after_encoded = false;

    for (ws, token) in tokens(value) {
        if ws {
            pending_ws = token;
            continue;
        }
        match decode_word(token)? {
            Some(decoded) => {
                // §6.2: whitespace between two adjacent encoded-words is not
                // part of the text.
                if !after_encoded {
                    out.push_str(pending_ws);
                }
                out.push_str(&decoded);
                after_encoded = true;
            }
            None => {
                out.push_str(pending_ws);
                out.push_str(token);
                after_encoded = false;
            }
        }
        pending_ws = "";
    }
    out.push_str(pending_ws);
    Ok(out)
}

/// `Ok(None)` for a token that is not an encoded-word at all; `Err` for one
/// that is shaped like one and does not decode — guessing there would rewrite
/// text Simmer has not read correctly.
fn decode_word(token: &str) -> Result<Option<String>, SkipReason> {
    if !looks_encoded(token) {
        return Ok(None);
    }
    let charset = token[2..]
        .split('?')
        .next()
        .unwrap_or_default()
        .split('*')
        .next()
        .unwrap_or_default();
    // `mail-parser` decodes UTF-8 itself rather than through its charset map,
    // and decodes an unknown charset as UTF-8 too — so the map alone cannot
    // tell the two apart, and an unknown one would be guessed at.
    let utf8 = UTF8_LABELS.iter().any(|l| l.eq_ignore_ascii_case(charset));
    if !utf8 && charset_decoder(charset.as_bytes()).is_none() {
        return Err(SkipReason::UnsupportedCharset);
    }

    // `decode_rfc2047` expects the stream just past the leading `=`.
    let mut stream = MessageStream::new(&token.as_bytes()[1..]);
    match stream.decode_rfc2047() {
        // Its UTF-8 decode is lossy. A replacement character is bytes that were
        // not UTF-8, and rewriting around them would emit text nobody sent.
        Some(text) if utf8 && text.contains('\u{FFFD}') => Err(SkipReason::MalformedEncoding),
        Some(text) if stream.remaining() == 0 => Ok(Some(text)),
        _ => Err(SkipReason::MalformedEncoding),
    }
}

/// The labels `mail-parser` treats as UTF-8 without a charset-map entry.
const UTF8_LABELS: [&str; 6] = [
    "utf-8",
    "utf8",
    "unicode-1-1-utf-8",
    "unicode11utf8",
    "unicode20utf8",
    "x-unicode20utf8",
];

/// Serialise rewritten text as a conformant field value, or `None`.
///
/// Tokens that are plain printable ASCII are written as themselves. Each
/// maximal run of the others — non-ASCII, a control character, or ASCII that
/// would read back as an encoded-word — is encoded as one unit, spaces
/// included, so the whitespace RFC 2047 §6.2 drops between adjacent
/// encoded-words is only ever whitespace this function inserted.
fn write_back(header: &str, text: &str) -> Option<String> {
    let needs = |t: &str| looks_encoded(t) || !t.chars().all(|c| ('!'..='~').contains(&c));

    let parts = tokens(text);
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < parts.len() {
        let (ws, token) = parts[i];
        if ws || !needs(token) {
            out.push_str(token);
            i += 1;
            continue;
        }
        let mut run = token.to_string();
        let mut j = i + 1;
        while j + 1 < parts.len() && parts[j].0 && needs(parts[j + 1].1) {
            run.push_str(parts[j].1);
            run.push_str(parts[j + 1].1);
            j += 2;
        }
        out.push_str(&encode::encode_words(&run));
        i = j;
    }

    conformant(header, &out).then_some(out)
}

/// RFC 5322 §2.1.1 and §2.2: printable ASCII and WSP only, and every line —
/// once `encode::fold_if_needed` has done what it can — within 998 octets.
pub(crate) fn conformant(header: &str, value: &str) -> bool {
    if !value.chars().all(is_field_char) {
        return false;
    }
    let folded = encode::fold_if_needed(header, value);
    folded.split("\r\n").enumerate().all(|(i, line)| {
        let len = if i == 0 {
            header.len() + 2 + line.len()
        } else {
            line.len()
        };
        len <= 998
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(yaml: &str) -> Rules {
        let specs: Vec<HeaderRewrite> = serde_yaml_ng::from_str(yaml).expect("fixture parses");
        Rules::compile(&specs).expect("fixture compiles")
    }

    fn compile_errors(yaml: &str) -> Vec<EntryError> {
        let specs: Vec<HeaderRewrite> = serde_yaml_ng::from_str(yaml).expect("fixture parses");
        Rules::compile(&specs).expect_err("fixture should not compile")
    }

    /// The case D-089 was written for.
    const UNSUB: &str = r#"
- header: List-Unsubscribe
  pattern: '<https://www\.meddoc\.net/'
  replacement: '<https://link-pmps.healthcarematch.com/'
"#;

    const TOKEN_VALUE: &str =
        " <https://www.meddoc.net/unsub.cfm?13323193_418550_3_9011119906_90535>\r\n";

    #[test]
    fn the_host_moves_and_the_token_is_kept_verbatim() {
        let edit = rules(UNSUB).apply("List-Unsubscribe", TOKEN_VALUE.as_bytes());
        assert_eq!(
            edit,
            Edit::Rewritten(
                "<https://link-pmps.healthcarematch.com/unsub.cfm?13323193_418550_3_9011119906_90535>"
                    .into()
            )
        );
    }

    #[test]
    fn header_names_match_case_insensitively() {
        let edit = rules(UNSUB).apply("list-unsubscribe", TOKEN_VALUE.as_bytes());
        assert!(matches!(edit, Edit::Rewritten(_)), "{edit:?}");
    }

    #[test]
    fn a_header_the_rules_do_not_name_is_not_looked_at() {
        assert_eq!(
            rules(UNSUB).apply("X-Other", TOKEN_VALUE.as_bytes()),
            Edit::Unchanged
        );
    }

    #[test]
    fn a_value_no_pattern_matches_is_unchanged() {
        let edit = rules(UNSUB).apply("List-Unsubscribe", b" <mailto:u@x.example>\r\n");
        assert_eq!(edit, Edit::Unchanged);
    }

    #[test]
    fn a_folded_value_matches_across_the_fold() {
        let raw = b" <https://www.meddoc.net/unsub.cfm?1>,\r\n <mailto:u@x.example>\r\n";
        assert_eq!(
            rules(UNSUB).apply("List-Unsubscribe", raw),
            Edit::Rewritten(
                "<https://link-pmps.healthcarematch.com/unsub.cfm?1>, <mailto:u@x.example>".into()
            )
        );
    }

    #[test]
    fn rules_for_one_header_apply_in_order() {
        let r = rules(
            r#"
- header: X-Stage
  pattern: 'one'
  replacement: 'two'
- header: X-Stage
  pattern: 'two'
  replacement: 'three'
"#,
        );
        assert_eq!(
            r.apply("X-Stage", b" one\r\n"),
            Edit::Rewritten("three".into())
        );
        assert!(r.fixed_point_violations().is_empty());
    }

    // -- idempotence ------------------------------------------------------

    #[test]
    fn applying_the_rewrite_to_its_own_output_changes_nothing() {
        let r = rules(UNSUB);
        let Edit::Rewritten(once) = r.apply("List-Unsubscribe", TOKEN_VALUE.as_bytes()) else {
            panic!("should rewrite");
        };
        assert_eq!(
            r.apply("List-Unsubscribe", once.as_bytes()),
            Edit::Unchanged
        );
        assert!(r.fixed_point_violations().is_empty());
    }

    #[test]
    fn a_rule_that_matches_its_own_output_is_reported() {
        let r = rules(
            r#"
- header: List-Unsubscribe
  pattern: 'meddoc\.net'
  replacement: 'meddoc.net.proxy.example'
"#,
        );
        let v = r.fixed_point_violations();
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].0, "List-Unsubscribe");
        assert_ne!(v[0].1, v[0].2);
    }

    #[test]
    fn an_unstable_rule_that_only_shows_through_a_capture_is_reported() {
        // The replacement as written contains `${1}`, which the pattern does
        // not match; with the reference dropped, it does.
        let r = rules(
            r#"
- header: X-Id
  pattern: 'id=(\w+)'
  replacement: 'id=x${1}'
"#,
        );
        assert_eq!(r.fixed_point_violations().len(), 1);
    }

    // -- conformance ------------------------------------------------------

    #[test]
    fn a_replacement_with_a_line_break_does_not_compile() {
        let errs = compile_errors(
            r#"
- header: X-A
  pattern: 'a'
  replacement: "b\r\nBcc: victim@example.com"
"#,
        );
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].field, "replacement");
        assert!(errs[0].message.contains("RFC 5322"), "{}", errs[0].message);
    }

    #[test]
    fn a_non_ascii_replacement_does_not_compile() {
        let errs = compile_errors(
            r#"
- header: X-A
  pattern: 'a'
  replacement: "é"
"#,
        );
        assert_eq!(errs[0].field, "replacement");
    }

    #[test]
    fn a_replacement_too_long_for_any_line_does_not_compile() {
        let yaml = format!(
            "- header: X-A\n  pattern: 'a'\n  replacement: '{}'\n",
            "x".repeat(995)
        );
        let errs = compile_errors(&yaml);
        assert_eq!(errs[0].field, "replacement");
        assert!(errs[0].message.contains("998"), "{}", errs[0].message);
        // Spaces make it foldable, and so acceptable.
        let spaced = format!(
            "- header: X-A\n  pattern: 'a'\n  replacement: '{} {}'\n",
            "x".repeat(600),
            "x".repeat(600)
        );
        rules(&spaced);
    }

    #[test]
    fn a_result_too_long_for_any_line_is_not_emitted() {
        let long = "x".repeat(1200);
        let r = rules(
            r#"
- header: X-A
  pattern: 'short'
  replacement: 'SHORT'
"#,
        );
        let raw = format!(" short{long}\r\n");
        assert_eq!(
            r.apply("X-A", raw.as_bytes()),
            Edit::Skipped(SkipReason::Unrepresentable)
        );
    }

    #[test]
    fn a_captured_line_break_is_neutralised_at_the_boundary() {
        // An encoded-word may carry a CRLF. Captured and placed, it must not end
        // the field.
        let r = rules(
            r#"
- header: Subject
  pattern: '(?s)^(.*)$'
  replacement: '[x] $1'
"#,
        );
        // "a\r\nBcc: v@x" in UTF-8, base64.
        let edit = r.apply("Subject", b" =?UTF-8?B?YQ0KQmNjOiB2QHg=?=\r\n");
        let Edit::Rewritten(value) = edit else {
            panic!("{edit:?}");
        };
        assert_eq!(value, "[x] a Bcc: v@x");
        assert!(conformant("Subject", &value));
    }

    // -- RFC 2047 ---------------------------------------------------------

    #[test]
    fn an_encoded_word_is_matched_decoded() {
        let r = rules(
            r#"
- header: Subject
  pattern: 'Grüße'
  replacement: 'Hello'
"#,
        );
        assert_eq!(
            r.apply("Subject", b" =?UTF-8?Q?Gr=C3=BC=C3=9Fe_aus_Berlin?=\r\n"),
            Edit::Rewritten("Hello aus Berlin".into())
        );
    }

    #[test]
    fn a_word_split_across_two_encoded_words_still_matches() {
        // RFC 2047 §6.2: the whitespace between adjacent encoded-words is not
        // text. "newsletter" split at "news" / "letter".
        let r = rules(
            r#"
- header: Subject
  pattern: 'newsletter'
  replacement: 'digest'
"#,
        );
        assert_eq!(
            r.apply(
                "Subject",
                b" =?UTF-8?Q?news?=\r\n =?UTF-8?Q?letter?= today\r\n"
            ),
            Edit::Rewritten("digest today".into())
        );
    }

    #[test]
    fn non_ascii_that_survives_the_rewrite_is_re_encoded_and_reads_back_the_same() {
        let r = rules(
            r#"
- header: Subject
  pattern: 'oldbrand'
  replacement: 'newbrand'
"#,
        );
        let edit = r.apply(
            "Subject",
            b" =?UTF-8?Q?Caf=C3=A9_caf=C3=A9?= from oldbrand\r\n",
        );
        let Edit::Rewritten(value) = edit else {
            panic!("{edit:?}");
        };
        assert!(value.is_ascii(), "{value}");
        assert!(value.ends_with(" from newbrand"), "{value}");
        assert_eq!(decode(&value).unwrap(), "Café café from newbrand");
        // ...and a second pass leaves it alone.
        assert_eq!(r.apply("Subject", value.as_bytes()), Edit::Unchanged);
    }

    #[test]
    fn ascii_text_that_would_read_back_as_an_encoded_word_is_encoded() {
        let r = rules(
            r#"
- header: X-A
  pattern: 'old'
  replacement: 'new'
"#,
        );
        // Decodes to the literal text "=?UTF-8?Q?x?=" followed by " old".
        let raw = b" =?UTF-8?B?PT9VVEYtOD9RP3g/PQ==?= old\r\n";
        let Edit::Rewritten(value) = r.apply("X-A", raw) else {
            panic!("should rewrite");
        };
        assert_eq!(decode(&value).unwrap(), "=?UTF-8?Q?x?= new");
    }

    #[test]
    fn an_encoded_word_inside_angle_brackets_is_not_decoded() {
        // RFC 2047 §5: not an encoded-word position. Matched as the text it is.
        let r = rules(UNSUB);
        let raw = b" <https://www.meddoc.net/u?=?x?=>\r\n";
        assert_eq!(
            r.apply("List-Unsubscribe", raw),
            Edit::Rewritten("<https://link-pmps.healthcarematch.com/u?=?x?=>".into())
        );
    }

    #[test]
    fn an_unknown_charset_is_skipped_not_guessed() {
        let r = rules(UNSUB);
        assert_eq!(
            r.apply("List-Unsubscribe", b" =?x-no-such-charset?Q?a?=\r\n"),
            Edit::Skipped(SkipReason::UnsupportedCharset)
        );
    }

    #[test]
    fn a_broken_encoded_word_is_skipped_not_guessed() {
        let r = rules(UNSUB);
        assert_eq!(
            r.apply("List-Unsubscribe", b" =?UTF-8?X?abc?=\r\n"),
            Edit::Skipped(SkipReason::MalformedEncoding)
        );
    }

    #[test]
    fn raw_eight_bit_is_skipped() {
        let r = rules(UNSUB);
        assert_eq!(
            r.apply(
                "List-Unsubscribe",
                " <https://www.meddoc.net/é>\r\n".as_bytes()
            ),
            Edit::Skipped(SkipReason::Raw8bit)
        );
    }

    // -- replacement syntax -------------------------------------------------

    #[test]
    fn replacement_syntax_matches_the_regex_crate() {
        let pattern = regex::Regex::new(r"(?P<host>[a-z.]+)/(\d+)").unwrap();
        let caps = pattern.captures("www.x.example/42").unwrap();
        for source in [
            "$host-$2",
            "${host}x${2}y",
            "$$1 costs $$",
            "trailing $",
            "$ space",
            "${unclosed",
            "$0",
        ] {
            let ours = Replacement::parse(source, &pattern, "X-A")
                .unwrap_or_else(|e| panic!("{source}: {e}"))
                .expand(&caps);
            let mut theirs = String::new();
            caps.expand(source, &mut theirs);
            assert_eq!(ours, theirs, "{source}");
        }
    }

    #[test]
    fn a_reference_to_a_missing_group_does_not_compile() {
        for replacement in ["$2", "$1a", "${nope}"] {
            let yaml = format!("- header: X-A\n  pattern: '(a)'\n  replacement: '{replacement}'\n");
            let errs = compile_errors(&yaml);
            assert_eq!(errs[0].field, "replacement", "{replacement}");
        }
    }

    #[test]
    fn every_bad_entry_is_reported() {
        let errs = compile_errors(
            r#"
- header: "Bad Name"
  pattern: 'a'
  replacement: 'b'
- header: X-B
  pattern: '[unclosed'
  replacement: 'b'
- header: ""
  pattern: 'a'
  replacement: "\n"
"#,
        );
        let fields: Vec<_> = errs.iter().map(|e| (e.index, e.field)).collect();
        assert_eq!(
            fields,
            [
                (0, "header"),
                (1, "pattern"),
                (2, "header"),
                (2, "replacement")
            ]
        );
    }
}
