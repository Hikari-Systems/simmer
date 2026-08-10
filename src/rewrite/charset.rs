//! Charset decoding and encoding for §6.4's `text/*` parts.
//!
//! §6.4: "decode the charset to UTF-8, apply each `body_rewrites` entry in order
//! as a regex replacement, re-encode".
//!
//! ## Why the supported set is four charsets and not all of them (D-044)
//!
//! §6.4 gives the answer for a charset this module does not know: "If a part
//! cannot be decoded (unknown charset, malformed encoding), leave it untouched,
//! log at `WARN`, and increment `simmer_body_rewrite_skipped_total`." That is a
//! specified, observable outcome rather than a gap, so the question is only
//! where to draw the line.
//!
//! It is drawn at the charsets an application generating its own mail actually
//! emits: UTF-8, US-ASCII, and the two single-byte Latin sets that legacy
//! systems still produce. Everything else is skipped and counted. The
//! alternative — `encoding_rs`, which `mail-parser` can pull in behind a feature
//! flag — decodes far more, but its *encoder* substitutes numeric character
//! references for characters a legacy charset cannot represent, which is an HTML
//! behaviour and produces `&#8212;` in the middle of an email body. Round-tripping
//! a part faithfully matters more here than covering Shift-JIS, and adding it
//! later is a change to this file alone.
//!
//! US-ASCII is deliberately handled as UTF-8. The two agree on every byte a
//! conforming us-ascii part can contain, and mail that labels UTF-8 content as
//! us-ascii is common enough that treating it as an unreadable part would skip
//! rewrites that are perfectly safe to make. A part whose label is us-ascii and
//! whose bytes are neither ASCII nor valid UTF-8 still fails to decode, which is
//! the honest answer: we do not know what it says.

/// A charset Simmer can both read and write exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    /// Also US-ASCII, and the RFC 2045 default when no `charset` parameter is
    /// present. See the module comment.
    Utf8,
    Latin1,
    Windows1252,
}

/// The part says something this module cannot read or write exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmappable;

impl Charset {
    /// Parse a `charset` parameter. `None` is §6.4's "unknown charset".
    ///
    /// An absent parameter is UTF-8 by the reasoning above: RFC 2045 §5.2 makes
    /// the default `us-ascii`, and this module reads us-ascii as UTF-8.
    pub fn parse(label: Option<&str>) -> Option<Charset> {
        let Some(label) = label else {
            return Some(Charset::Utf8);
        };
        // Some mailers quote the parameter and some pad it.
        let label = label.trim().trim_matches('"').trim();
        match label.to_ascii_lowercase().replace('_', "-").as_str() {
            "" | "utf-8" | "utf8" | "us-ascii" | "ascii" | "ansi-x3.4-1968" | "iso-ir-6" => {
                Some(Charset::Utf8)
            }
            "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1" | "l1" | "iso-ir-100" => {
                Some(Charset::Latin1)
            }
            "windows-1252" | "cp1252" | "cp-1252" | "x-cp1252" => Some(Charset::Windows1252),
            _ => None,
        }
    }

    pub fn decode(self, bytes: &[u8]) -> Result<String, Unmappable> {
        match self {
            Charset::Utf8 => String::from_utf8(bytes.to_vec()).map_err(|_| Unmappable),
            Charset::Latin1 => Ok(bytes.iter().map(|b| *b as char).collect()),
            Charset::Windows1252 => Ok(bytes.iter().map(|b| cp1252_to_char(*b)).collect()),
        }
    }

    /// Write `text` back in this charset.
    ///
    /// `Err` means the rewritten text contains a character the part's own
    /// charset cannot carry — which only happens when a `body_rewrites`
    /// replacement introduces one. Simmer does not change a part's charset any
    /// more than it changes its transfer encoding (D-045), so the part is left
    /// untouched and counted.
    pub fn encode(self, text: &str) -> Result<Vec<u8>, Unmappable> {
        match self {
            Charset::Utf8 => Ok(text.as_bytes().to_vec()),
            Charset::Latin1 => text
                .chars()
                .map(|c| u8::try_from(c as u32).map_err(|_| Unmappable))
                .collect(),
            Charset::Windows1252 => text.chars().map(char_to_cp1252).collect(),
        }
    }
}

/// The 27 positions where Windows-1252 differs from ISO-8859-1.
///
/// The five gaps (0x81, 0x8D, 0x8F, 0x90, 0x9D) are unassigned in the vendor
/// definition; they map to the matching C1 control, as WHATWG's Encoding
/// Standard specifies, so that every byte round-trips.
const CP1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

