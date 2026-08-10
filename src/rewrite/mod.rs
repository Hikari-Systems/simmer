//! `SPEC.md` §6 — the rewriting engine.
//!
//! This is the module the component exists for. Everything around it —
//! selection, quota, the ramp — decides *which* identity a message leaves under;
//! this decides what "leaving under an identity" actually means in bytes.
//!
//! ## The order is §6.1's order
//!
//! ```text
//! 2. Parse into headers and MIME structure
//! 4. Strip authentication artefacts (§6.5)
//! 5. Apply remove_headers
//! 6. Apply set_headers, rendering templates
//! 8. Prepend a Received: header naming Simmer
//! 9. Compute the outbound envelope sender
//! 10. Serialise and transmit
//! ```
//!
//! Steps 1 and 3 happened before we got here (`smtp::session` buffered, `relay`
//! selected). Step 7 — `body_rewrites` — is §6.4 and phase 5; the body is
//! carried through as an opaque slice until then.
//!
//! Two orderings inside that list are load-bearing rather than arbitrary:
//!
//! - **Every template reads the message as it arrived.** The context is built
//!   from the original header block before step 4 touches anything, so
//!   `set_headers` entries cannot see each other's output and the result does
//!   not depend on the order the operator happened to write them in. Without
//!   this, `Reply-To: {{original.from.address}}` would mean different things
//!   depending on whether it was listed above or below `From:`.
//! - **`remove_headers` before `set_headers`** is §6.2's own rule, and it is what
//!   makes "replace this header" expressible as naming it in both.
//!
//! ## What makes it stable (§6.6)
//!
//! Every assignment here is absolute: the value written is a function of the
//! *original* message and the route's configuration, never of the value being
//! overwritten. Composing that with itself is a no-op — unless a template reads
//! a field the same pass writes, which is exactly the accident §6.6's property
//! is designed to catch. See `stability.rs`.

pub mod encode;
pub mod headers;
pub mod stability;
pub mod template;

use std::collections::HashMap;

use crate::config::{Config, Identity};

pub use template::{AddressParts, Context, ParseError, Template, Var};

/// §6.5 — stripped unconditionally, on every route, with no configuration.
///
/// "Because rewriting `From:` or a body invalidates any inbound signature, and a
/// *failing* signature is treated more harshly by filters than an absent one."
/// The downstream signs; Simmer holds no key material.
pub const AUTH_ARTEFACTS: [&str; 5] = [
    "DKIM-Signature",
    "Authentication-Results",
    "ARC-Seal",
    "ARC-Message-Signature",
    "ARC-Authentication-Results",
];

// ---------------------------------------------------------------------------
// compiled form
// ---------------------------------------------------------------------------

/// A route's `identity` block with its templates parsed.
///
/// Compiled once at startup, for two reasons. The obvious one is that parsing
/// per message is waste. The one that matters is that a template parse failure
/// is a **configuration** error (D-034), and configuration errors belong at
/// startup where §4.2 can report all of them together — not on a connection that
/// has already been accepted.
#[derive(Debug, Clone)]
pub struct RouteRewrite {
    pub envelope_from: Template,
    pub set_headers: Vec<(String, Template)>,
    pub remove_headers: Vec<String>,
    pub unstable_headers: Vec<String>,
}

/// Where a template failed to parse, so §4.2 can name the YAML key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    /// Relative to the route's `identity`, e.g. `set_headers.From`.
    pub field: String,
    pub error: ParseError,
}

