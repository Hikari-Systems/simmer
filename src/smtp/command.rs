//! §5.2 command parsing: one wire line in, one [`Command`] out.
//!
//! Kept entirely free of I/O and of session state so the whole grammar is
//! testable as a pure function. The state machine in [`super::session`] decides
//! whether a well-formed command is *allowed* right now; this module only decides
//! whether it is well-formed at all.

use std::fmt;

/// A parsed command. Anything not in §5.2's list becomes [`Command::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Ehlo(String),
    Helo(String),
    /// `AUTH <mechanism> [initial-response]`.
    Auth {
        mechanism: String,
        initial: Option<String>,
    },
    Mail {
        /// `None` for the null sender, `MAIL FROM:<>` — legal, and used by
        /// bounces. Never confuse it with a parse failure.
        from: Option<String>,
        params: MailParams,
    },
    Rcpt {
        to: String,
    },
    Data,
    Rset,
    Noop,
    Quit,
    Vrfy,
    Expn,
    /// §5.2: "`BDAT` is `502 5.5.1 command not implemented`". Recognised
    /// explicitly rather than falling into `Unknown` so that the reply is `502`
    /// ("I know this command and refuse it") rather than `500` ("I have never
    /// heard of it"), which is what a client needs to fall back correctly.
    Bdat,
    Unknown(String),
}

/// §5.2 parameters Simmer understands on `MAIL FROM`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailParams {
    /// RFC 1870 `SIZE=`, checked against `max_message_bytes` before the body is
    /// transferred (§5.5).
    pub size: Option<u64>,
    /// RFC 6152 `BODY=8BITMIME`.
    pub body_8bitmime: bool,
    /// RFC 6531 `SMTPUTF8`. Only legal when `EHLO` advertised it (D-018).
    pub smtputf8: bool,
    /// RFC 4954 `AUTH=` — the authenticated identity a submitting client claims
    /// to be relaying for. Accepted and **ignored**: §5.3 says "the authenticated
    /// username plays no part in route selection", and passing an unverified
    /// assertion downstream would be worse than dropping it.
    pub auth_identity: Option<String>,
}

/// Why a syntactically invalid line was rejected. All map to `501`, but the
/// distinction is worth logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// e.g. `MAIL` with no `FROM:`, or a missing angle bracket.
    Syntax(&'static str),
    /// A recognised parameter with an unusable value, e.g. `SIZE=banana`.
    BadParameter(String),
    /// An unrecognised `MAIL FROM` parameter. RFC 1869 requires `555` rather than
    /// silent acceptance: silently dropping a parameter a client believes was
    /// honoured is how `BODY=8BITMIME` messages get mangled.
    UnknownParameter(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Syntax(s) => write!(f, "{s}"),
            ParseError::BadParameter(p) => write!(f, "unusable parameter: {p}"),
            ParseError::UnknownParameter(p) => write!(f, "unrecognised parameter: {p}"),
        }
    }
}

/// Parse one command line. The caller has already stripped CRLF.
pub fn parse(line: &str) -> Result<Command, ParseError> {
    let line = line.trim_start();
    let (verb, rest) = match line.find([' ', '\t']) {
        Some(i) => (&line[..i], line[i + 1..].trim_start()),
        None => (line, ""),
    };

    // Verbs are case-insensitive (RFC 5321 §2.4). Arguments are not, in general.
    let upper = verb.to_ascii_uppercase();

    match upper.as_str() {
        "EHLO" => {
            if rest.is_empty() {
                return Err(ParseError::Syntax("EHLO requires a domain"));
            }
            Ok(Command::Ehlo(rest.to_string()))
        }
        "HELO" => {
            if rest.is_empty() {
                return Err(ParseError::Syntax("HELO requires a domain"));
            }
            Ok(Command::Helo(rest.to_string()))
        }
        "AUTH" => parse_auth(rest),
        "MAIL" => parse_mail(rest),
        "RCPT" => parse_rcpt(rest),
        "DATA" => Ok(Command::Data),
        "RSET" => Ok(Command::Rset),
        "NOOP" => Ok(Command::Noop),
        "QUIT" => Ok(Command::Quit),
        "VRFY" => Ok(Command::Vrfy),
        "EXPN" => Ok(Command::Expn),
        "BDAT" => Ok(Command::Bdat),
        _ => Ok(Command::Unknown(upper)),
    }
}

