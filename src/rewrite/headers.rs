//! The header block, as something that can be edited without disturbing what it
//! was not asked to change.
//!
//! ## Why this is not `mail-parser`
//!
//! `mail-parser` reads; this writes. It is used here — and in §5.4 — to *decode*
//! values, because `{{original.subject}}` is specified as the decoded subject and
//! decoding RFC 2047 by hand is where the bugs live. But its parsed form throws
//! away the bytes, and giving it back would mean re-serialising every header in
//! the message from a normalised representation.
//!
//! §12.3 compares raw downstream output byte for byte. A message that arrived
//! with `Subject:  two  spaces`, a lowercase `message-id:`, or a continuation
//! line indented with a tab must leave with all three intact, because none of
//! them is something Simmer was configured to change. So an untouched header is
//! carried as **the original bytes**, never as a re-rendering of a parsed value,
//! and only headers the route actually names are rebuilt.
//!
//! ## The body is not in here
//!
//! [`split`] returns the body as an opaque slice. §6.4 is `body.rs`, and it
//! rewrites that slice only where a `body_rewrites` entry matched — a body in
//! which nothing matched comes back through this module's caller as the same
//! bytes it arrived as. The division is the same one D-039 draws: this module
//! knows nothing about MIME, and the MIME walk knows nothing about the header
//! block except how to read a `Content-Type` out of one.

use super::encode;

/// A parsed header block plus the body it was attached to.
pub struct Message<'a> {
    pub headers: HeaderBlock,
    /// Everything after the blank line, verbatim.
    pub body: &'a [u8],
}

/// Split a buffered message into an editable header block and an opaque body.
///
/// The buffer is CRLF-normalised on ingest (`smtp::session::read_data_inner`
/// strips whatever line ending arrived and re-adds CRLF), so the separator is
/// always `CRLF CRLF`. The bare-LF form is accepted anyway: this function is
/// also reachable from the stability probe and from tests, where messages are
/// written by hand.
pub fn split(raw: &[u8]) -> Message<'_> {
    let (header_bytes, body) = match find_separator(raw) {
        Some((end_of_headers, body_start)) => (&raw[..end_of_headers], &raw[body_start..]),
        // No blank line: the whole thing is headers, and the message has no
        // body. Malformed, but a client that sends it has still been accepted.
        None => (raw, &raw[raw.len()..]),
    };

    Message {
        headers: HeaderBlock::parse(header_bytes),
        body,
    }
}

/// Returns `(end of header bytes, start of body)`.
fn find_separator(raw: &[u8]) -> Option<(usize, usize)> {
    // A message that *begins* with the blank line has no headers at all. Without
    // this case the leading CRLF is parsed as a nameless field and carried to
    // the top of the output, where it terminates the header block before the
    // headers Simmer just wrote — and the message the next pass sees has a body
    // where its headers should be. Found by the §6.6 property test on the
    // smallest input there is.
    if raw.starts_with(b"\r\n") {
        return Some((0, 2));
    }
    if raw.starts_with(b"\n") {
        return Some((0, 1));
    }

    if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((i + 2, i + 4));
    }
    raw.windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| (i + 1, i + 2))
}

/// An ordered list of header fields.
///
/// Order is preserved through every operation, because header order is part of
/// what §12.3 compares.
#[derive(Debug, Clone, Default)]
pub struct HeaderBlock {
    fields: Vec<Field>,
}

#[derive(Debug, Clone)]
enum Field {
    /// Carried as the bytes it arrived as, including its own line endings and
    /// any continuation lines.
    Original { name: String, raw: Vec<u8> },
    /// Written by Simmer. Serialised as `Name: value CRLF`, folded only if the
    /// line would otherwise be illegal.
    Written { name: String, value: String },
}

impl Field {
    fn name(&self) -> &str {
        match self {
            Field::Original { name, .. } | Field::Written { name, .. } => name,
        }
    }

    fn matches(&self, name: &str) -> bool {
        self.name().eq_ignore_ascii_case(name)
    }
}

impl HeaderBlock {
    /// Parse a header block. Lossless: every byte lands in some field.
    fn parse(bytes: &[u8]) -> HeaderBlock {
        let mut fields: Vec<Field> = Vec::new();

        for line in lines(bytes) {
            let is_continuation = line.first().is_some_and(|c| *c == b' ' || *c == b'\t');

            if is_continuation || name_of(line).is_none() {
                // A continuation, or a line with no colon at all. Either way it
                // belongs to the field above it; a leading garbage line with
                // nothing above it becomes a nameless field, which no rule can
                // match and which therefore survives untouched.
                match fields.last_mut() {
                    Some(Field::Original { raw, .. }) => raw.extend_from_slice(line),
                    _ => fields.push(Field::Original {
                        name: String::new(),
                        raw: line.to_vec(),
                    }),
                }
                continue;
            }

            fields.push(Field::Original {
                name: name_of(line).expect("checked above").to_string(),
                raw: line.to_vec(),
            });
        }

        HeaderBlock { fields }
    }

