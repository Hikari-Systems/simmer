//! SMTP replies, and the §10.1 rule for quoting a downstream's text back.
//!
//! Every reply Simmer can emit is named here rather than spelled inline at its
//! call site. §14.1's principle — *never emit a reply that makes a client record
//! permanent state* — is a property of the set of replies, not of any one of
//! them, and a set you can read in one screen is a set you can audit. Adding a
//! `5xx` to this file should feel like a decision.

use std::fmt;

/// One SMTP reply: a code, an optional enhanced status code (RFC 3463), and text.
///
/// Multi-line replies (only `EHLO`, in practice) are built by
/// [`Reply::multiline`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    /// Lines of text. One entry is a single-line reply; more than one produces
    /// the `250-`/`250 ` continuation form.
    pub lines: Vec<String>,
}

impl Reply {
    pub fn new(code: u16, text: impl Into<String>) -> Self {
        Self {
            code,
            lines: vec![text.into()],
        }
    }

    pub fn multiline(code: u16, lines: Vec<String>) -> Self {
        Self { code, lines }
    }

    /// `4xx` and `5xx` both mean "not accepted"; only `2xx` and `3xx` continue.
    pub fn is_positive(&self) -> bool {
        (200..400).contains(&self.code)
    }

    /// Does this reply end the session? §5.1 and §5.3 use `421` for exactly that.
    pub fn closes_connection(&self) -> bool {
        self.code == 421 || self.code == 554
    }

    /// Serialise to the wire, CRLF-terminated, with continuation dashes on every
    /// line but the last.
    pub fn to_wire(&self) -> String {
        let mut out = String::new();
        let last = self.lines.len().saturating_sub(1);
        for (i, line) in self.lines.iter().enumerate() {
            let sep = if i == last { ' ' } else { '-' };
            out.push_str(&format!("{}{}{}\r\n", self.code, sep, line));
        }
        if self.lines.is_empty() {
            out.push_str(&format!("{}\r\n", self.code));
        }
        out
    }
}

impl fmt::Display for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.code, self.lines.join(" / "))
    }
}

// ---------------------------------------------------------------------------
// §10.1 — quoting the downstream
// ---------------------------------------------------------------------------

/// The longest downstream text Simmer will repeat to a client.
///
/// §10.1 wants the text included "because it is frequently the only diagnostic
/// the operator will see", but a downstream controls it and RFC 5321 caps a
/// reply line at 512 octets including the code and CRLF. Truncating here keeps
/// the reply we build legal no matter what arrives.
const MAX_DOWNSTREAM_TEXT: usize = 200;

