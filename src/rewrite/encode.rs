//! RFC 5322 / RFC 2047 conformance for rendered header values.
//!
//! Three jobs, in the order they matter:
//!
//! 1. **Sanitising.** A rendered value can carry text from the message being
//!    rewritten — `{{original.subject}}` is in §6.3's table — so a bare `CR` or
//!    `LF` in it is header injection. It is replaced, never passed through.
//! 2. **Encoding.** §6.3: "non-ASCII in display names is RFC 2047-encoded
//!    automatically."
//! 3. **Folding.** Only when a line would otherwise break RFC 5322's 998-octet
//!    limit. The 78-octet *recommendation* is deliberately not honoured — see
//!    [`fold_if_needed`].
//!
//! Everything here is a pure function of its input, and every function is
//! idempotent on its own output. That is not incidental: §6.6's property
//! composes the whole rewrite with itself, and a value that re-encodes or
//! re-folds differently the second time would fail it for a reason that has
//! nothing to do with the operator's configuration.

use base64::Engine as _;

/// The longest an encoded-word may be, per RFC 2047 §2.
const MAX_ENCODED_WORD: usize = 75;
/// `=?UTF-8?B?` + `?=`.
const ENCODED_WORD_OVERHEAD: usize = 12;
/// Bytes of UTF-8 that base64 to a payload fitting `MAX_ENCODED_WORD`.
/// `4 * ceil(45 / 3) == 60`, and `60 + 12 == 72 <= 75`.
const MAX_CHUNK_BYTES: usize = 45;
/// RFC 5322 §2.1.1: "Each line of characters MUST be no more than 998
/// characters ... excluding the CRLF."
const MAX_LINE: usize = 998;

/// Remove anything that would end the header field early.
///
/// `CR` and `LF` become a single space each, and the pair `CRLF` becomes one
/// space rather than two so that a value which arrived folded reads the same
/// as one that did not. `NUL` goes the same way. Trailing whitespace is
/// trimmed, because it is invisible and would otherwise be the sort of
/// difference that fails a byte-equivalence assertion for no reason a human can
/// see.
pub fn sanitise(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(' ');
            }
            '\n' | '\0' => out.push(' '),
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// RFC 2047-encode a whole value, for headers whose grammar is unstructured
/// (`Subject:` and every `X-*`).
pub fn encode_unstructured(value: &str) -> String {
    let value = sanitise(value);
    if value.is_ascii() {
        return value;
    }
    encode_words(&value)
}

/// Make an address header's value conform to RFC 5322's `mailbox-list` grammar:
/// display names quoted where the grammar requires it, RFC 2047-encoded where
/// they are non-ASCII, addresses left exactly alone.
///
/// **Quoting is not cosmetic.** A display name containing a comma —
/// `Smith, Jane` — written bare turns one mailbox into two, and a second pass
/// over that output reads the display name as `Smith`. That is a §6.6 stability
/// violation manufactured by the encoder rather than by the operator's
/// configuration, and the property test in `tests/rewrite_stability.rs` is what
/// found it.
///
/// Encoding an `addr-spec` would corrupt it: `=?UTF-8?B?…?=` is not a mailbox.
/// So an element with no `<angle-addr>` is left exactly as it arrived even when
/// it is non-ASCII — that is an internationalised address, which belongs to
/// SMTPUTF8 (§5.2, D-018) and not to RFC 2047.
pub fn conform_address_list(value: &str) -> String {
    let value = sanitise(value);

    split_address_list(&value)
        .iter()
        .map(|element| conform_one_address(element))
        .collect::<Vec<_>>()
        .join(", ")
}

fn conform_one_address(element: &str) -> String {
    let element = element.trim();

    // `phrase <addr-spec>` — the phrase is ours to normalise, the angle-addr is
    // not.
    if let Some(open) = element.rfind('<') {
        let (phrase, addr) = element.split_at(open);
        let phrase = phrase.trim();
        if phrase.is_empty() {
            return addr.to_string();
        }
        // A quoted phrase's quotes belong to the grammar, not to the name, so
        // they come off before either treatment and go back on after.
        let bare = unquote_phrase(phrase);
        if bare.is_ascii() {
            return format!("{} {}", quote_phrase_if_needed(&bare), addr);
        }
        // An encoded-word is an atom: it needs no quoting, and quoting it would
        // be a grammar error.
        return format!("{} {}", encode_words(&bare), addr);
    }

    // A bare addr-spec. See the doc comment: left alone on purpose.
    element.to_string()
}

