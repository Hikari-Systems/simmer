//! `SPEC.md` §6.3 — the templating language for `set_headers` values and
//! `identity.envelope_from`.
//!
//! Deliberately not a general-purpose template engine. §1.1 constraint 1 says
//! rewrites are **absolute assignments, never relative transformations**, and a
//! language with conditionals, filters or arithmetic is exactly how a relative
//! transformation gets written by accident. What is here is substitution of a
//! closed set of named variables into literal text, and nothing else.
//!
//! Two consequences worth stating, because both are load-bearing elsewhere:
//!
//! - **An unknown variable is a parse error**, not an empty string (D-034). §4.2
//!   does not list this rule; the reasoning is in `DECISIONS.md`.
//! - **Volatility is a property of the parsed template**, not a guess made by
//!   string-matching the source. §6.6 excludes `uuid`, `now.*` and
//!   `correlation_id` from the stability comparison, and [`Template::is_volatile`]
//!   is what the stability probe asks.

use std::fmt;

use super::encode;

/// A parsed template. Cheap to render, so routes parse theirs once at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    segments: Vec<Segment>,
    /// The original text, kept for error messages and for logging a route's
    /// configuration back at the operator.
    source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Var(Var),
}

/// The §6.3 variable table, closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Var {
    FromAddress,
    FromLocal,
    FromDomain,
    FromDisplayName,
    EnvelopeFromAddress,
    EnvelopeFromLocal,
    EnvelopeFromDomain,
    MessageId,
    Subject,
    /// `original.header["X-Foo"]`. The name is stored as written; lookup is
    /// case-insensitive per RFC 5322.
    Header(String),
    RecipientAddress,
    RecipientLocal,
    RecipientDomain,
    RouteName,
    CorrelationId,
    Uuid,
    NowRfc3339,
    NowDate,
}

impl Var {
    /// §6.6: "Volatile template variables (`uuid`, `now.*`, `correlation_id`) are
    /// excluded from the comparison." A header whose template touches one of
    /// these cannot be compared across two renders, because the two renders
    /// happen at different instants with different identifiers.
    pub fn is_volatile(&self) -> bool {
        matches!(
            self,
            Var::Uuid | Var::NowRfc3339 | Var::NowDate | Var::CorrelationId
        )
    }

    /// §6.3's per-recipient splitting warning, and §6.3's "single-recipient case
    /// only" note on the `recipient.*` family.
    pub fn is_recipient(&self) -> bool {
        matches!(
            self,
            Var::RecipientAddress | Var::RecipientLocal | Var::RecipientDomain
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("unterminated '{{{{' — expected a closing '}}}}'")]
    Unterminated,
    #[error(
        "unknown template variable '{0}'. Available: original.from.{{address,local,domain,\
         display_name}}, original.envelope_from.{{address,local,domain}}, original.message_id, \
         original.subject, original.header[\"Name\"], recipient.{{address,local,domain}}, \
         route.name, correlation_id, uuid, now.{{rfc3339,date}} (§6.3)"
    )]
    UnknownVariable(String),
    #[error("malformed header reference '{0}' — expected original.header[\"Name\"]")]
    MalformedHeaderRef(String),
    #[error("empty '{{{{}}}}' — a template variable name is required")]
    EmptyVariable,
}