fn cp1252_to_char(b: u8) -> char {
    match b {
        0x80..=0x9F => CP1252_HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

fn char_to_cp1252(c: char) -> Result<u8, Unmappable> {
    if let Some(i) = CP1252_HIGH.iter().position(|h| *h == c) {
        return Ok(0x80 + i as u8);
    }
    match u32::from(c) {
        // The C1 range itself belongs to the table above, so a literal U+0080..
        // U+009F that is not one of the five gaps has no encoding here.
        n @ (0x00..=0x7F | 0xA0..=0xFF) => Ok(n as u8),
        _ => Err(Unmappable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- labels -------------------------------------------------------------

    #[test]
    fn an_absent_charset_parameter_is_read_as_utf8() {
        assert_eq!(Charset::parse(None), Some(Charset::Utf8));
    }

    #[test]
    fn us_ascii_is_read_as_utf8() {
        // See the module comment: the two agree on every byte a conforming
        // us-ascii part can hold, and mislabelled UTF-8 is common.
        assert_eq!(Charset::parse(Some("us-ascii")), Some(Charset::Utf8));
        assert_eq!(Charset::parse(Some("US-ASCII")), Some(Charset::Utf8));
    }

    #[test]
    fn labels_are_matched_past_the_ways_mailers_write_them() {
        assert_eq!(Charset::parse(Some(" \"UTF-8\" ")), Some(Charset::Utf8));
        assert_eq!(Charset::parse(Some("iso_8859-1")), Some(Charset::Latin1));
        assert_eq!(
            Charset::parse(Some("Windows-1252")),
            Some(Charset::Windows1252)
        );
    }

    #[test]
    fn a_charset_we_do_not_implement_is_none_rather_than_a_guess() {
        // §6.4: unknown charset means the part is left untouched.
        for label in ["shift_jis", "iso-2022-jp", "koi8-r", "utf-16", "gb2312"] {
            assert_eq!(Charset::parse(Some(label)), None, "{label}");
        }
    }

    // -- the codecs ---------------------------------------------------------

    #[test]
    fn utf8_that_is_not_utf8_does_not_decode() {
        // Lossy decoding would put U+FFFD in a body the client never wrote.
        assert_eq!(Charset::Utf8.decode(b"\xff\xfe"), Err(Unmappable));
    }

    #[test]
    fn latin1_maps_the_byte_to_the_code_point_of_the_same_number() {
        assert_eq!(Charset::Latin1.decode(b"caf\xe9").unwrap(), "café");
        assert_eq!(Charset::Latin1.encode("café").unwrap(), b"caf\xe9");
    }

    #[test]
    fn windows_1252_differs_from_latin1_exactly_where_it_should() {
        // 0x93/0x94 are the curly quotes that make this charset worth having.
        assert_eq!(Charset::Windows1252.decode(b"\x93hi\x94").unwrap(), "“hi”");
        assert_eq!(
            Charset::Latin1.decode(b"\x93hi\x94").unwrap(),
            "\u{93}hi\u{94}"
        );
    }

    #[test]
    fn every_single_byte_charset_round_trips_every_byte() {
        // What `encode` writes, `decode` reads back. A part whose pattern did
        // not match is never re-encoded at all, but one whose pattern matched
        // outside the escaped region must not shift anywhere else.
        for charset in [Charset::Latin1, Charset::Windows1252] {
            for b in 0..=255u8 {
                let decoded = charset.decode(&[b]).expect("single byte sets never fail");
                let encoded = charset.encode(&decoded).expect("its own output");
                assert_eq!(encoded, vec![b], "{charset:?} on byte {b:#04x}");
            }
        }
    }

    #[test]
    fn a_character_the_part_cannot_carry_is_refused_rather_than_mangled() {
        // The case D-045 turns on: a replacement string with a character the
        // part's charset has no room for. Substituting `?` or `&#8212;` would
        // put text in the body that neither the client nor the operator wrote.
        assert_eq!(Charset::Latin1.encode("an em dash —"), Err(Unmappable));
        assert_eq!(Charset::Windows1252.encode("日本語"), Err(Unmappable));
        // Windows-1252 *can* carry the em dash, which is the point of it.
        assert_eq!(Charset::Windows1252.encode("—").unwrap(), b"\x97");
    }
}