/// Strip surrounding quotes and undo `\` escaping, giving the name as written.
fn unquote_phrase(phrase: &str) -> String {
    let Some(inner) = phrase.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return phrase.to_string();
    };

    let mut out = String::with_capacity(inner.len());
    let mut escaped = false;
    for c in inner.chars() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// Quote a **substituted value** that lands in phrase position of an address
/// header.
///
/// This runs at substitution time, not on the finished string, and that is the
/// whole point. Once `{{original.from.display_name}}` has been pasted into
/// `{{…}} <sales@newbrand.com>`, a comma that came from the display name is
/// indistinguishable from a comma the operator wrote to separate two mailboxes.
/// Escaping at the boundary keeps the two apart: operator text stays a grammar,
/// message-derived text stays a value.
pub fn quote_phrase_component(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    quote_phrase_if_needed(value)
}

/// Escape a substituted value that lands *inside* quotes the operator wrote.
pub fn escape_quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Quote a display name if RFC 5322's `phrase` grammar requires it.
///
/// Idempotent by way of [`unquote_phrase`]: a name is unquoted before this runs,
/// so quoting the same name twice gives the same bytes.
fn quote_phrase_if_needed(phrase: &str) -> String {
    // RFC 5322 §3.2.3 `specials`, none of which are `atext`.
    const SPECIALS: [char; 13] = [
        '(', ')', '<', '>', '[', ']', ':', ';', '@', '\\', ',', '.', '"',
    ];

    if !phrase.chars().any(|c| SPECIALS.contains(&c)) {
        return phrase.to_string();
    }
    let escaped = phrase.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Split an `address-list` on commas that are outside quotes and outside
/// angle brackets. Both can legally contain a comma.
fn split_address_list(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut in_angles = false;
    let mut escaped = false;

    for c in value.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_quotes => {
                current.push(c);
                escaped = true;
            }
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            '<' if !in_quotes => {
                in_angles = true;
                current.push(c);
            }
            '>' if !in_quotes => {
                in_angles = false;
                current.push(c);
            }
            ',' if !in_quotes && !in_angles => {
                out.push(std::mem::take(&mut current));
            }
            c => current.push(c),
        }
    }
    out.push(current);
    out
}