fn parse_auth(rest: &str) -> Result<Command, ParseError> {
    if rest.is_empty() {
        return Err(ParseError::Syntax("AUTH requires a mechanism"));
    }
    let mut parts = rest.splitn(2, ' ');
    let mechanism = parts.next().unwrap_or_default().to_ascii_uppercase();
    let initial = parts.next().map(str::trim).filter(|s| !s.is_empty());
    Ok(Command::Auth {
        mechanism,
        initial: initial.map(str::to_string),
    })
}

fn parse_mail(rest: &str) -> Result<Command, ParseError> {
    // RFC 5321 spells it `MAIL FROM:<...>`; whitespace after the colon is not
    // strictly legal but is common enough in the wild that refusing it buys
    // nothing.
    let rest =
        strip_keyword(rest, "FROM").ok_or(ParseError::Syntax("MAIL must be followed by FROM:"))?;

    let (path, params) = split_path(rest).ok_or(ParseError::Syntax("MAIL FROM requires <path>"))?;

    // `<>` is the null sender: legal, and distinct from a parse failure.
    let from = if path.is_empty() {
        None
    } else {
        Some(strip_source_route(path).to_string())
    };

    Ok(Command::Mail {
        from,
        params: parse_mail_params(params)?,
    })
}

fn parse_rcpt(rest: &str) -> Result<Command, ParseError> {
    let rest =
        strip_keyword(rest, "TO").ok_or(ParseError::Syntax("RCPT must be followed by TO:"))?;

    let (path, _params) = split_path(rest).ok_or(ParseError::Syntax("RCPT TO requires <path>"))?;

    if path.is_empty() {
        // `RCPT TO:<>` is meaningless — there is nobody to deliver to. Unlike
        // `MAIL FROM:<>`, there is no legitimate reading of it.
        return Err(ParseError::Syntax("RCPT TO requires an address"));
    }

    // RFC 5321 §4.5.1 requires `postmaster` (no domain) to be accepted. Simmer
    // is a relay for its own applications, not a public MX, and §2.2 rules out
    // inbound mail handling — but the address still has to reach a downstream,
    // and a bare local part cannot be routed. Passed through as-is; the
    // downstream is the authority on whether it is deliverable.
    Ok(Command::Rcpt {
        to: strip_source_route(path).to_string(),
    })
}

/// Consume `KEYWORD` and its `:` from the front of `s`, case-insensitively.
fn strip_keyword<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let s = s.trim_start();
    if s.len() < keyword.len() || !s[..keyword.len()].eq_ignore_ascii_case(keyword) {
        return None;
    }
    let after = s[keyword.len()..].trim_start();
    after.strip_prefix(':').map(str::trim_start)
}

/// Split `<path> [params]` into the path's contents and the parameter tail.
///
/// The closing bracket is the first `>` **outside a quoted string**. Neither
/// simpler rule works: taking the first `>` unconditionally breaks
/// `<"a>b"@example.com>`, and taking the last one swallows the parameters in
/// `MAIL FROM:<a@b.com> AUTH=<jane@example.com>` — where the final `>` belongs to
/// a parameter value, not to the path.
fn split_path(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    let inner = s.strip_prefix('<')?;

    let bytes = inner.as_bytes();
    let mut in_quotes = false;
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            // RFC 5321 §4.1.2: inside a quoted string, `\` quotes the next
            // character — including a `"` that would otherwise end the string.
            b'\\' if in_quotes => i += 1,
            b'"' => in_quotes = !in_quotes,
            b'>' if !in_quotes => {
                return Some((&inner[..i], inner[i + 1..].trim_start()));
            }
            _ => {}
        }
        i += 1;
    }

    None
}