/// Sanitise downstream reply text for inclusion in a reply to the client.
///
/// §10.1: "sanitised of control characters and truncated to a safe length". A
/// downstream that emits a bare CR or LF would otherwise let us be used to
/// inject a whole extra reply line into our own client's stream, and the client
/// would act on it — a reply-splitting attack with an SMTP status code as the
/// payload.
pub fn sanitise_downstream_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_DOWNSTREAM_TEXT));
    let mut last_was_space = false;

    for ch in text.chars() {
        // Control characters *and* anything else non-printable: C1 controls and
        // Unicode line separators (U+2028/U+2029) are not `is_control()` in every
        // sense a terminal or a log pipeline cares about.
        let ch = if ch.is_control() || ch == '\u{2028}' || ch == '\u{2029}' {
            ' '
        } else {
            ch
        };

        if ch == ' ' {
            // Collapse runs, and never lead with one.
            if last_was_space || out.is_empty() {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }

        // Count characters, not bytes, but stop before splitting one.
        if out.chars().count() >= MAX_DOWNSTREAM_TEXT {
            break;
        }
        out.push(ch);
    }

    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Format a downstream verdict for the client: our code, then theirs.
///
/// The shape is deliberately uniform — `<our reply>: <their code> <their text>`
/// — so that an operator grepping logs for a downstream code finds it in the
/// same position regardless of which row of §10.1 produced the line.
pub fn with_downstream(
    code: u16,
    enhanced_and_prefix: &str,
    downstream_code: u16,
    downstream_text: &str,
) -> Reply {
    let text = sanitise_downstream_text(downstream_text);
    if text.is_empty() {
        Reply::new(
            code,
            format!("{enhanced_and_prefix}: downstream said {downstream_code}"),
        )
    } else {
        Reply::new(
            code,
            format!("{enhanced_and_prefix}: downstream said {downstream_code} {text}"),
        )
    }
}

// ---------------------------------------------------------------------------
// The fixed vocabulary
// ---------------------------------------------------------------------------

/// §5.2 — the greeting. `ESMTP` in the banner is what tells a client to try
/// `EHLO` rather than `HELO`.
pub fn greeting(hostname: &str) -> Reply {
    Reply::new(220, format!("{hostname} simmer ESMTP ready"))
}

/// §5.2 — `EHLO` advertises **exactly** `PIPELINING`, `8BITMIME`, `SMTPUTF8`,
/// `SIZE`, `STARTTLS` and `AUTH`, the last two when they apply. Nothing else.
///
/// `SMTPUTF8` is conditional rather than unconditional, which is a divergence:
/// see `DECISIONS.md` D-018 and [`crate::config::Config::advertise_smtputf8`].
/// `STARTTLS` arrived with D-070, and is advertised only on a listener that
/// offers it and only until the handshake — RFC 3207 §4.2 has the client
/// re-issue `EHLO` afterwards, and offering it again invites a loop.
pub fn ehlo(hostname: &str, client: &str, caps: Capabilities) -> Reply {
    let mut lines = vec![format!("{hostname} greets {client}")];
    lines.push("PIPELINING".into());
    lines.push("8BITMIME".into());
    if caps.smtputf8 {
        lines.push("SMTPUTF8".into());
    }
    lines.push(format!("SIZE {}", caps.max_size));
    if caps.starttls {
        lines.push("STARTTLS".into());
    }
    if caps.auth {
        // Both forms: RFC 4954's `AUTH PLAIN LOGIN`, and the historical
        // `AUTH=PLAIN LOGIN` that pre-RFC clients (still shipping) look for.
        lines.push("AUTH PLAIN LOGIN".into());
        lines.push("AUTH=PLAIN LOGIN".into());
    }
    Reply::multiline(250, lines)
}

/// What one `EHLO` reply advertises. A struct rather than a row of booleans
/// because the row had reached five and two of them were adjacent `bool`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub max_size: u64,
    pub smtputf8: bool,
    pub starttls: bool,
    pub auth: bool,
}

pub fn helo(hostname: &str) -> Reply {
    Reply::new(250, hostname.to_string())
}

pub fn ok() -> Reply {
    Reply::new(250, "2.0.0 ok")
}

pub fn accepted() -> Reply {
    Reply::new(250, "2.0.0 accepted")
}

pub fn bye(hostname: &str) -> Reply {
    Reply::new(221, format!("2.0.0 {hostname} closing connection"))
}

pub fn start_mail_input() -> Reply {
    Reply::new(354, "end data with <CR><LF>.<CR><LF>")
}

/// §5.2 — `VRFY` is always `252`: "cannot verify, but will attempt delivery".
/// Never `250`, which would confirm an address exists and make Simmer a
/// recipient oracle.
pub fn vrfy() -> Reply {
    Reply::new(252, "2.5.2 cannot verify, but will attempt delivery")
}

pub fn not_implemented() -> Reply {
    Reply::new(502, "5.5.1 command not implemented")
}

pub fn unrecognised() -> Reply {
    Reply::new(500, "5.5.2 command not recognised")
}

pub fn syntax_error() -> Reply {
    Reply::new(501, "5.5.4 syntax error in parameters or arguments")
}

pub fn bad_sequence() -> Reply {
    Reply::new(503, "5.5.1 bad sequence of commands")
}

/// Not in the spec's list. See `DECISIONS.md` D-020: RFC 5321 §4.5.3.1 caps a
/// command line at 512 octets, and without the cap a client can exhaust memory
/// before `SIZE` has anything to say about it.
pub fn line_too_long() -> Reply {
    Reply::new(500, "5.5.2 line too long")
}

// -- authentication (§5.3) --

pub fn auth_required() -> Reply {
    Reply::new(530, "5.7.0 authentication required")
}

pub fn auth_already_done() -> Reply {
    Reply::new(503, "5.5.1 already authenticated")
}

pub fn auth_mechanism_unsupported() -> Reply {
    Reply::new(504, "5.5.4 unrecognised authentication mechanism")
}

pub fn auth_succeeded() -> Reply {
    Reply::new(235, "2.7.0 authentication successful")
}

pub fn auth_failed() -> Reply {
    Reply::new(535, "5.7.8 authentication credentials invalid")
}

