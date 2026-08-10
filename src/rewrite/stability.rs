//! `SPEC.md` §6.6 — the rewrite stability property, as a startup check.
//!
//! ```text
//! rewrite(route, rewrite(route, m)) == rewrite(route, m)
//! ```
//!
//! §6.6 is explicit that this "does not model a real execution path" — chain
//! fall-through skips ineligible routes *before* any rewriting, and §3.3 rules
//! out failover after a downstream failure, so two rewrites never touch one
//! message. What it models is **arrangement B of §1.1**: if the application has
//! already been reconfigured to send the target identity, does this route leave
//! it alone? Composing the rewrite with itself asks exactly that question,
//! because the output of pass 1 *is* a message already carrying the target
//! identity.
//!
//! ## On excluding the volatile variables
//!
//! §6.6 says `uuid`, `now.*` and `correlation_id` are "excluded from the
//! comparison". They are excluded here by being **pinned** rather than by
//! skipping the headers that use them: the probe renders both passes at one
//! instant with one UUID. That is strictly stronger. Skipping the header would
//! also skip the literal part around the variable, so
//! `Message-ID: <{{uuid}}@{{original.from.domain}}>` — which is genuinely
//! unstable, because the domain part reads a field `From:` overwrites — would
//! go unreported. Pinning catches it.
//!
//! `Received:` is the one field genuinely excluded: pass 2 prepends a second one
//! by design, and §6.1 step 8 says it should.

use std::collections::BTreeSet;

use super::{headers, rewrite, Inbound, Received, Rewritten, RouteRewrite, Var};

/// The synthetic probe's own domain. RFC 2606 reserves `.invalid` precisely so
/// that a name which must never resolve cannot collide with a real one.
const PROBE_DOMAIN: &str = "probe.invalid";

/// One field that changed between the two passes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unstable {
    /// `envelope_from`, or a header name.
    pub field: String,
    pub first: String,
    pub second: String,
    /// §6.6 field classes: an identity field is fatal with no override.
    pub identity: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub unstable: Vec<Unstable>,
    /// §6.6: "Naming a header that is in fact stable is also a startup `WARN` —
    /// it means either the declaration is stale or the intent was
    /// misunderstood."
    pub declared_but_stable: Vec<String>,
}

impl Report {
    pub fn is_stable(&self) -> bool {
        self.unstable.is_empty()
    }
}

/// Run the property against a synthetic probe message.
pub fn probe(route: &RouteRewrite) -> Report {
    let message = probe_message(route);
    let recipients = [format!("rcpt@{PROBE_DOMAIN}")];
    let envelope = format!("probe-envelope@{PROBE_DOMAIN}");

    // Pinned, so the two passes differ only where the *route* makes them differ.
    let now = chrono::DateTime::from_timestamp(1_767_225_600, 0).expect("a valid instant");
    let uuid = || "00000000-0000-4000-8000-000000000000".to_string();

    let pass = |raw: &[u8], envelope_from: Option<&str>| -> Rewritten {
        rewrite(
            route,
            &Inbound {
                raw,
                envelope_from,
                recipients: &recipients,
                route_name: "probe",
                correlation_id: "probe",
                received: Received {
                    helo: "probe",
                    peer: "127.0.0.1",
                    by: "probe",
                    authenticated: false,
                },
                now,
                uuid: &uuid,
            },
        )
    };

    let first = pass(&message, Some(&envelope));
    // Composition: pass 2 sees pass 1's message *and* pass 1's envelope sender.
    // Feeding it the original envelope would test a message that cannot occur.
    let second = pass(&first.raw, first.envelope_from.as_deref());

    compare(route, &first, &second)
}

fn compare(route: &RouteRewrite, first: &Rewritten, second: &Rewritten) -> Report {
    let mut report = Report::default();

    if first.envelope_from != second.envelope_from {
        report.unstable.push(Unstable {
            field: "envelope_from".to_string(),
            first: first.envelope_from.clone().unwrap_or_default(),
            second: second.envelope_from.clone().unwrap_or_default(),
            identity: true,
        });
    }

    let a = headers::split(&first.raw).headers;
    let b = headers::split(&second.raw).headers;

    let mut names: BTreeSet<String> = BTreeSet::new();
    for name in a.names().into_iter().chain(b.names()) {
        // §6.1 step 8 prepends one per pass. Excluded by construction, not by
        // oversight (D-002 excludes it from §12.3's comparison for the same
        // reason).
        if name.eq_ignore_ascii_case("Received") || name.is_empty() {
            continue;
        }
        names.insert(name.to_ascii_lowercase());
    }

    let unstable_now: BTreeSet<String> = names
        .iter()
        .filter(|name| a.get_all(name) != b.get_all(name))
        .cloned()
        .collect();

    for name in &unstable_now {
        let display = canonical_name(route, name);
        report.unstable.push(Unstable {
            field: display.clone(),
            first: a.get_all(name).join(", "),
            second: b.get_all(name).join(", "),
            identity: is_identity_header(&display),
        });
    }

    for declared in &route.unstable_headers {
        if !unstable_now.contains(&declared.to_ascii_lowercase()) {
            report.declared_but_stable.push(declared.clone());
        }
    }

    report
}