/// Drop an RFC 5321 §4.1.2 source route (`@a,@b:user@host` → `user@host`).
///
/// Deprecated since RFC 2821 and never generated by anything modern, but a relay
/// that passed one through would be handing a downstream a routing instruction
/// the client did not intend to be honoured.
fn strip_source_route(path: &str) -> &str {
    if !path.starts_with('@') {
        return path;
    }
    match path.find(':') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

fn parse_mail_params(params: &str) -> Result<MailParams, ParseError> {
    let mut out = MailParams::default();

    for token in params.split_whitespace() {
        let (key, value) = match token.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (token, None),
        };

        match key.to_ascii_uppercase().as_str() {
            "SIZE" => {
                let v = value.ok_or_else(|| ParseError::BadParameter(token.to_string()))?;
                out.size = Some(
                    v.parse::<u64>()
                        .map_err(|_| ParseError::BadParameter(token.to_string()))?,
                );
            }
            "BODY" => match value.map(str::to_ascii_uppercase).as_deref() {
                Some("8BITMIME") => out.body_8bitmime = true,
                Some("7BIT") => out.body_8bitmime = false,
                // BINARYMIME belongs to CHUNKING, which §2.2 puts out of scope.
                _ => return Err(ParseError::BadParameter(token.to_string())),
            },
            "SMTPUTF8" => out.smtputf8 = true,
            "AUTH" => out.auth_identity = value.map(str::to_string),
            _ => return Err(ParseError::UnknownParameter(key.to_string())),
        }
    }

    Ok(out)
}