/// Base64 encoded-words, chunked so each stays inside RFC 2047's 75-character
/// limit, joined by a single space.
///
/// B-encoding rather than Q-encoding throughout: Q is more readable for mostly
/// ASCII text, but its escaping rules differ by context (`phrase` forbids
/// characters that `text` allows), and one deterministic encoder is worth more
/// here than readable output. Adjacent encoded-words separated by whitespace are
/// concatenated by the receiver with the whitespace dropped (RFC 2047 §6.2),
/// so the split is invisible.
pub(crate) fn encode_words(value: &str) -> String {
    let mut words = Vec::new();
    let mut chunk = String::new();

    for c in value.chars() {
        if chunk.len() + c.len_utf8() > MAX_CHUNK_BYTES {
            words.push(encode_word(&chunk));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(encode_word(&chunk));
    }

    words.join(" ")
}

fn encode_word(chunk: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(chunk.as_bytes());
    debug_assert!(b64.len() + ENCODED_WORD_OVERHEAD <= MAX_ENCODED_WORD);
    format!("=?UTF-8?B?{b64}?=")
}

/// Fold a header line, but **only** when it would otherwise exceed RFC 5322's
/// 998-octet hard limit.
///
/// The 78-octet recommendation is not honoured, and that is a deliberate call.
/// Folding is the one transformation in this module that a downstream, a filter
/// or a spam scorer can observe *and* that has no single right answer — where to
/// break is a choice, and any choice we make becomes part of the bytes §12.3
/// compares. Folding only at the hard limit means the common case emits exactly
/// the bytes the operator's template describes, and the uncommon case stays
/// legal.
///
/// Returns the value with `CRLF SP` inserted at fold points. Unfolding replaces
/// `CRLF SP` with a single space, which is exactly the space that was there
/// before, so the round trip is lossless and §6.6's property holds.
pub fn fold_if_needed(name: &str, value: &str) -> String {
    // +2 for ": ".
    if name.len() + 2 + value.len() <= MAX_LINE {
        return value.to_string();
    }

    let mut out = String::with_capacity(value.len() + 16);
    let mut line_len = name.len() + 2;
    let mut first = true;

    for word in value.split(' ') {
        if first {
            out.push_str(word);
            line_len += word.len();
            first = false;
            continue;
        }
        if line_len + 1 + word.len() > MAX_LINE {
            out.push_str("\r\n ");
            line_len = 1;
        } else {
            out.push(' ');
            line_len += 1;
        }
        out.push_str(word);
        line_len += word.len();
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- sanitising -------------------------------------------------------

    #[test]
    fn header_injection_through_a_template_variable_is_neutralised() {
        // The attack: a message whose Subject: contains a newline, copied into a
        // set_headers value by {{original.subject}}.
        let injected = "Order 5\r\nBcc: attacker@evil.example";
        let out = sanitise(injected);
        assert!(!out.contains('\r') && !out.contains('\n'));
        assert_eq!(out, "Order 5 Bcc: attacker@evil.example");
    }

    #[test]
    fn a_lone_cr_or_lf_is_also_replaced() {
        assert_eq!(sanitise("a\rb"), "a b");
        assert_eq!(sanitise("a\nb"), "a b");
        assert_eq!(sanitise("a\0b"), "a b");
    }

    #[test]
    fn crlf_becomes_one_space_not_two() {
        // So a value that arrived folded reads the same as one that did not.
        assert_eq!(sanitise("a\r\nb"), "a b");
    }

    #[test]
    fn sanitising_is_idempotent() {
        let once = sanitise("  x\r\ny  ");
        assert_eq!(sanitise(&once), once);
    }

    // -- unstructured encoding --------------------------------------------

    #[test]
    fn ascii_is_left_alone() {
        assert_eq!(encode_unstructured("Your order"), "Your order");
    }

    #[test]
    fn non_ascii_becomes_a_base64_encoded_word() {
        assert_eq!(encode_unstructured("Grüße"), "=?UTF-8?B?R3LDvMOfZQ==?=");
    }

    #[test]
    fn a_long_value_is_split_into_several_encoded_words() {
        let long = "é".repeat(60); // 120 bytes
        let out = encode_unstructured(&long);
        assert!(out.split(' ').count() > 1, "{out}");
        for word in out.split(' ') {
            assert!(
                word.len() <= MAX_ENCODED_WORD,
                "{word} is {} long",
                word.len()
            );
            assert!(word.starts_with("=?UTF-8?B?") && word.ends_with("?="));
        }
    }

    #[test]
    fn chunking_never_splits_a_character() {
        // A 3-byte character straddling the 45-byte boundary would decode to a
        // replacement character on the receiver.
        let value = format!("{}€€€", "a".repeat(44));
        let out = encode_unstructured(&value);
        let decoded: String = out
            .split(' ')
            .map(|w| {
                let b64 = w.trim_start_matches("=?UTF-8?B?").trim_end_matches("?=");
                String::from_utf8(
                    base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .unwrap(),
                )
                .expect("each chunk is valid UTF-8 on its own")
            })
            .collect();
        assert_eq!(decoded, value);
    }

    #[test]
    fn encoding_is_idempotent_because_its_output_is_ascii() {
        let once = encode_unstructured("Grüße");
        assert_eq!(encode_unstructured(&once), once);
    }

    // -- address headers ---------------------------------------------------

    #[test]
    fn only_the_display_name_is_encoded() {
        let out = conform_address_list("Jäne Smith <sales@newbrand.com>");
        assert_eq!(out, "=?UTF-8?B?SsOkbmUgU21pdGg=?= <sales@newbrand.com>");
    }

    #[test]
    fn a_quoted_display_name_loses_its_quotes_to_the_encoded_word() {
        // An encoded-word is an atom; quoting it would be a grammar error.
        let out = conform_address_list("\"Jäne, Smith\" <sales@newbrand.com>");
        assert!(!out.contains('"'), "{out}");
        assert!(out.ends_with("<sales@newbrand.com>"));
    }

    #[test]
    fn an_ascii_display_name_beside_a_non_ascii_one_is_not_encoded() {
        let out = conform_address_list("Jäne <a@x.com>, Bob Smith <b@y.com>");
        assert!(out.contains("Bob Smith <b@y.com>"), "{out}");
        assert!(out.starts_with("=?UTF-8?B?"), "{out}");
    }

    #[test]
    fn a_comma_inside_a_quoted_phrase_does_not_split_the_list() {
        let out = conform_address_list("\"Smith, Jäne\" <a@x.com>");
        assert_eq!(out.matches('<').count(), 1, "{out}");
    }

    #[test]
    fn a_bare_internationalised_address_is_left_untouched() {
        // Encoding it would corrupt it; this is SMTPUTF8's territory (D-018).
        assert_eq!(conform_address_list("jäne@example.com"), "jäne@example.com");
    }

    #[test]
    fn address_encoding_is_idempotent() {
        let once = conform_address_list("Jäne <a@x.com>");
        assert_eq!(conform_address_list(&once), once);
    }

    // -- quoting: the bug the property test found ---------------------------

    #[test]
    fn a_bare_comma_in_a_finished_value_is_a_list_separator() {
        // At this layer the comma is the grammar's, not a value's — the operator
        // may legitimately have written two mailboxes. Keeping a display name's
        // comma from reaching here is `quote_phrase_component`'s job, applied at
        // substitution time by `Template::render_header`.
        assert_eq!(
            conform_address_list("Smith, Jane <a@x.com>"),
            "Smith, Jane <a@x.com>"
        );
    }

    #[test]
    fn a_substituted_value_with_a_comma_is_quoted_at_the_boundary() {
        assert_eq!(quote_phrase_component("Smith, Jane"), "\"Smith, Jane\"");
        assert_eq!(
            conform_address_list(&format!(
                "{} <a@x.com>",
                quote_phrase_component("Smith, Jane")
            )),
            "\"Smith, Jane\" <a@x.com>"
        );
    }

    #[test]
    fn quoting_survives_the_round_trip_a_second_pass_makes() {
        // Pass 2 reads the *decoded* display name back — quotes stripped — and
        // must put exactly the same quoting back.
        let once = conform_address_list(&format!(
            "{} <a@x.com>",
            quote_phrase_component("Smith, Jane")
        ));
        let decoded = unquote_phrase(once.split(" <").next().unwrap());
        assert_eq!(decoded, "Smith, Jane");
        assert_eq!(
            conform_address_list(&format!("{} <a@x.com>", quote_phrase_component(&decoded))),
            once
        );
    }

    #[test]
    fn an_empty_substitution_adds_no_quotes() {
        // `From: {{display_name}} <a@x>` on a message with no display name must
        // not become `From: "" <a@x>`.
        assert_eq!(quote_phrase_component(""), "");
    }

    #[test]
    fn a_display_name_needing_no_quotes_does_not_get_them() {
        // Quoting everything would be safe and would also change the bytes of
        // every message that did not need it.
        assert_eq!(
            conform_address_list("Jane Smith <a@x.com>"),
            "Jane Smith <a@x.com>"
        );
    }

    #[test]
    fn every_rfc_5322_special_triggers_quoting() {
        for special in [
            '(', ')', '<', '>', '[', ']', ':', ';', '@', ',', '.', '"', '\\',
        ] {
            let out = quote_phrase_component(&format!("Jane{special}Smith"));
            assert!(
                out.starts_with('"'),
                "'{special}' should force quoting, got {out}"
            );
        }
    }

    #[test]
    fn an_embedded_quote_is_escaped_and_survives_a_second_pass() {
        let once = quote_phrase_component("Jane \"JJ\" Smith");
        assert_eq!(once, "\"Jane \\\"JJ\\\" Smith\"");
        assert_eq!(unquote_phrase(&once), "Jane \"JJ\" Smith");
        assert_eq!(quote_phrase_component(&unquote_phrase(&once)), once);
    }

    #[test]
    fn an_already_quoted_name_is_not_quoted_twice() {
        assert_eq!(
            conform_address_list("\"Smith, Jane\" <a@x.com>"),
            "\"Smith, Jane\" <a@x.com>"
        );
    }

    #[test]
    fn a_bare_address_with_no_phrase_is_untouched() {
        assert_eq!(conform_address_list("<a@x.com>"), "<a@x.com>");
        assert_eq!(conform_address_list("a@x.com"), "a@x.com");
    }

    // -- folding ------------------------------------------------------------

    #[test]
    fn a_short_value_is_not_folded() {
        // The common case must emit exactly what the template rendered.
        let v = "Jane Smith <sales@newbrand.com>";
        assert_eq!(fold_if_needed("From", v), v);
    }

    #[test]
    fn a_value_past_998_octets_is_folded() {
        let v = vec!["word"; 400].join(" "); // ~2000 octets
        let folded = fold_if_needed("X-Long", &v);
        assert!(folded.contains("\r\n "), "should have folded");
        for line in folded.split("\r\n") {
            assert!(line.len() <= MAX_LINE);
        }
    }

    #[test]
    fn unfolding_a_folded_value_recovers_it_exactly() {
        // This is what makes §6.6's property survive folding: the fold point was
        // a space, and unfolding puts a space back.
        let v = vec!["word"; 400].join(" ");
        let folded = fold_if_needed("X-Long", &v);
        assert_eq!(folded.replace("\r\n ", " "), v);
    }
}