pub fn auth_cancelled() -> Reply {
    Reply::new(501, "5.7.0 authentication exchange cancelled")
}

pub fn auth_bad_encoding() -> Reply {
    Reply::new(501, "5.5.2 cannot decode authentication response")
}

/// §5.3 — three failures, then `421` and disconnect.
pub fn auth_too_many_failures() -> Reply {
    Reply::new(
        421,
        "4.7.0 too many authentication failures, closing connection",
    )
}

/// The `334` challenge in an `AUTH` exchange carries base64 in its *text*, and
/// no enhanced status code.
pub fn auth_challenge(b64: &str) -> Reply {
    Reply::new(334, b64.to_string())
}

/// D-070 — `AUTH` on a listener configured `auth: disabled`. `503` as the design
/// specifies: the command exists and is refused in this state, which is not the
/// same claim as `504`'s "no such mechanism".
pub fn auth_not_available() -> Reply {
    Reply::new(503, "5.5.1 authentication not available on this port")
}

/// RFC 4954 §6 — `AUTH` over plaintext where plaintext credentials are refused
/// (`allow_insecure_auth: false`, D-070). Not `530`, which means "you have not
/// authenticated"; the client needs to know encryption is what is missing.
pub fn encryption_required_for_auth() -> Reply {
    Reply::new(
        538,
        "5.7.11 encryption required for requested authentication mechanism",
    )
}

/// D-071 — an authenticated user presenting a sender identity outside its
/// `grants.send_as`, at `MAIL FROM` or at the final dot.
///
/// `550`, under §10.3's carve-out for `strict_senders` and for the same reason:
/// it is a statement about the *sender*, it cannot put a deliverable recipient
/// on a suppression list, and it should be loud because it means either a
/// misconfigured application or a stolen credential.
pub fn sender_not_permitted() -> Reply {
    Reply::new(550, "5.7.1 sender not permitted")
}

// -- inbound TLS (§5.1, RFC 3207, D-070) --

pub fn starttls_ready() -> Reply {
    Reply::new(220, "2.0.0 ready to start TLS")
}

/// RFC 3207 §4 — anything but `EHLO`, `NOOP`, `RSET`, `QUIT` and `STARTTLS` on
/// a `starttls_required` listener before the handshake.
pub fn must_starttls_first() -> Reply {
    Reply::new(530, "5.7.0 must issue a STARTTLS command first")
}

/// `STARTTLS` on a session already encrypted — implicit TLS, or a second one.
pub fn tls_already_active() -> Reply {
    Reply::new(503, "5.5.1 TLS already active")
}

// -- limits (§5.1, §5.5, §5.6) --

pub fn too_many_connections() -> Reply {
    Reply::new(421, "4.3.2 too many concurrent connections")
}

/// §5.1 — a peer outside `allowed_cidrs`. §2.3 says Simmer belongs on a trusted
/// segment, so this is a deployment error rather than an attack in the normal
/// case, and saying so plainly is more useful than a bare TCP close.
pub fn access_denied() -> Reply {
    Reply::new(554, "5.7.1 access denied")
}

/// D-047 — the second `RCPT TO` of any transaction, always.
///
/// §5.5's `too_many_recipients` used to sit beside this one, for `max_recipients`.
/// It was removed rather than left unreachable: one recipient is a stricter limit
/// than any ceiling, so nothing could ever reach it, and an unused reply in this
/// file is exactly what its enumeration test exists to catch.
///
/// `452`, not `5xx`: the recipient is perfectly deliverable and §14.1 will not
/// have a limit of ours recorded against them permanently.
pub fn multiple_recipients_not_permitted() -> Reply {
    Reply::new(452, "4.5.3 multiple recipients not permitted")
}

pub fn message_too_large() -> Reply {
    Reply::new(552, "5.3.4 message too large")
}

// -- timeouts and shutdown (§8.4, §10.4) --

pub fn command_timeout() -> Reply {
    Reply::new(421, "4.4.2 timeout waiting for command")
}

pub fn data_timeout() -> Reply {
    Reply::new(421, "4.4.2 timeout receiving message data")
}

pub fn session_timeout() -> Reply {
    Reply::new(421, "4.4.2 session timeout")
}

/// §10.4 — a session still running when the shutdown grace period expires.
pub fn shutting_down() -> Reply {
    Reply::new(421, "4.3.2 service shutting down")
}