/// Does this string contain non-ASCII, i.e. does it need `SMTPUTF8` (RFC 6531)?
///
/// Used at `MAIL FROM` and `RCPT TO` to answer O-10 before a body is transferred
/// rather than mid-relay. See `DECISIONS.md` D-018.
pub fn needs_smtputf8(address: &str) -> bool {
    !address.is_ascii()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(line: &str) -> Command {
        parse(line).expect("should parse")
    }

    // -- verbs -----------------------------------------------------------

    #[test]
    fn verbs_are_case_insensitive() {
        assert_eq!(ok("quit"), Command::Quit);
        assert_eq!(ok("QUIT"), Command::Quit);
        assert_eq!(ok("QuIt"), Command::Quit);
    }

    #[test]
    fn parses_the_simple_verbs() {
        assert_eq!(ok("DATA"), Command::Data);
        assert_eq!(ok("RSET"), Command::Rset);
        assert_eq!(ok("NOOP"), Command::Noop);
        assert_eq!(ok("VRFY someone"), Command::Vrfy);
        assert_eq!(ok("EXPN a-list"), Command::Expn);
    }

    #[test]
    fn bdat_is_recognised_so_it_can_be_refused_with_502_not_500() {
        // §5.2. A 500 tells a client the verb is unknown; a 502 tells it the verb
        // is known and unavailable, which is what drives the fallback to DATA.
        assert_eq!(ok("BDAT 1024"), Command::Bdat);
        assert_eq!(ok("BDAT 1024 LAST"), Command::Bdat);
    }

    #[test]
    fn unknown_verbs_are_reported_uppercased() {
        assert_eq!(ok("frobnicate x"), Command::Unknown("FROBNICATE".into()));
    }

    #[test]
    fn ehlo_and_helo_require_a_domain() {
        assert_eq!(
            ok("EHLO client.example"),
            Command::Ehlo("client.example".into())
        );
        assert_eq!(
            ok("HELO client.example"),
            Command::Helo("client.example".into())
        );
        assert!(parse("EHLO").is_err());
        assert!(parse("HELO   ").is_err());
    }

    // -- MAIL FROM -------------------------------------------------------

    #[test]
    fn parses_mail_from() {
        assert_eq!(
            ok("MAIL FROM:<jane@oldbrand.com>"),
            Command::Mail {
                from: Some("jane@oldbrand.com".into()),
                params: MailParams::default()
            }
        );
    }

    #[test]
    fn mail_from_keyword_is_case_insensitive_and_tolerates_spacing() {
        for line in [
            "MAIL FROM:<a@b.com>",
            "MAIL from:<a@b.com>",
            "MAIL FROM: <a@b.com>",
            "MAIL   FROM  :  <a@b.com>",
        ] {
            assert_eq!(
                ok(line),
                Command::Mail {
                    from: Some("a@b.com".into()),
                    params: MailParams::default()
                },
                "{line}"
            );
        }
    }

    #[test]
    fn the_null_sender_is_none_not_an_error() {
        // MAIL FROM:<> is legal and must not be confused with a parse failure —
        // it is what every bounce and every delivery-status notification uses.
        assert_eq!(
            ok("MAIL FROM:<>"),
            Command::Mail {
                from: None,
                params: MailParams::default()
            }
        );
    }

    #[test]
    fn mail_from_requires_angle_brackets() {
        assert!(parse("MAIL FROM:jane@oldbrand.com").is_err());
        assert!(parse("MAIL FROM:<unclosed").is_err());
        assert!(parse("MAIL").is_err());
        assert!(parse("MAIL TO:<a@b.com>").is_err());
    }

    #[test]
    fn a_quoted_local_part_may_contain_a_closing_bracket() {
        assert_eq!(
            ok(r#"MAIL FROM:<"a>b"@example.com>"#),
            Command::Mail {
                from: Some(r#""a>b"@example.com"#.into()),
                params: MailParams::default()
            }
        );
    }

    #[test]
    fn a_quoted_bracket_does_not_swallow_the_parameters() {
        // The two failure modes of a naive scan meet here: first-`>` truncates
        // the address, last-`>` eats SIZE. Only a quote-aware scan gets both.
        let Command::Mail { from, params } = ok(r#"MAIL FROM:<"a>b"@example.com> SIZE=99"#) else {
            panic!("not a MAIL");
        };
        assert_eq!(from.as_deref(), Some(r#""a>b"@example.com"#));
        assert_eq!(params.size, Some(99));
    }

    #[test]
    fn an_escaped_quote_inside_a_quoted_local_part_does_not_end_it() {
        let Command::Mail { from, .. } = ok(r#"MAIL FROM:<"a\">b"@example.com> SIZE=1"#) else {
            panic!("not a MAIL");
        };
        assert_eq!(from.as_deref(), Some(r#""a\">b"@example.com"#));
    }

    #[test]
    fn an_unclosed_quoted_string_is_a_syntax_error_not_a_truncated_address() {
        assert!(parse(r#"MAIL FROM:<"unterminated@example.com>"#).is_err());
    }

    #[test]
    fn a_source_route_is_stripped_rather_than_relayed() {
        assert_eq!(
            ok("MAIL FROM:<@relay1.example,@relay2.example:jane@oldbrand.com>"),
            Command::Mail {
                from: Some("jane@oldbrand.com".into()),
                params: MailParams::default()
            }
        );
        assert_eq!(
            ok("RCPT TO:<@relay.example:bob@gmail.com>"),
            Command::Rcpt {
                to: "bob@gmail.com".into()
            }
        );
    }

    // -- MAIL FROM parameters --------------------------------------------

    #[test]
    fn parses_size_body_and_smtputf8() {
        let Command::Mail { params, .. } =
            ok("MAIL FROM:<a@b.com> SIZE=4096 BODY=8BITMIME SMTPUTF8")
        else {
            panic!("not a MAIL");
        };
        assert_eq!(params.size, Some(4096));
        assert!(params.body_8bitmime);
        assert!(params.smtputf8);
    }

    #[test]
    fn parameter_keywords_are_case_insensitive() {
        let Command::Mail { params, .. } = ok("MAIL FROM:<a@b.com> size=10 body=8bitmime smtputf8")
        else {
            panic!("not a MAIL");
        };
        assert_eq!(params.size, Some(10));
        assert!(params.body_8bitmime);
        assert!(params.smtputf8);
    }

    #[test]
    fn body_7bit_is_accepted_and_means_not_8bit() {
        let Command::Mail { params, .. } = ok("MAIL FROM:<a@b.com> BODY=7BIT") else {
            panic!("not a MAIL");
        };
        assert!(!params.body_8bitmime);
    }

    #[test]
    fn the_auth_parameter_is_accepted_and_ignored() {
        // §5.3: "the authenticated username plays no part in route selection".
        // Accepting it keeps RFC 4954 clients happy; passing an unverified
        // assertion downstream would be worse than dropping it.
        let Command::Mail { params, .. } = ok("MAIL FROM:<a@b.com> AUTH=<jane@example.com>") else {
            panic!("not a MAIL");
        };
        assert_eq!(params.auth_identity.as_deref(), Some("<jane@example.com>"));
    }

    #[test]
    fn a_bad_size_is_a_parameter_error_not_a_silent_zero() {
        assert_eq!(
            parse("MAIL FROM:<a@b.com> SIZE=banana"),
            Err(ParseError::BadParameter("SIZE=banana".into()))
        );
        assert_eq!(
            parse("MAIL FROM:<a@b.com> SIZE"),
            Err(ParseError::BadParameter("SIZE".into()))
        );
    }

    #[test]
    fn an_unknown_parameter_is_refused_rather_than_dropped() {
        // Silently dropping a parameter the client believes was honoured is how
        // 8-bit messages get mangled and how DSN requests vanish.
        assert_eq!(
            parse("MAIL FROM:<a@b.com> RET=FULL"),
            Err(ParseError::UnknownParameter("RET".into()))
        );
        assert_eq!(
            parse("MAIL FROM:<a@b.com> BODY=BINARYMIME"),
            Err(ParseError::BadParameter("BODY=BINARYMIME".into()))
        );
    }

    // -- RCPT TO ---------------------------------------------------------

    #[test]
    fn parses_rcpt_to() {
        assert_eq!(
            ok("RCPT TO:<bob@gmail.com>"),
            Command::Rcpt {
                to: "bob@gmail.com".into()
            }
        );
    }

    #[test]
    fn rcpt_to_ignores_trailing_parameters() {
        // ORCPT/NOTIFY belong to the DSN extension, which §2.2 puts out of scope
        // and EHLO does not advertise. A client sending them anyway is not a
        // reason to refuse the recipient.
        assert_eq!(
            ok("RCPT TO:<bob@gmail.com> NOTIFY=SUCCESS"),
            Command::Rcpt {
                to: "bob@gmail.com".into()
            }
        );
    }

    #[test]
    fn rcpt_to_with_an_empty_path_is_an_error() {
        // Unlike MAIL FROM:<>, there is no legitimate reading of RCPT TO:<>.
        assert!(parse("RCPT TO:<>").is_err());
        assert!(parse("RCPT").is_err());
        assert!(parse("RCPT FROM:<a@b.com>").is_err());
    }

    // -- AUTH ------------------------------------------------------------

    #[test]
    fn parses_auth_with_and_without_an_initial_response() {
        assert_eq!(
            ok("AUTH LOGIN"),
            Command::Auth {
                mechanism: "LOGIN".into(),
                initial: None
            }
        );
        assert_eq!(
            ok("AUTH PLAIN AGNmYXBwAHB3"),
            Command::Auth {
                mechanism: "PLAIN".into(),
                initial: Some("AGNmYXBwAHB3".into())
            }
        );
        assert_eq!(
            ok("auth plain AGNmYXBwAHB3"),
            Command::Auth {
                mechanism: "PLAIN".into(),
                initial: Some("AGNmYXBwAHB3".into())
            }
        );
    }

    #[test]
    fn auth_requires_a_mechanism() {
        assert!(parse("AUTH").is_err());
    }

    // -- SMTPUTF8 detection (D-018) --------------------------------------

    #[test]
    fn detects_addresses_that_need_smtputf8() {
        assert!(!needs_smtputf8("jane@oldbrand.com"));
        assert!(needs_smtputf8("jane@öldbrand.com"));
        assert!(needs_smtputf8("珍@example.com"));
        // A-label (punycode) form needs no extension — it is pure ASCII.
        assert!(!needs_smtputf8("jane@xn--ldbrand-5za.com"));
    }
}