impl RouteRewrite {
    pub fn compile(identity: &Identity) -> Result<RouteRewrite, Vec<CompileError>> {
        let mut errors = Vec::new();

        let envelope_from = match Template::parse(&identity.envelope_from) {
            Ok(t) => Some(t),
            Err(error) => {
                errors.push(CompileError {
                    field: "envelope_from".to_string(),
                    error,
                });
                None
            }
        };

        let mut set_headers = Vec::with_capacity(identity.set_headers.0.len());
        for (name, source) in identity.set_headers.iter() {
            match Template::parse(source) {
                Ok(t) => set_headers.push((name.to_string(), t)),
                Err(error) => errors.push(CompileError {
                    field: format!("set_headers.{name}"),
                    error,
                }),
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(RouteRewrite {
            envelope_from: envelope_from.expect("no errors means it parsed"),
            set_headers,
            remove_headers: identity.remove_headers.clone(),
            unstable_headers: identity.unstable_headers.clone(),
        })
    }

    /// Whether `name` is declared migration-only for this route (§6.6).
    pub fn is_declared_unstable(&self, name: &str) -> bool {
        self.unstable_headers
            .iter()
            .any(|h| h.eq_ignore_ascii_case(name))
    }
}

/// Every route's compiled rewrite, by route name.
///
/// Built once at startup and held by the relay engine. A route missing from here
/// is impossible after validation, and the relay treats it as a `451` rather
/// than a panic — the message has already been accepted from the client.
#[derive(Debug, Clone, Default)]
pub struct Rewriters(HashMap<String, RouteRewrite>);

impl Rewriters {
    pub fn compile(cfg: &Config) -> Result<Rewriters, Vec<(String, CompileError)>> {
        let mut out = HashMap::with_capacity(cfg.routes.len());
        let mut errors = Vec::new();

        for route in &cfg.routes {
            match RouteRewrite::compile(&route.identity) {
                Ok(r) => {
                    out.insert(route.name.clone(), r);
                }
                Err(errs) => errors.extend(errs.into_iter().map(|e| (route.name.clone(), e))),
            }
        }

        if errors.is_empty() {
            Ok(Rewriters(out))
        } else {
            Err(errors)
        }
    }

    pub fn get(&self, route: &str) -> Option<&RouteRewrite> {
        self.0.get(route)
    }
}

// ---------------------------------------------------------------------------
// inputs and outputs
// ---------------------------------------------------------------------------

/// What arrived, plus what the route decision produced.
pub struct Inbound<'a> {
    pub raw: &'a [u8],
    /// `None` is the null sender, `<>`.
    pub envelope_from: Option<&'a str>,
    pub recipients: &'a [String],
    pub route_name: &'a str,
    pub correlation_id: &'a str,
    pub received: Received<'a>,
    /// Injected so the §6.6 probe and the property test can render twice at one
    /// instant. Production passes `Utc::now()`.
    pub now: chrono::DateTime<chrono::Utc>,
    /// Injected for the same reason. Production passes a fresh v4 per render.
    pub uuid: &'a dyn Fn() -> String,
}

/// §6.1 step 8's raw material.
pub struct Received<'a> {
    /// What the client said in `EHLO`/`HELO`.
    pub helo: &'a str,
    /// The client's address, as seen by the listener.
    pub peer: &'a str,
    /// `server.hostname`.
    pub by: &'a str,
    /// Whether the client authenticated — the difference between RFC 3848's
    /// `ESMTP` and `ESMTPA`.
    pub authenticated: bool,
}

/// What to send.
pub struct Rewritten {
    pub raw: Vec<u8>,
    /// `None` is the null sender. Ready for `MAIL FROM:<…>` — no angle brackets.
    pub envelope_from: Option<String>,
}

// ---------------------------------------------------------------------------
// the engine
// ---------------------------------------------------------------------------