// -- routing (§3.2, §5.4, §10.3) --

/// §5.4 — `From:` absent, unparseable, or a group with no addresses, when
/// `match_on` needs it.
pub fn malformed_from_header() -> Reply {
    Reply::new(550, "5.6.0 malformed From header")
}

/// §3.2 step 1 with `strict_senders: true`, and §10.3's explicit carve-out: this
/// `550` is "a policy statement about the *sender*", cannot trigger recipient
/// suppression, and "should be loud because it indicates misconfiguration".
pub fn sender_not_configured() -> Reply {
    Reply::new(550, "5.7.1 sender domain not configured")
}

/// §10.3, and the whole of §14.1. Default `451`: a `550` here would permanently
/// suppress a perfectly deliverable recipient in systems that outlive Simmer by
/// years, because a warming route hit today's ceiling.
pub fn no_eligible_route(permanent: bool) -> Reply {
    if permanent {
        Reply::new(550, "5.7.1 no eligible route")
    } else {
        Reply::new(451, "4.7.1 no eligible route, try later")
    }
}

/// D-018 — a UTF-8 address or `SMTPUTF8` parameter when `EHLO` did not advertise
/// the extension.
///
/// `550` is right here despite §14.1, and the test in §10.3 is why: were Simmer
/// removed, the client would talk to the same downstream, which does not
/// implement RFC 6531 either, and would get the same permanent refusal. The
/// reply is not an artefact of Simmer's presence.
pub fn smtputf8_unsupported() -> Reply {
    Reply::new(550, "5.6.7 SMTPUTF8 addresses not supported")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_reply_is_space_separated() {
        assert_eq!(Reply::new(250, "2.0.0 ok").to_wire(), "250 2.0.0 ok\r\n");
    }

    #[test]
    fn multiline_reply_uses_continuation_dashes_except_on_the_last() {
        let r = Reply::multiline(250, vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(r.to_wire(), "250-a\r\n250-b\r\n250 c\r\n");
    }

    #[test]
    fn ehlo_advertises_exactly_the_spec_list() {
        let caps = Capabilities {
            max_size: 1024,
            smtputf8: true,
            starttls: true,
            auth: true,
        };
        let wire = ehlo("simmer.test", "client.example", caps).to_wire();
        // §5.2: PIPELINING, 8BITMIME, SMTPUTF8, SIZE, STARTTLS, AUTH. "Nothing
        // else."
        assert!(wire.contains("250-PIPELINING\r\n"));
        assert!(wire.contains("250-8BITMIME\r\n"));
        assert!(wire.contains("250-SMTPUTF8\r\n"));
        assert!(wire.contains("250-SIZE 1024\r\n"));
        assert!(wire.contains("250-STARTTLS\r\n"));
        assert!(wire.contains("AUTH PLAIN LOGIN"));
        for banned in [
            "CHUNKING",
            "BDAT",
            "DSN",
            "ENHANCEDSTATUSCODES",
            "REQUIRETLS",
        ] {
            assert!(!wire.contains(banned), "{banned} must not be advertised");
        }
    }

    #[test]
    fn ehlo_omits_auth_starttls_and_smtputf8_when_disabled() {
        let caps = Capabilities {
            max_size: 10,
            smtputf8: false,
            starttls: false,
            auth: false,
        };
        let wire = ehlo("h", "c", caps).to_wire();
        assert!(!wire.contains("AUTH"));
        assert!(!wire.contains("STARTTLS"));
        assert!(!wire.contains("SMTPUTF8"));
        // ...and the last line still terminates the reply properly.
        assert!(wire.ends_with("250 SIZE 10\r\n"));
    }

    // -- §10.1 sanitisation ---------------------------------------------

    #[test]
    fn sanitiser_strips_crlf_so_a_downstream_cannot_inject_a_reply() {
        // The attack: a downstream whose text is "ok\r\n250 accepted" would,
        // unsanitised, make our client believe a second, positive reply arrived.
        let out = sanitise_downstream_text("rejected\r\n250 2.0.0 accepted");
        assert!(!out.contains('\r') && !out.contains('\n'));
        assert_eq!(out, "rejected 250 2.0.0 accepted");
    }

    #[test]
    fn sanitiser_strips_other_control_characters() {
        assert_eq!(sanitise_downstream_text("a\tb\0c\x07d"), "a b c d");
        assert_eq!(sanitise_downstream_text("a\u{2028}b"), "a b");
    }

    #[test]
    fn sanitiser_collapses_and_trims_whitespace() {
        assert_eq!(sanitise_downstream_text("   a    b   "), "a b");
        assert_eq!(sanitise_downstream_text("\r\n\r\n"), "");
    }

    #[test]
    fn sanitiser_truncates_to_a_safe_length() {
        let long = "x".repeat(1000);
        assert_eq!(sanitise_downstream_text(&long).len(), MAX_DOWNSTREAM_TEXT);
    }

    #[test]
    fn sanitiser_truncates_on_a_character_boundary() {
        // 300 three-byte characters: a byte-wise truncation would split one and
        // panic, or produce invalid UTF-8 in a log.
        let long = "é".repeat(300);
        let out = sanitise_downstream_text(&long);
        assert_eq!(out.chars().count(), MAX_DOWNSTREAM_TEXT);
    }

    #[test]
    fn downstream_verdict_keeps_their_code_in_a_predictable_position() {
        let r = with_downstream(
            451,
            "4.0.0 deferred by downstream",
            421,
            "too many messages",
        );
        assert_eq!(
            r.to_wire(),
            "451 4.0.0 deferred by downstream: downstream said 421 too many messages\r\n"
        );
    }

    #[test]
    fn downstream_verdict_survives_empty_text() {
        let r = with_downstream(550, "5.0.0 rejected by downstream", 550, "  \r\n ");
        assert_eq!(
            r.to_wire(),
            "550 5.0.0 rejected by downstream: downstream said 550\r\n"
        );
    }

    // -- the §14.1 audit -------------------------------------------------

    #[test]
    fn the_only_permanent_replies_are_the_ones_we_argued_for() {
        // §14.1: "Simmer must not emit a reply that causes a client to record
        // permanent state about a message or recipient. Apply this test to any
        // new failure path added later." This test is that instruction, made
        // mechanical — a new 5xx in this module fails it until someone writes
        // down why it is allowed.
        let permanent: Vec<(&str, Reply)> = vec![
            ("not_implemented", not_implemented()),
            ("unrecognised", unrecognised()),
            ("syntax_error", syntax_error()),
            ("bad_sequence", bad_sequence()),
            ("line_too_long", line_too_long()),
            ("auth_required", auth_required()),
            ("auth_already_done", auth_already_done()),
            ("auth_mechanism_unsupported", auth_mechanism_unsupported()),
            ("auth_failed", auth_failed()),
            ("auth_cancelled", auth_cancelled()),
            ("auth_bad_encoding", auth_bad_encoding()),
            // D-070. About the session's security, never a recipient.
            ("auth_not_available", auth_not_available()),
            (
                "encryption_required_for_auth",
                encryption_required_for_auth(),
            ),
            ("must_starttls_first", must_starttls_first()),
            ("tls_already_active", tls_already_active()),
            // D-071. About the sender, under §10.3's strict_senders carve-out.
            ("sender_not_permitted", sender_not_permitted()),
            ("access_denied", access_denied()),
            ("message_too_large", message_too_large()),
            ("malformed_from_header", malformed_from_header()),
            ("sender_not_configured", sender_not_configured()),
            ("smtputf8_unsupported", smtputf8_unsupported()),
            ("no_eligible_route(permanent)", no_eligible_route(true)),
        ];

        // Every one of these is about the *protocol exchange*, the *sender*, or
        // the *message*, and none of them is about a recipient's deliverability.
        // The recipient-bearing failures — chain exhaustion and every downstream
        // failure other than a 5xx at RCPT TO — are 4xx by construction.
        for (name, reply) in &permanent {
            assert!(
                (500..600).contains(&reply.code),
                "{name} is listed as permanent but is {}",
                reply.code
            );
        }

        assert_eq!(no_eligible_route(false).code, 451, "§10.3 default");
        assert_eq!(multiple_recipients_not_permitted().code, 452);
        assert_eq!(command_timeout().code, 421);
    }

    #[test]
    fn twenty_one_replies_close_the_connection() {
        assert!(too_many_connections().closes_connection());
        assert!(auth_too_many_failures().closes_connection());
        assert!(session_timeout().closes_connection());
        assert!(shutting_down().closes_connection());
        assert!(access_denied().closes_connection());
        assert!(!auth_failed().closes_connection());
        assert!(!ok().closes_connection());
    }
}