    /// The logical (unfolded, whitespace-trimmed) value of the first instance.
    ///
    /// Raw, in the RFC 2047 sense — an encoded-word comes back encoded. §6.3's
    /// decoded variables come from `mail-parser` instead; this is for the
    /// arbitrary-header escape hatch and for the stability comparison.
    pub fn get(&self, name: &str) -> Option<String> {
        self.fields
            .iter()
            .find(|f| f.matches(name))
            .map(|f| match f {
                Field::Original { raw, .. } => unfold(raw),
                Field::Written { value, .. } => value.clone(),
            })
    }

    /// Every instance's logical value, in order. `Received:` is the header that
    /// makes this necessary.
    pub fn get_all(&self, name: &str) -> Vec<String> {
        self.fields
            .iter()
            .filter(|f| f.matches(name))
            .map(|f| match f {
                Field::Original { raw, .. } => unfold(raw),
                Field::Written { value, .. } => value.clone(),
            })
            .collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.fields.iter().any(|f| f.matches(name))
    }

    /// Field names in order, for tests and for the stability diff.
    pub fn names(&self) -> Vec<&str> {
        self.fields.iter().map(Field::name).collect()
    }

    /// §6.2 `remove_headers`, and §6.5's unconditional strip. Removes **every**
    /// instance — a message with three `DKIM-Signature` headers must leave with
    /// none.
    pub fn remove(&mut self, name: &str) -> usize {
        let before = self.fields.len();
        self.fields.retain(|f| !f.matches(name));
        before - self.fields.len()
    }

    /// §6.2: "Setting a header that already exists replaces all instances."
    ///
    /// The replacement lands **at the position of the first instance**, and the
    /// rest are dropped. Appending instead would reorder a message that already
    /// carries the target identity — which is precisely arrangement B of §1.1,
    /// the case where Simmer is supposed to be invisible.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        let written = Field::Written {
            name: name.to_string(),
            value,
        };

        match self.fields.iter().position(|f| f.matches(name)) {
            Some(first) => {
                self.fields[first] = written;
                let mut seen_first = false;
                self.fields.retain(|f| {
                    if !f.matches(name) {
                        return true;
                    }
                    if seen_first {
                        return false;
                    }
                    seen_first = true;
                    true
                });
            }
            None => self.fields.push(written),
        }
    }

    /// §6.1 step 8. Trace headers go at the top, per RFC 5321 §4.4: the topmost
    /// `Received:` is the most recent hop.
    pub fn prepend(&mut self, name: &str, value: impl Into<String>) {
        self.fields.insert(
            0,
            Field::Written {
                name: name.to_string(),
                value: value.into(),
            },
        );
    }

    /// Serialise, including the blank line that terminates the block.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(512);
        for field in &self.fields {
            match field {
                Field::Original { raw, .. } => out.extend_from_slice(raw),
                Field::Written { name, value } => {
                    out.extend_from_slice(name.as_bytes());
                    out.extend_from_slice(b": ");
                    out.extend_from_slice(encode::fold_if_needed(name, value).as_bytes());
                    out.extend_from_slice(b"\r\n");
                }
            }
        }
        out.extend_from_slice(b"\r\n");
        out
    }
}

/// Split into lines, each keeping its own terminator.
fn lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = bytes;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(rest.len());
        let (line, tail) = rest.split_at(end);
        rest = tail;
        Some(line)
    })
}

/// The field name of a header line, if it has one.
///
/// RFC 5322 `field-name` is printable ASCII excluding colon, so a line whose
/// text before the colon contains a space or a non-ASCII byte is not a header
/// field however much it looks like one.
fn name_of(line: &[u8]) -> Option<&str> {
    let colon = line.iter().position(|b| *b == b':')?;
    if colon == 0 {
        return None;
    }
    let name = &line[..colon];
    if !name.iter().all(|b| (33..=126).contains(b) && *b != b':') {
        return None;
    }
    std::str::from_utf8(name).ok()
}