/// Apply a route's identity to a message. §6.1 steps 2 and 4–10.
///
/// Infallible. Everything that can fail — an unparseable template, an identity
/// field that is not stable — has already failed at startup, and a message that
/// has been accepted from a client must not be dropped because a header it
/// happens not to carry made a variable render empty.
pub fn rewrite(route: &RouteRewrite, inbound: &Inbound<'_>) -> Rewritten {
    // -- step 2 --------------------------------------------------------
    let mut message = headers::split(inbound.raw);

    // The context is built from the message **as it arrived**, before any of the
    // steps below run. See the module comment: this is what makes the result
    // independent of the order the operator listed `set_headers` in.
    let original = message.headers.clone();
    let parsed = mail_parser::MessageParser::default().parse_headers(inbound.raw);

    let from_address = parsed
        .as_ref()
        .and_then(|m| m.from())
        .and_then(|list| list.first())
        .and_then(|addr| addr.address())
        .unwrap_or_default()
        .to_string();
    let from_display_name = parsed
        .as_ref()
        .and_then(|m| m.from())
        .and_then(|list| list.first())
        .and_then(|addr| addr.name())
        .unwrap_or_default()
        .to_string();
    let subject = parsed
        .as_ref()
        .and_then(|m| m.subject())
        .unwrap_or_default()
        .to_string();
    let message_id = original.get("Message-ID").unwrap_or_default();

    let header_lookup = move |name: &str| original.get(name);
    let ctx = Context {
        from: AddressParts::split(&from_address),
        from_display_name,
        envelope_from: AddressParts::split(inbound.envelope_from.unwrap_or_default()),
        message_id,
        subject,
        header: &header_lookup,
        // §6.3 scopes recipient.* to the single-recipient case. Multi-recipient
        // splitting is §5.6 and phase 9 (O-9); until then a message with more
        // than one recipient renders these empty rather than picking one
        // arbitrarily, which would be worse than admitting we do not know.
        recipient: match inbound.recipients {
            [only] => Some(AddressParts::split(only)),
            _ => None,
        },
        route_name: inbound.route_name,
        correlation_id: inbound.correlation_id,
        now: inbound.now,
        uuid: inbound.uuid,
    };

    // -- step 4: §6.5, unconditional -----------------------------------
    for artefact in AUTH_ARTEFACTS {
        message.headers.remove(artefact);
    }

    // -- step 5 --------------------------------------------------------
    for name in &route.remove_headers {
        message.headers.remove(name);
    }

    // -- step 6 --------------------------------------------------------
    for (name, tmpl) in &route.set_headers {
        message.headers.set(name, tmpl.render_header(name, &ctx));
    }

    // -- step 8 --------------------------------------------------------
    message
        .headers
        .prepend("Received", received_value(&inbound.received, inbound));

    // -- step 9 --------------------------------------------------------
    let envelope_from = outbound_envelope_sender(route, &ctx, inbound.envelope_from);

    // -- step 10 -------------------------------------------------------
    let mut raw = message.headers.to_bytes();
    raw.extend_from_slice(message.body);

    Rewritten { raw, envelope_from }
}

/// §6.1 step 9.
///
/// **The null sender is never rewritten** (D-035). `MAIL FROM:<>` identifies a
/// bounce or other notification, and RFC 5321 §6.1 requires exactly that for
/// them; assigning it a real address would make every bounce bounceable and turn
/// a delivery loop into a live possibility. It is also the §1.1-correct answer:
/// an application sending its own notifications directly would use `<>` too, so
/// passing it through is what "exactly expressible as application-side
/// configuration" means here.
fn outbound_envelope_sender(
    route: &RouteRewrite,
    ctx: &Context<'_>,
    incoming: Option<&str>,
) -> Option<String> {
    incoming?;

    let rendered = sanitise_address(&route.envelope_from.render(ctx));
    if rendered.is_empty() {
        // A template that renders to nothing — `{{original.from.local}}@x` on a
        // message with no `From:`, say. Falling back to what arrived keeps the
        // message deliverable; the alternative is a null sender, which would
        // mark ordinary mail as a bounce.
        tracing::warn!(
            template = %route.envelope_from,
            "envelope_from rendered empty; keeping the incoming envelope sender"
        );
        return incoming.map(str::to_string);
    }
    Some(rendered)
}

/// A `MAIL FROM` argument, ready to go inside `<…>`.
///
/// Whitespace and line endings are removed rather than escaped: the value is
/// interpolated straight into an SMTP command, so a space would truncate it and
/// a CRLF would let a rendered template inject a command.
fn sanitise_address(value: &str) -> String {
    let trimmed = value.trim();
    let bare = trimmed
        .strip_prefix('<')
        .and_then(|v| v.strip_suffix('>'))
        .unwrap_or(trimmed);
    bare.chars().filter(|c| !c.is_whitespace()).collect()
}