impl Template {
    /// Parse, rejecting anything §6.3 does not name (D-034).
    pub fn parse(source: &str) -> Result<Template, ParseError> {
        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut rest = source;

        while let Some(open) = rest.find("{{") {
            literal.push_str(&rest[..open]);
            let after = &rest[open + 2..];
            let close = after.find("}}").ok_or(ParseError::Unterminated)?;
            let name = after[..close].trim();

            if !literal.is_empty() {
                segments.push(Segment::Literal(std::mem::take(&mut literal)));
            }
            segments.push(Segment::Var(parse_var(name)?));
            rest = &after[close + 2..];
        }

        literal.push_str(rest);
        if !literal.is_empty() {
            segments.push(Segment::Literal(literal));
        }

        Ok(Template {
            segments,
            source: source.to_string(),
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Every variable this template references, in order of appearance.
    pub fn vars(&self) -> impl Iterator<Item = &Var> {
        self.segments.iter().filter_map(|s| match s {
            Segment::Var(v) => Some(v),
            Segment::Literal(_) => None,
        })
    }

    /// §6.6 — whether this template's output can differ between two renders of
    /// the same message for reasons that are not instability.
    pub fn is_volatile(&self) -> bool {
        self.vars().any(Var::is_volatile)
    }

    /// §6.3 — whether rendering this forces per-recipient splitting.
    pub fn references_recipient(&self) -> bool {
        self.vars().any(Var::is_recipient)
    }

    /// Whether this template is pure literal text. A pure-literal assignment is
    /// stable by construction — it reads nothing, so nothing it writes can
    /// change what a second pass reads — which lets the §6.6 probe skip it.
    pub fn is_literal(&self) -> bool {
        self.vars().next().is_none()
    }

    /// Render against a message.
    ///
    /// Infallible: every variable resolves, and §6.3's table defines the absent
    /// cases as the empty string ("`original.message_id` — original
    /// `Message-ID`, empty if absent"). A missing value must not fail a message
    /// that has already been accepted from the client.
    ///
    /// Used directly for `envelope_from`, which is not a header. Headers go
    /// through [`Template::render_header`], which knows where the substituted
    /// text lands.
    pub fn render(&self, ctx: &Context<'_>) -> String {
        let mut out = String::with_capacity(self.source.len() + 32);
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => out.push_str(text),
                Segment::Var(v) => out.push_str(&ctx.resolve(v)),
            }
        }
        out
    }

    /// Render for a named header, quoting substituted values that land in the
    /// display-name position of an address header, then conforming the result.
    ///
    /// The position analysis is what makes quoting safe to do at all. Three
    /// cases, all of them real configurations:
    ///
    /// | Template | The variable is | Treatment |
    /// |---|---|---|
    /// | `{{original.from.display_name}} <sales@newbrand.com>` | a display name | quote if it has specials |
    /// | `{{original.from.address}}` | the address itself | leave alone |
    /// | `Sales <{{original.from.local}}@newbrand.com>` | inside the `addr-spec` | leave alone |
    ///
    /// A value is a display name when an `<` follows it before any top-level
    /// comma — that is precisely what makes the text before it a `phrase` in RFC
    /// 5322's `name-addr` production. Quoting an `addr-spec` would corrupt it
    /// just as surely as leaving a comma in a display name would.
    pub fn render_header(&self, header_name: &str, ctx: &Context<'_>) -> String {
        let address = is_address_header(header_name);
        let mut out = String::with_capacity(self.source.len() + 32);
        let mut in_angles = false;
        let mut in_quotes = false;

        for (i, segment) in self.segments.iter().enumerate() {
            match segment {
                Segment::Literal(text) => {
                    out.push_str(text);
                    for c in text.chars() {
                        match c {
                            '"' => in_quotes = !in_quotes,
                            '<' if !in_quotes => in_angles = true,
                            '>' if !in_quotes => in_angles = false,
                            _ => {}
                        }
                    }
                }
                Segment::Var(v) => {
                    let value = ctx.resolve(v);
                    let is_display_name = address && !in_angles && self.followed_by_angle_addr(i);
                    if !is_display_name {
                        out.push_str(&value);
                    } else if in_quotes {
                        out.push_str(&encode::escape_quoted(&value));
                    } else {
                        out.push_str(&encode::quote_phrase_component(&value));
                    }
                }
            }
        }

        conform(header_name, &out)
    }

    /// Whether an `<` appears after segment `i` before any top-level comma.
    fn followed_by_angle_addr(&self, i: usize) -> bool {
        for segment in &self.segments[i + 1..] {
            let Segment::Literal(text) = segment else {
                // A variable's *value* is never grammar — only the operator's
                // literal text can open an angle-addr or end a list element.
                continue;
            };
            let mut in_quotes = false;
            for c in text.chars() {
                match c {
                    '"' => in_quotes = !in_quotes,
                    '<' if !in_quotes => return true,
                    ',' if !in_quotes => return false,
                    _ => {}
                }
            }
        }
        false
    }
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

fn parse_var(name: &str) -> Result<Var, ParseError> {
    if name.is_empty() {
        return Err(ParseError::EmptyVariable);
    }

    // `original.header["X-Foo"]` is the one variable with an argument. Handled
    // before the table because its tail is operator-supplied.
    if let Some(tail) = name.strip_prefix("original.header") {
        let inner = tail
            .trim()
            .strip_prefix('[')
            .and_then(|t| t.trim().strip_suffix(']'))
            .map(str::trim)
            .ok_or_else(|| ParseError::MalformedHeaderRef(name.to_string()))?;

        let quoted = inner
            .strip_prefix('"')
            .and_then(|t| t.strip_suffix('"'))
            .or_else(|| inner.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')))
            .ok_or_else(|| ParseError::MalformedHeaderRef(name.to_string()))?;

        if quoted.is_empty() {
            return Err(ParseError::MalformedHeaderRef(name.to_string()));
        }
        return Ok(Var::Header(quoted.to_string()));
    }

    Ok(match name {
        "original.from.address" => Var::FromAddress,
        "original.from.local" => Var::FromLocal,
        "original.from.domain" => Var::FromDomain,
        "original.from.display_name" => Var::FromDisplayName,
        "original.envelope_from.address" => Var::EnvelopeFromAddress,
        "original.envelope_from.local" => Var::EnvelopeFromLocal,
        "original.envelope_from.domain" => Var::EnvelopeFromDomain,
        "original.message_id" => Var::MessageId,
        "original.subject" => Var::Subject,
        "recipient.address" => Var::RecipientAddress,
        "recipient.local" => Var::RecipientLocal,
        "recipient.domain" => Var::RecipientDomain,
        "route.name" => Var::RouteName,
        "correlation_id" => Var::CorrelationId,
        "uuid" => Var::Uuid,
        "now.rfc3339" => Var::NowRfc3339,
        "now.date" => Var::NowDate,
        other => return Err(ParseError::UnknownVariable(other.to_string())),
    })
}

// ---------------------------------------------------------------------------
// render context
// ---------------------------------------------------------------------------

/// An address split the way §6.3's table wants it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddressParts {
    pub address: String,
    pub local: String,
    pub domain: String,
}

impl AddressParts {
    /// Split at the **last** `@`, which is where RFC 5321 puts the domain
    /// separator: a quoted local part may legally contain one.
    pub fn split(address: &str) -> AddressParts {
        let address = address.trim();
        match address.rsplit_once('@') {
            Some((local, domain)) => AddressParts {
                address: address.to_string(),
                local: local.to_string(),
                domain: domain.to_string(),
            },
            // No `@` at all — `<>`, or a local-only address. Neither has a
            // domain, and §6.3 has no failure mode, so the parts are empty.
            None => AddressParts {
                address: address.to_string(),
                local: address.to_string(),
                domain: String::new(),
            },
        }
    }
}

/// Everything §6.3 can name, assembled once per message.
///
/// Borrowed rather than owned: the caller already has the parsed message and the
/// route, and a 25 MiB message should not have its header values cloned twice.
pub struct Context<'a> {
    pub from: AddressParts,
    pub from_display_name: String,
    pub envelope_from: AddressParts,
    pub message_id: String,
    pub subject: String,
    /// `original.header["X-Foo"]`. Case-insensitive; returns the first instance.
    pub header: &'a dyn Fn(&str) -> Option<String>,
    /// `None` in the multi-recipient case, which §6.3 declares out of scope for
    /// these variables. Renders empty rather than failing.
    pub recipient: Option<AddressParts>,
    pub route_name: &'a str,
    pub correlation_id: &'a str,
    /// Injected rather than read from the clock, so the §6.6 probe and the
    /// property test can render twice at the same instant and compare.
    pub now: chrono::DateTime<chrono::Utc>,
    /// Injected for the same reason. Production passes a fresh v4 per render.
    pub uuid: &'a dyn Fn() -> String,
}

impl Context<'_> {
    fn resolve(&self, v: &Var) -> String {
        match v {
            Var::FromAddress => self.from.address.clone(),
            Var::FromLocal => self.from.local.clone(),
            Var::FromDomain => self.from.domain.clone(),
            Var::FromDisplayName => self.from_display_name.clone(),
            Var::EnvelopeFromAddress => self.envelope_from.address.clone(),
            Var::EnvelopeFromLocal => self.envelope_from.local.clone(),
            Var::EnvelopeFromDomain => self.envelope_from.domain.clone(),
            Var::MessageId => self.message_id.clone(),
            Var::Subject => self.subject.clone(),
            Var::Header(name) => (self.header)(name).unwrap_or_default(),
            Var::RecipientAddress => self
                .recipient
                .as_ref()
                .map(|r| r.address.clone())
                .unwrap_or_default(),
            Var::RecipientLocal => self
                .recipient
                .as_ref()
                .map(|r| r.local.clone())
                .unwrap_or_default(),
            Var::RecipientDomain => self
                .recipient
                .as_ref()
                .map(|r| r.domain.clone())
                .unwrap_or_default(),
            Var::RouteName => self.route_name.to_string(),
            Var::CorrelationId => self.correlation_id.to_string(),
            Var::Uuid => (self.uuid)(),
            Var::NowRfc3339 => self.now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            Var::NowDate => self.now.format("%Y-%m-%d").to_string(),
        }
    }
}