/// The logical value of a raw field: colon-separated, unfolded, trimmed.
///
/// Unfolding removes the CRLF of a fold and keeps the whitespace that follows
/// it, per RFC 5322 §2.2.3. That is what makes [`encode::fold_if_needed`]
/// reversible, and reversibility is what §6.6's property needs.
fn unfold(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let after_colon = text.split_once(':').map(|(_, v)| v).unwrap_or("");
    after_colon
        .replace("\r\n", "")
        .replace('\n', "")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &[u8] = b"From: Jane <jane@old.com>\r\n\
                            To: bob@example.net\r\n\
                            Subject: Hello\r\n\
                            \r\n\
                            body line one\r\n";

    fn rendered(m: &Message<'_>) -> Vec<u8> {
        let mut out = m.headers.to_bytes();
        out.extend_from_slice(m.body);
        out
    }

    // -- splitting ---------------------------------------------------------

    #[test]
    fn a_message_splits_at_the_blank_line() {
        let m = split(SIMPLE);
        assert_eq!(m.headers.names(), ["From", "To", "Subject"]);
        assert_eq!(m.body, b"body line one\r\n");
    }

    #[test]
    fn an_untouched_message_round_trips_byte_for_byte() {
        assert_eq!(rendered(&split(SIMPLE)), SIMPLE);
    }

    #[test]
    fn quirks_in_untouched_headers_survive() {
        // Every one of these is something a normalising re-serialiser would
        // silently "fix", and every one of them is a byte §12.3 compares.
        let quirky: &[u8] = b"message-id: <a@b>\r\n\
                              Subject:  two  spaces\r\n\
                              X-Folded: first\r\n\tsecond\r\n\
                              X-No-Space:tight\r\n\
                              \r\n\
                              body\r\n";
        assert_eq!(rendered(&split(quirky)), quirky);
    }

    #[test]
    fn a_body_with_awkward_lines_is_opaque() {
        // The phase 2 baseline: a bare dot, a dot-prefixed line, a trailing
        // blank line. Phase 4 must not touch any of it.
        let awkward: &[u8] = b"From: a@b.com\r\n\r\n.\r\n..stuffed\r\n\r\n";
        let m = split(awkward);
        assert_eq!(m.body, b".\r\n..stuffed\r\n\r\n");
        assert_eq!(rendered(&m), awkward);
    }

    #[test]
    fn a_message_with_no_body_still_parses() {
        let m = split(b"From: a@b.com\r\n\r\n");
        assert_eq!(m.headers.names(), ["From"]);
        assert_eq!(m.body, b"");
        assert_eq!(rendered(&m), b"From: a@b.com\r\n\r\n");
    }

    #[test]
    fn a_message_with_no_separator_at_all_is_all_headers() {
        let m = split(b"From: a@b.com\r\n");
        assert_eq!(m.headers.names(), ["From"]);
        assert_eq!(m.body, b"");
    }

    #[test]
    fn bare_lf_separators_are_accepted() {
        // Not reachable from the relay, which normalises on ingest, but the
        // stability probe and the tests write messages by hand.
        let m = split(b"From: a@b.com\nSubject: x\n\nbody\n");
        assert_eq!(m.headers.names(), ["From", "Subject"]);
        assert_eq!(m.body, b"body\n");
    }

    #[test]
    fn an_empty_message_does_not_panic() {
        let m = split(b"");
        assert!(m.headers.names().is_empty());
        assert_eq!(m.body, b"");
    }

    #[test]
    fn a_message_that_is_only_the_separator_has_no_headers() {
        // The smallest message a client can send after DATA. If the leading CRLF
        // were parsed as a field it would end up above everything Simmer writes
        // and cut the header block off before it started.
        let m = split(b"\r\n");
        assert!(m.headers.names().is_empty());
        assert_eq!(m.body, b"");
        assert_eq!(rendered(&m), b"\r\n");
    }

    #[test]
    fn a_message_that_starts_with_a_blank_line_is_all_body() {
        let m = split(b"\r\nFrom: not-a-header@x\r\n");
        assert!(m.headers.names().is_empty());
        assert_eq!(m.body, b"From: not-a-header@x\r\n");
    }

    // -- reading -----------------------------------------------------------

    #[test]
    fn lookup_is_case_insensitive_and_unfolds() {
        let m = split(b"X-Folded: first\r\n second\r\n\r\n");
        assert_eq!(m.headers.get("x-folded").as_deref(), Some("first second"));
    }

    #[test]
    fn every_instance_is_available() {
        let m = split(b"Received: a\r\nReceived: b\r\n\r\n");
        assert_eq!(m.headers.get_all("received"), ["a", "b"]);
        assert_eq!(m.headers.get("received").as_deref(), Some("a"));
    }

    // -- removing ----------------------------------------------------------

    #[test]
    fn removing_takes_every_instance() {
        let mut m = split(b"DKIM-Signature: one\r\nFrom: a@b\r\nDKIM-Signature: two\r\n\r\n");
        assert_eq!(m.headers.remove("dkim-signature"), 2);
        assert_eq!(m.headers.names(), ["From"]);
    }

    #[test]
    fn removing_an_absent_header_is_a_no_op() {
        let mut m = split(SIMPLE);
        assert_eq!(m.headers.remove("X-Nope"), 0);
        assert_eq!(rendered(&m), SIMPLE);
    }

    #[test]
    fn removing_a_folded_header_takes_its_continuations_too() {
        let mut m = split(b"X-Long: first\r\n second\r\nFrom: a@b\r\n\r\nbody\r\n");
        m.headers.remove("X-Long");
        assert_eq!(rendered(&m), b"From: a@b\r\n\r\nbody\r\n");
    }

    // -- setting -----------------------------------------------------------

    #[test]
    fn setting_replaces_in_place_and_keeps_order() {
        // §1.1 arrangement B: the app already sends the target identity, so
        // From: is replaced where it stands and the message keeps its shape.
        let mut m = split(SIMPLE);
        m.headers.set("From", "Jane <sales@new.com>");
        assert_eq!(m.headers.names(), ["From", "To", "Subject"]);
        assert_eq!(
            rendered(&m),
            b"From: Jane <sales@new.com>\r\n\
              To: bob@example.net\r\n\
              Subject: Hello\r\n\
              \r\n\
              body line one\r\n"
        );
    }

    #[test]
    fn setting_replaces_all_instances_but_holds_the_first_position() {
        // §6.2: "Setting a header that already exists replaces all instances."
        let mut m = split(b"X-A: 1\r\nFrom: one@x\r\nX-B: 2\r\nFrom: two@x\r\n\r\n");
        m.headers.set("From", "new@x");
        assert_eq!(m.headers.names(), ["X-A", "From", "X-B"]);
        assert_eq!(m.headers.get("From").as_deref(), Some("new@x"));
    }

    #[test]
    fn setting_an_absent_header_appends_it() {
        let mut m = split(SIMPLE);
        m.headers.set("List-Unsubscribe", "<https://x/u>");
        assert_eq!(
            m.headers.names(),
            ["From", "To", "Subject", "List-Unsubscribe"]
        );
    }

    #[test]
    fn setting_is_case_insensitive_about_what_it_replaces() {
        let mut m = split(b"message-id: <old@x>\r\n\r\n");
        m.headers.set("Message-ID", "<new@y>");
        // The configured spelling wins for the header it writes.
        assert_eq!(m.headers.names(), ["Message-ID"]);
        assert_eq!(rendered(&m), b"Message-ID: <new@y>\r\n\r\n");
    }

    #[test]
    fn setting_the_same_value_twice_changes_nothing() {
        // §6.6 in miniature at the storage layer.
        let mut once = split(SIMPLE);
        once.headers.set("From", "Jane <sales@new.com>");
        let first = rendered(&once);

        let mut twice = split(&first);
        twice.headers.set("From", "Jane <sales@new.com>");
        assert_eq!(rendered(&twice), first);
    }

    // -- prepending --------------------------------------------------------

    #[test]
    fn prepending_puts_the_field_at_the_top() {
        // RFC 5321 §4.4: the most recent hop's Received: is first.
        let mut m = split(SIMPLE);
        m.headers.prepend("Received", "from x by y");
        assert_eq!(m.headers.names(), ["Received", "From", "To", "Subject"]);
        assert!(rendered(&m).starts_with(b"Received: from x by y\r\nFrom:"));
    }

    // -- serialising -------------------------------------------------------

    #[test]
    fn a_written_header_past_the_line_limit_is_folded() {
        let mut m = split(b"From: a@b\r\n\r\n");
        m.headers.set("X-Long", vec!["word"; 400].join(" "));
        let out = rendered(&m);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\r\n "));
        for line in text.split("\r\n") {
            assert!(line.len() <= 998, "line of {} octets", line.len());
        }
    }

    #[test]
    fn a_nameless_leading_line_is_preserved_rather_than_dropped() {
        // Malformed input, but throwing bytes away is worse than carrying them.
        let odd: &[u8] = b"not a header at all\r\nFrom: a@b\r\n\r\nbody\r\n";
        assert_eq!(rendered(&split(odd)), odd);
    }

    #[test]
    fn a_field_name_with_a_space_is_not_treated_as_a_header() {
        // "Subject line: value" is not a field; RFC 5322 field-name excludes SP.
        let m = split(b"From: a@b\r\nSubject line: value\r\n\r\n");
        assert_eq!(m.headers.names(), ["From"]);
    }
}