/// §6.1 step 8 — a `Received:` line naming Simmer, in RFC 5321 §4.4 form.
///
/// Excluded from §12.3's byte-equivalence comparison by D-002, along with the
/// `X-Simmer-*` headers — which, per the phase 4 decision, Simmer does not emit
/// at all. One added header is the smallest honest cost of being in the path.
fn received_value(r: &Received<'_>, inbound: &Inbound<'_>) -> String {
    // RFC 3848: `ESMTPA` when the session authenticated, `ESMTP` when not.
    let with = if r.authenticated { "ESMTPA" } else { "ESMTP" };
    format!(
        "from {} ({}) by {} with {} id {}; {}",
        encode::sanitise(r.helo),
        encode::sanitise(r.peer),
        encode::sanitise(r.by),
        with,
        encode::sanitise(inbound.correlation_id),
        inbound.now.to_rfc2822(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn at(ts: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(ts)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn compile(yaml: &str) -> RouteRewrite {
        let identity: Identity = serde_yaml_ng::from_str(yaml).expect("fixture parses");
        RouteRewrite::compile(&identity).expect("fixture compiles")
    }

    fn inbound<'a>(
        raw: &'a [u8],
        envelope_from: Option<&'a str>,
        recipients: &'a [String],
        uuid: &'a dyn Fn() -> String,
    ) -> Inbound<'a> {
        Inbound {
            raw,
            envelope_from,
            recipients,
            route_name: "warming",
            correlation_id: "cid-1",
            received: Received {
                helo: "app.internal",
                peer: "10.1.2.3",
                by: "simmer.test",
                authenticated: true,
            },
            now: at("2026-08-10T12:30:00Z"),
            uuid,
        }
    }

    fn run(route: &RouteRewrite, raw: &[u8], envelope: Option<&str>) -> Rewritten {
        let rcpt = ["bob@example.net".to_string()];
        let uuid = || "11111111-2222-3333-4444-555555555555".to_string();
        rewrite(route, &inbound(raw, envelope, &rcpt, &uuid))
    }

    fn text(r: &Rewritten) -> String {
        String::from_utf8(r.raw.clone()).unwrap()
    }

    const MESSAGE: &[u8] = b"From: Jane Smith <jane@oldbrand.com>\r\n\
                             To: bob@example.net\r\n\
                             Subject: Your order\r\n\
                             Message-ID: <abc@oldbrand.com>\r\n\
                             \r\n\
                             Hello.\r\n";

    // -- the shape of the whole thing ------------------------------------

    #[test]
    fn the_worked_example_from_6_6_produces_what_the_spec_says() {
        // SPEC.md §6.6's own table, before app cutover.
        let route = compile(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  From: "{{original.from.display_name}} <sales@newbrand.com>"
  Reply-To: "{{original.from.address}}"
unstable_headers: ["Reply-To"]
"#,
        );
        let out = run(&route, MESSAGE, Some("jane@oldbrand.com"));
        let text = text(&out);

        assert!(
            text.contains("From: Jane Smith <sales@newbrand.com>\r\n"),
            "{text}"
        );
        assert!(text.contains("Reply-To: jane@oldbrand.com\r\n"), "{text}");
        assert_eq!(out.envelope_from.as_deref(), Some("bounce@newbrand.com"));
    }

    #[test]
    fn after_app_cutover_the_same_route_changes_nothing_material() {
        // §6.6's table, second row: arrangement B. This is the cutover invariant
        // stated as a unit test.
        let route = compile(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  From: "{{original.from.display_name}} <sales@newbrand.com>"
"#,
        );
        let already: &[u8] = b"From: Jane Smith <sales@newbrand.com>\r\n\
                               To: bob@example.net\r\n\
                               \r\n\
                               Hello.\r\n";
        let out = run(&route, already, Some("bounce@newbrand.com"));
        let text = text(&out);
        assert!(
            text.contains("From: Jane Smith <sales@newbrand.com>\r\n"),
            "{text}"
        );
    }

    #[test]
    fn the_body_is_never_touched() {
        let route = compile(r#"envelope_from: "b@new.com""#);
        // The phase 2 baseline body: a bare dot, a dot-prefixed line, a trailing
        // blank line.
        let raw: &[u8] = b"From: a@old.com\r\n\r\n.\r\n..stuffed\r\nline\r\n\r\n";
        let out = run(&route, raw, Some("a@old.com"));
        assert!(out.raw.ends_with(b"\r\n\r\n.\r\n..stuffed\r\nline\r\n\r\n"));
    }

    #[test]
    fn headers_the_route_does_not_name_are_left_alone() {
        let route = compile(
            r#"
envelope_from: "b@new.com"
set_headers:
  From: "<sales@new.com>"
"#,
        );
        let out = text(&run(&route, MESSAGE, Some("jane@oldbrand.com")));
        assert!(out.contains("To: bob@example.net\r\n"));
        assert!(out.contains("Subject: Your order\r\n"));
        assert!(out.contains("Message-ID: <abc@oldbrand.com>\r\n"));
    }

    // -- §6.5 -------------------------------------------------------------

    #[test]
    fn authentication_artefacts_are_stripped_unconditionally() {
        // No configuration involved: §6.5 says every route, always.
        let route = compile(r#"envelope_from: "b@new.com""#);
        let signed: &[u8] = b"DKIM-Signature: v=1; a=rsa-sha256; d=oldbrand.com;\r\n\
                              Authentication-Results: mx.google.com; spf=pass\r\n\
                              ARC-Seal: i=1; cv=none\r\n\
                              ARC-Message-Signature: i=1\r\n\
                              ARC-Authentication-Results: i=1\r\n\
                              From: a@oldbrand.com\r\n\
                              \r\n\
                              body\r\n";
        let out = text(&run(&route, signed, Some("a@oldbrand.com")));
        for artefact in AUTH_ARTEFACTS {
            assert!(!out.contains(artefact), "{artefact} survived:\n{out}");
        }
        assert!(out.contains("From: a@oldbrand.com"));
    }

    #[test]
    fn every_instance_of_an_artefact_is_stripped() {
        // Mail that has crossed two signing hops carries two signatures.
        let route = compile(r#"envelope_from: "b@new.com""#);
        let raw: &[u8] = b"DKIM-Signature: one\r\nDKIM-Signature: two\r\nFrom: a@b\r\n\r\n";
        let out = text(&run(&route, raw, Some("a@b")));
        assert!(!out.contains("DKIM-Signature"), "{out}");
    }

    // -- §6.2 ordering ----------------------------------------------------

    #[test]
    fn remove_runs_before_set_so_a_header_can_be_replaced_by_naming_both() {
        let route = compile(
            r#"
envelope_from: "b@new.com"
remove_headers: ["Return-Path", "X-Mailer"]
set_headers:
  X-Mailer: "simmer"
"#,
        );
        let raw: &[u8] =
            b"Return-Path: <a@old.com>\r\nX-Mailer: OldApp 1.0\r\nFrom: a@old.com\r\n\r\n";
        let out = text(&run(&route, raw, Some("a@old.com")));
        assert!(!out.contains("Return-Path"), "{out}");
        assert_eq!(out.matches("X-Mailer").count(), 1, "{out}");
        assert!(out.contains("X-Mailer: simmer\r\n"), "{out}");
    }

    #[test]
    fn set_headers_all_read_the_message_as_it_arrived() {
        // Order-independence: Reply-To reads the *original* From:, not the one
        // the same pass just wrote, regardless of which was listed first.
        let route = compile(
            r#"
envelope_from: "b@new.com"
set_headers:
  From: "<sales@newbrand.com>"
  Reply-To: "{{original.from.address}}"
unstable_headers: ["Reply-To"]
"#,
        );
        let out = text(&run(&route, MESSAGE, Some("jane@oldbrand.com")));
        assert!(out.contains("Reply-To: jane@oldbrand.com\r\n"), "{out}");

        let reversed = compile(
            r#"
envelope_from: "b@new.com"
set_headers:
  Reply-To: "{{original.from.address}}"
  From: "<sales@newbrand.com>"
unstable_headers: ["Reply-To"]
"#,
        );
        let out2 = text(&run(&reversed, MESSAGE, Some("jane@oldbrand.com")));
        assert!(out2.contains("Reply-To: jane@oldbrand.com\r\n"), "{out2}");
    }

    // -- §6.1 step 8 -------------------------------------------------------

    #[test]
    fn a_received_header_is_prepended() {
        let route = compile(r#"envelope_from: "b@new.com""#);
        let out = text(&run(&route, MESSAGE, Some("a@old.com")));
        assert!(
            out.starts_with(
                "Received: from app.internal (10.1.2.3) by simmer.test with ESMTPA id cid-1;"
            ),
            "{out}"
        );
    }

    #[test]
    fn an_unauthenticated_session_is_recorded_as_esmtp_not_esmtpa() {
        let route = compile(r#"envelope_from: "b@new.com""#);
        let rcpt = ["bob@example.net".to_string()];
        let uuid = || String::new();
        let mut inb = inbound(MESSAGE, Some("a@old.com"), &rcpt, &uuid);
        inb.received.authenticated = false;
        let out = String::from_utf8(rewrite(&route, &inb).raw).unwrap();
        assert!(
            out.starts_with("Received: from app.internal (10.1.2.3) by simmer.test with ESMTP id"),
            "{out}"
        );
    }

    #[test]
    fn the_existing_received_chain_is_kept_below_ours() {
        let route = compile(r#"envelope_from: "b@new.com""#);
        let raw: &[u8] = b"Received: from upstream by app\r\nFrom: a@old.com\r\n\r\n";
        let out = text(&run(&route, raw, Some("a@old.com")));
        assert!(out.contains("Received: from upstream by app\r\n"), "{out}");
        assert!(out.starts_with("Received: from app.internal"), "{out}");
    }

    #[test]
    fn no_x_simmer_headers_are_emitted() {
        // The phase 4 call: Received: only. Every header Simmer adds is a header
        // the recipient sees that would not exist once Simmer is unplugged.
        let route = compile(r#"envelope_from: "b@new.com""#);
        let out = text(&run(&route, MESSAGE, Some("a@old.com")));
        assert!(!out.contains("X-Simmer"), "{out}");
    }

    #[test]
    fn a_helo_containing_a_newline_cannot_break_the_header_block() {
        let route = compile(r#"envelope_from: "b@new.com""#);
        let rcpt = ["bob@example.net".to_string()];
        let uuid = || String::new();
        let mut inb = inbound(MESSAGE, Some("a@old.com"), &rcpt, &uuid);
        inb.received.helo = "evil\r\nBcc: attacker@evil.example";
        let out = String::from_utf8(rewrite(&route, &inb).raw).unwrap();
        // The text survives inside the Received: value, which is harmless; what
        // must not happen is it becoming a field of its own.
        assert!(
            !out.split("\r\n").any(|line| line.starts_with("Bcc:")),
            "{out}"
        );
        assert_eq!(out.matches("Received:").count(), 1, "{out}");
    }

    // -- §6.1 step 9 -------------------------------------------------------

    #[test]
    fn the_envelope_sender_is_rendered_from_the_template() {
        let route = compile(r#"envelope_from: "{{original.from.local}}@newbrand.com""#);
        let out = run(&route, MESSAGE, Some("jane@oldbrand.com"));
        assert_eq!(out.envelope_from.as_deref(), Some("jane@newbrand.com"));
    }

    #[test]
    fn a_null_sender_is_never_rewritten() {
        // D-035. MAIL FROM:<> is a bounce; giving it a real return path makes it
        // bounceable and RFC 5321 §6.1 forbids it.
        let route = compile(r#"envelope_from: "bounce@newbrand.com""#);
        let out = run(&route, MESSAGE, None);
        assert_eq!(out.envelope_from, None);
    }

    #[test]
    fn angle_brackets_in_the_template_are_stripped() {
        // The command is written as MAIL FROM:<{value}>, so a configured
        // "<a@b>" would otherwise produce "<<a@b>>".
        let route = compile(r#"envelope_from: "<bounce@newbrand.com>""#);
        let out = run(&route, MESSAGE, Some("a@old.com"));
        assert_eq!(out.envelope_from.as_deref(), Some("bounce@newbrand.com"));
    }

    #[test]
    fn an_envelope_template_cannot_inject_an_smtp_command() {
        let route = compile(r#"envelope_from: "{{original.subject}}@newbrand.com""#);
        let raw: &[u8] = b"Subject: a\r\n b\r\nFrom: x@old.com\r\n\r\n";
        let out = run(&route, raw, Some("x@old.com"));
        let envelope = out.envelope_from.unwrap();
        assert!(
            !envelope.contains(' ') && !envelope.contains('\r'),
            "{envelope}"
        );
    }

    #[test]
    fn an_envelope_template_that_renders_empty_falls_back_to_what_arrived() {
        let route = compile(r#"envelope_from: "{{original.header[\"X-Absent\"]}}""#);
        let out = run(&route, MESSAGE, Some("jane@oldbrand.com"));
        assert_eq!(out.envelope_from.as_deref(), Some("jane@oldbrand.com"));
    }

    // -- §6.3 in place -----------------------------------------------------

    #[test]
    fn a_decoded_display_name_is_re_encoded_on_the_way_out() {
        let route = compile(
            r#"
envelope_from: "b@new.com"
set_headers:
  From: "{{original.from.display_name}} <sales@newbrand.com>"
"#,
        );
        let raw: &[u8] = b"From: =?UTF-8?B?SsOkbmU=?= <jane@old.com>\r\n\r\nbody\r\n";
        let out = text(&run(&route, raw, Some("jane@old.com")));
        assert!(
            out.contains("From: =?UTF-8?B?SsOkbmU=?= <sales@newbrand.com>\r\n"),
            "{out}"
        );
    }

    #[test]
    fn a_multi_recipient_message_renders_recipient_variables_empty() {
        // §6.3 scopes them to the single-recipient case; O-9 is phase 9.
        let route = compile(
            r#"
envelope_from: "b@new.com"
set_headers:
  X-To: "{{recipient.domain}}"
"#,
        );
        let rcpt = ["a@x.com".to_string(), "b@y.com".to_string()];
        let uuid = || String::new();
        let out = rewrite(&route, &inbound(MESSAGE, Some("j@old.com"), &rcpt, &uuid));
        assert!(String::from_utf8(out.raw).unwrap().contains("X-To: \r\n"));
    }

    // -- compilation -------------------------------------------------------

    #[test]
    fn a_bad_template_names_the_key_it_came_from() {
        let identity: Identity = serde_yaml_ng::from_str(
            r#"
envelope_from: "b@new.com"
set_headers:
  From: "{{original.frm.address}}"
"#,
        )
        .unwrap();
        let errs = RouteRewrite::compile(&identity).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, "set_headers.From");
    }

    #[test]
    fn every_bad_template_is_reported_not_just_the_first() {
        // §4.2's rule: "Report all violations, not just the first."
        let identity: Identity = serde_yaml_ng::from_str(
            r#"
envelope_from: "{{nope}}"
set_headers:
  From: "{{also.nope}}"
  Sender: "{{still.nope}}"
"#,
        )
        .unwrap();
        let errs = RouteRewrite::compile(&identity).unwrap_err();
        assert_eq!(errs.len(), 3);
    }
}