/// §6.3: "Rendered header values must be RFC 5322-conformant; non-ASCII in
/// display names is RFC 2047-encoded automatically."
///
/// Applied after rendering rather than per-variable, because whether a value is
/// a display name is a property of where it lands, not of where it came from:
/// `{{original.from.display_name}}` is a phrase in `From:` and a plain word in
/// `X-Original-Sender:`.
pub fn conform(header_name: &str, value: &str) -> String {
    if is_address_header(header_name) {
        // Address headers go through the full treatment whether or not they are
        // ASCII: a display name can need *quoting* without needing encoding, and
        // an unquoted comma silently splits one mailbox into two.
        return encode::conform_address_list(value);
    }

    if value.is_ascii() {
        // Already conformant, including the case that matters most for §6.6:
        // an encoded-word is pure ASCII, so a second pass leaves it alone
        // instead of double-encoding it.
        return encode::sanitise(value);
    }

    encode::encode_unstructured(value)
}

/// The headers whose grammar is `mailbox-list` or `address-list`, where a
/// leading phrase is a display name rather than free text.
fn is_address_header(name: &str) -> bool {
    const ADDRESS_HEADERS: [&str; 10] = [
        "From",
        "Sender",
        "Reply-To",
        "To",
        "Cc",
        "Bcc",
        "Resent-From",
        "Resent-Sender",
        "Resent-To",
        "Resent-Cc",
    ];
    ADDRESS_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(
        header: &'a dyn Fn(&str) -> Option<String>,
        uuid: &'a dyn Fn() -> String,
    ) -> Context<'a> {
        Context {
            from: AddressParts::split("jane@oldbrand.com"),
            from_display_name: "Jane Smith".to_string(),
            envelope_from: AddressParts::split("bounce@oldbrand.com"),
            message_id: "<abc@oldbrand.com>".to_string(),
            subject: "Your order".to_string(),
            header,
            recipient: Some(AddressParts::split("bob@example.net")),
            route_name: "warming",
            correlation_id: "cid-1",
            now: chrono::DateTime::parse_from_rfc3339("2026-08-10T12:30:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            uuid,
        }
    }

    fn render(source: &str) -> String {
        let header = |name: &str| match name.to_ascii_lowercase().as_str() {
            "x-campaign" => Some("spring".to_string()),
            _ => None,
        };
        let uuid = || "11111111-2222-3333-4444-555555555555".to_string();
        Template::parse(source)
            .unwrap()
            .render(&ctx(&header, &uuid))
    }

    // -- the §6.3 table, one case each ----------------------------------

    #[test]
    fn every_variable_in_the_table_resolves() {
        assert_eq!(render("{{original.from.address}}"), "jane@oldbrand.com");
        assert_eq!(render("{{original.from.local}}"), "jane");
        assert_eq!(render("{{original.from.domain}}"), "oldbrand.com");
        assert_eq!(render("{{original.from.display_name}}"), "Jane Smith");
        assert_eq!(
            render("{{original.envelope_from.address}}"),
            "bounce@oldbrand.com"
        );
        assert_eq!(render("{{original.envelope_from.local}}"), "bounce");
        assert_eq!(render("{{original.envelope_from.domain}}"), "oldbrand.com");
        assert_eq!(render("{{original.message_id}}"), "<abc@oldbrand.com>");
        assert_eq!(render("{{original.subject}}"), "Your order");
        assert_eq!(render("{{original.header[\"X-Campaign\"]}}"), "spring");
        assert_eq!(render("{{recipient.address}}"), "bob@example.net");
        assert_eq!(render("{{recipient.local}}"), "bob");
        assert_eq!(render("{{recipient.domain}}"), "example.net");
        assert_eq!(render("{{route.name}}"), "warming");
        assert_eq!(render("{{correlation_id}}"), "cid-1");
        assert_eq!(render("{{uuid}}"), "11111111-2222-3333-4444-555555555555");
        assert_eq!(render("{{now.rfc3339}}"), "2026-08-10T12:30:00Z");
        assert_eq!(render("{{now.date}}"), "2026-08-10");
    }

    #[test]
    fn literals_and_variables_interleave() {
        assert_eq!(
            render("{{original.from.display_name}} <sales@newbrand.com>"),
            "Jane Smith <sales@newbrand.com>"
        );
        assert_eq!(
            render("<{{uuid}}@newbrand.com>"),
            "<11111111-2222-3333-4444-555555555555@newbrand.com>"
        );
        assert_eq!(render("no variables here"), "no variables here");
        assert_eq!(render(""), "");
    }

    #[test]
    fn whitespace_inside_the_braces_is_ignored() {
        assert_eq!(render("{{ original.from.domain }}"), "oldbrand.com");
        assert_eq!(render("{{original.header[ \"X-Campaign\" ]}}"), "spring");
    }

    #[test]
    fn an_absent_header_renders_empty_rather_than_failing() {
        // §6.3: "empty if absent". A message that has already been accepted must
        // not fail because a header the operator hoped for is missing.
        assert_eq!(render("{{original.header[\"X-Missing\"]}}"), "");
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        assert_eq!(render("{{original.header[\"x-CAMPAIGN\"]}}"), "spring");
    }

    #[test]
    fn a_missing_recipient_renders_empty() {
        // §6.3 scopes recipient.* to the single-recipient case; the multi case
        // renders empty rather than failing the message.
        let header = |_: &str| None;
        let uuid = || String::new();
        let mut c = ctx(&header, &uuid);
        c.recipient = None;
        let t = Template::parse("{{recipient.address}}|{{recipient.local}}").unwrap();
        assert_eq!(t.render(&c), "|");
    }

    // -- D-034: unknown variables are fatal -----------------------------

    #[test]
    fn an_unknown_variable_is_a_parse_error() {
        // D-034. The typo that motivates it: `frm` for `from` in an identity
        // header would otherwise emit `From: <sales@newbrand.com>` for weeks.
        let err = Template::parse("{{original.frm.address}}").unwrap_err();
        assert_eq!(
            err,
            ParseError::UnknownVariable("original.frm.address".into())
        );
        // The message lists what is available, so the typo is fixable from the
        // startup log without opening SPEC.md.
        assert!(err.to_string().contains("original.from."));
        assert!(err.to_string().contains("§6.3"));
    }

    #[test]
    fn a_near_miss_on_a_real_variable_is_still_an_error() {
        assert!(Template::parse("{{original.from.adress}}").is_err());
        assert!(Template::parse("{{now}}").is_err());
        assert!(Template::parse("{{Original.From.Address}}").is_err());
    }

    #[test]
    fn an_unterminated_variable_is_a_parse_error() {
        assert_eq!(
            Template::parse("{{original.from.address").unwrap_err(),
            ParseError::Unterminated
        );
    }

    #[test]
    fn an_empty_variable_is_a_parse_error() {
        assert_eq!(
            Template::parse("{{}}").unwrap_err(),
            ParseError::EmptyVariable
        );
        assert_eq!(
            Template::parse("{{   }}").unwrap_err(),
            ParseError::EmptyVariable
        );
    }

    #[test]
    fn a_malformed_header_reference_is_a_parse_error() {
        for bad in [
            "{{original.header}}",
            "{{original.header[X-Foo]}}",
            "{{original.header[\"X-Foo\"}}",
            "{{original.header[\"\"]}}",
        ] {
            assert!(
                matches!(
                    Template::parse(bad).unwrap_err(),
                    ParseError::MalformedHeaderRef(_) | ParseError::UnknownVariable(_)
                ),
                "{bad} should not parse"
            );
        }
    }

    #[test]
    fn single_quoted_header_names_are_accepted() {
        // YAML makes `"` inside a double-quoted scalar awkward; accepting both
        // costs nothing and the alternative is a config file full of backslashes.
        assert_eq!(render("{{original.header['X-Campaign']}}"), "spring");
    }

    // -- classification the rest of the engine asks about ---------------

    #[test]
    fn volatility_is_read_from_the_parsed_variables() {
        assert!(Template::parse("{{uuid}}@x.com").unwrap().is_volatile());
        assert!(Template::parse("{{now.date}}").unwrap().is_volatile());
        assert!(Template::parse("{{now.rfc3339}}").unwrap().is_volatile());
        assert!(Template::parse("{{correlation_id}}").unwrap().is_volatile());
        assert!(!Template::parse("{{original.from.address}}")
            .unwrap()
            .is_volatile());
        // The string "uuid" appearing as literal text is not volatility.
        assert!(!Template::parse("uuid <a@b.com>").unwrap().is_volatile());
    }

    #[test]
    fn recipient_references_are_read_from_the_parsed_variables() {
        assert!(Template::parse("{{recipient.domain}}")
            .unwrap()
            .references_recipient());
        assert!(!Template::parse("recipient.domain")
            .unwrap()
            .references_recipient());
        // The phase 1 string check missed this spacing; the parsed form cannot.
        assert!(Template::parse("{{  recipient.address  }}")
            .unwrap()
            .references_recipient());
    }

    #[test]
    fn a_pure_literal_is_recognised() {
        assert!(Template::parse("sales@newbrand.com").unwrap().is_literal());
        assert!(!Template::parse("{{original.from.local}}@newbrand.com")
            .unwrap()
            .is_literal());
    }

    // -- address splitting ----------------------------------------------

    #[test]
    fn an_address_splits_at_the_last_at_sign() {
        // A quoted local part may legally contain '@'.
        let p = AddressParts::split("\"odd@name\"@example.com");
        assert_eq!(p.local, "\"odd@name\"");
        assert_eq!(p.domain, "example.com");
    }

    #[test]
    fn an_address_with_no_domain_yields_empty_parts() {
        let p = AddressParts::split("");
        assert_eq!(p.address, "");
        assert_eq!(p.domain, "");
    }

    // -- §6.3 conformance -------------------------------------------------

    #[test]
    fn an_ascii_value_passes_through_unchanged() {
        assert_eq!(
            conform("From", "Jane Smith <sales@newbrand.com>"),
            "Jane Smith <sales@newbrand.com>"
        );
    }

    #[test]
    fn a_non_ascii_display_name_is_encoded_and_the_address_is_not() {
        let out = conform("From", "Jäne Smith <sales@newbrand.com>");
        assert!(out.ends_with("<sales@newbrand.com>"), "{out}");
        assert!(out.starts_with("=?UTF-8?B?"), "{out}");
        assert!(out.is_ascii());
    }

    #[test]
    fn encoding_a_value_twice_changes_nothing() {
        // The §6.6 property in miniature: an encoded-word is ASCII, so the
        // second pass takes the early return.
        let once = conform("Subject", "Grüße");
        assert_eq!(conform("Subject", &once), once);
    }
}