/// Report a header under the spelling the operator used, where we have one.
fn canonical_name(route: &RouteRewrite, lowercase: &str) -> String {
    route
        .set_headers
        .iter()
        .map(|(name, _)| name.as_str())
        .chain(route.unstable_headers.iter().map(String::as_str))
        .find(|name| name.eq_ignore_ascii_case(lowercase))
        .map(str::to_string)
        .unwrap_or_else(|| lowercase.to_string())
}

/// §6.6 field classes. `envelope_from` is handled separately — it is an identity
/// *field* but not a header.
fn is_identity_header(name: &str) -> bool {
    crate::config::validate::IDENTITY_HEADERS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name))
}

/// A message shaped to make the route's own templates say something.
///
/// Every variable the route reads is given a value distinguishable from anything
/// the route could write, so "the second pass read what the first pass wrote"
/// shows up as a difference rather than as a coincidence.
fn probe_message(route: &RouteRewrite) -> Vec<u8> {
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for tmpl in
        std::iter::once(&route.envelope_from).chain(route.set_headers.iter().map(|(_, t)| t))
    {
        for var in tmpl.vars() {
            if let Var::Header(name) = var {
                referenced.insert(name.clone());
            }
        }
    }

    let mut out = format!(
        "From: Probe Display <probe-from@{PROBE_DOMAIN}>\r\n\
         To: rcpt@{PROBE_DOMAIN}\r\n\
         Subject: probe subject\r\n\
         Message-ID: <probe-message-id@{PROBE_DOMAIN}>\r\n\
         Date: Thu, 1 Jan 2026 00:00:00 +0000\r\n"
    );

    for name in referenced {
        // Not for headers the probe already carries — overwriting `Subject:`
        // with a placeholder would weaken the check.
        if ["from", "to", "subject", "message-id", "date"]
            .contains(&name.to_ascii_lowercase().as_str())
        {
            continue;
        }
        out.push_str(&format!("{name}: probe-value-for-{name}\r\n"));
    }

    out.push_str("\r\nProbe body.\r\n");
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Identity;

    fn route(yaml: &str) -> RouteRewrite {
        let identity: Identity = serde_yaml_ng::from_str(yaml).expect("fixture parses");
        RouteRewrite::compile(&identity).expect("fixture compiles")
    }

    // -- the case §6.6 is written about -----------------------------------

    #[test]
    fn the_spec_worked_example_reports_reply_to_and_only_reply_to() {
        // §6.6's own table: the From: rewrite is stable, the Reply-To is not.
        let r = probe(&route(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  From: "{{original.from.display_name}} <sales@newbrand.com>"
  Reply-To: "{{original.from.address}}"
"#,
        ));
        assert_eq!(r.unstable.len(), 1, "{:?}", r.unstable);
        assert_eq!(r.unstable[0].field, "Reply-To");
        assert!(!r.unstable[0].identity);
    }

    #[test]
    fn declaring_it_does_not_make_it_stable_it_makes_it_downgradable() {
        // The report is the same; §4.2 is where the declaration changes the
        // consequence from error to WARN.
        //
        // Note the `From:` rewrite: without it, `Reply-To` reads a field nothing
        // overwrites and is perfectly stable. Instability is a property of the
        // *pair*, not of the Reply-To template on its own.
        let r = probe(&route(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  From: "<sales@newbrand.com>"
  Reply-To: "{{original.from.address}}"
unstable_headers: ["Reply-To"]
"#,
        ));
        assert_eq!(r.unstable.len(), 1);
        assert!(r.declared_but_stable.is_empty());
    }

    // -- stable configurations ---------------------------------------------

    #[test]
    fn a_pure_literal_identity_is_stable() {
        let r = probe(&route(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  From: "Sales <sales@newbrand.com>"
  Sender: "sales@newbrand.com"
"#,
        ));
        assert!(r.is_stable(), "{:?}", r.unstable);
    }

    #[test]
    fn reading_a_field_nothing_writes_is_stable() {
        let r = probe(&route(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  From: "{{original.from.display_name}} <sales@newbrand.com>"
  X-Original-Subject: "{{original.subject}}"
"#,
        ));
        assert!(r.is_stable(), "{:?}", r.unstable);
    }

    #[test]
    fn a_uuid_message_id_is_stable_because_the_variable_is_pinned() {
        // §6.6 excludes volatile variables from the comparison. Pinning them is
        // how that exclusion is implemented.
        let r = probe(&route(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  Message-ID: "<{{uuid}}@newbrand.com>"
  X-Sent: "{{now.date}} {{correlation_id}}"
"#,
        ));
        assert!(r.is_stable(), "{:?}", r.unstable);
    }

    #[test]
    fn removing_a_header_is_stable() {
        let r = probe(&route(
            r#"
envelope_from: "bounce@newbrand.com"
remove_headers: ["Return-Path", "Subject"]
"#,
        ));
        assert!(r.is_stable(), "{:?}", r.unstable);
    }

    #[test]
    fn a_route_that_rewrites_nothing_is_stable() {
        // Pass-through: §1.1's "degenerate case where the target identity
        // already equals the incoming one".
        let r = probe(&route(r#"envelope_from: "probe-envelope@probe.invalid""#));
        assert!(r.is_stable(), "{:?}", r.unstable);
    }

    // -- the cases the pinning argument exists for -------------------------

    #[test]
    fn a_volatile_variable_does_not_hide_an_unstable_literal_around_it() {
        // The reason volatile variables are pinned rather than skipped: the
        // domain part reads From:, which the same pass overwrites.
        let r = probe(&route(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  From: "<sales@newbrand.com>"
  Message-ID: "<{{uuid}}@{{original.from.domain}}>"
"#,
        ));
        assert_eq!(r.unstable.len(), 1, "{:?}", r.unstable);
        assert_eq!(r.unstable[0].field, "Message-ID");
        assert!(r.unstable[0].identity, "Message-ID is an identity field");
    }

    // -- identity classification -------------------------------------------

    #[test]
    fn an_unstable_from_is_classified_as_an_identity_field() {
        let r = probe(&route(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  From: "{{original.from.local}}@newbrand.com"
"#,
        ));
        // local part of `probe-from@probe.invalid` is `probe-from`; pass 2 reads
        // `probe-from@newbrand.com` and writes the same. Stable, in fact —
        // assert the classification through Sender instead.
        let _ = r;

        let r = probe(&route(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  From: "<sales@newbrand.com>"
  Sender: "{{original.from.address}}"
"#,
        ));
        assert_eq!(r.unstable.len(), 1, "{:?}", r.unstable);
        assert_eq!(r.unstable[0].field, "Sender");
        assert!(r.unstable[0].identity);
    }

    #[test]
    fn an_unstable_envelope_sender_is_reported_as_an_identity_field() {
        let r = probe(&route(
            r#"
envelope_from: "{{original.envelope_from.local}}-x@newbrand.com"
"#,
        ));
        let e = r
            .unstable
            .iter()
            .find(|u| u.field == "envelope_from")
            .expect("should be unstable");
        assert!(e.identity);
        assert_ne!(e.first, e.second);
    }

    // -- the stale-declaration warning -------------------------------------

    #[test]
    fn declaring_a_header_that_is_stable_is_reported() {
        // §6.6: "either the declaration is stale or the intent was
        // misunderstood, and both are worth surfacing."
        let r = probe(&route(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  Reply-To: "support@newbrand.com"
unstable_headers: ["Reply-To"]
"#,
        ));
        assert!(r.is_stable());
        assert_eq!(r.declared_but_stable, ["Reply-To"]);
    }

    #[test]
    fn declaring_a_header_the_route_never_touches_is_reported() {
        let r = probe(&route(
            r#"
envelope_from: "bounce@newbrand.com"
unstable_headers: ["X-Nothing"]
"#,
        ));
        assert_eq!(r.declared_but_stable, ["X-Nothing"]);
    }

    // -- the probe itself ---------------------------------------------------

    #[test]
    fn the_probe_carries_every_header_the_route_reads() {
        let r = route(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  X-A: "{{original.header[\"X-Campaign\"]}}"
  X-B: "{{original.header['X-Tenant']}}"
"#,
        );
        let probe = String::from_utf8(probe_message(&r)).unwrap();
        assert!(
            probe.contains("X-Campaign: probe-value-for-X-Campaign\r\n"),
            "{probe}"
        );
        assert!(
            probe.contains("X-Tenant: probe-value-for-X-Tenant\r\n"),
            "{probe}"
        );
    }

    #[test]
    fn the_probe_does_not_overwrite_a_header_it_already_defines() {
        let r = route(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  X-A: "{{original.header[\"Subject\"]}}"
"#,
        );
        let probe = String::from_utf8(probe_message(&r)).unwrap();
        assert_eq!(probe.matches("Subject:").count(), 1, "{probe}");
        assert!(probe.contains("Subject: probe subject"), "{probe}");
    }

    #[test]
    fn copying_a_header_the_route_also_sets_is_caught() {
        let r = probe(&route(
            r#"
envelope_from: "bounce@newbrand.com"
set_headers:
  X-Campaign: "rewritten"
  X-Copy: "{{original.header[\"X-Campaign\"]}}"
"#,
        ));
        assert_eq!(r.unstable.len(), 1, "{:?}", r.unstable);
        assert_eq!(r.unstable[0].field, "X-Copy");
    }
}
