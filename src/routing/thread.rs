//! §3.2 step 2a — thread affinity (D-090).
//!
//! A reply the application sends into a conversation Simmer started should
//! leave under the identity the recipient has already seen. Without this, the
//! ramp decides afresh for every message, and the recipient who replied to
//! `sales@newbrand.com` in the morning hears back from
//! `news@mail.established.com` in the afternoon, when the warming route has
//! spent its day.
//!
//! **Stateless, on purpose.** Every route's `Message-ID:` already carries a
//! literal domain of its own (§4.2 requires it when `thread_affinity` is on), so
//! an ID Simmer emitted names the route that emitted it. The recipient's mail
//! client puts that ID in `In-Reply-To:`/`References:` when they reply, and the
//! application carries it forward into its answer. Nothing is recorded, so
//! there is no table, no sweeper, no salt, and nothing that works differently
//! with two instances than with one.
//!
//! **The pin outranks the ramp, and nothing else.** Settled by the spec's
//! author (D-090): the pinned route is walked first, and for it alone the walk
//! does not apply §7.3's threshold and does not refuse for want of headroom — a
//! pinned reply with the day spent is reserved *past* the cap, still under the
//! row lock and still counted, so `committed` reads above `allowance` and says
//! so. Pause, strict preflight and a future `warmup.started` still eliminate
//! it: those say the route cannot send, not that it has sent enough, and an
//! operator's pause has to keep meaning "stop". An eliminated pin falls back to
//! the ordinary walk over the rest of the chain, in configured order.
//!
//! **Only the chain's own routes.** The sender rule decides which identities a
//! message may leave under (§3.2 step 1). An ID naming a route outside that
//! chain pins nothing — it cannot widen the set, only order it.

use crate::config::{Identity, Ramp};

/// How many message IDs one message may make Simmer consider. A `References:`
/// chain grows by one per turn of a conversation; RFC 5322 lets clients trim
/// it, and the long ones in the wild are a few dozen. This bounds the work a
/// hostile or broken header can cause, not a real thread.
pub const MAX_IDS: usize = 256;

/// The literal domain of a route's `Message-ID:` template, lowercased — the
/// value an emitted ID is matched against.
///
/// `None` when the route sets no `Message-ID:` (the application's own ID
/// passes through, and names no route), or when the domain is templated: a
/// domain that varies per message cannot be recognised on the way back.
/// §4.2 refuses both when `thread_affinity` is on.
///
/// Textual, like `preflight::literal_domain`: `<{{uuid}}@newbrand.com>` gives
/// `newbrand.com`. The angle brackets are optional because RFC 5322 is not
/// what `set_headers` is checked against — the rendered value is.
pub fn route_domain(identity: &Identity) -> Option<String> {
    let template = identity.set_headers.get("Message-ID")?.trim();
    let template = template.strip_suffix('>').unwrap_or(template);
    let (_, domain) = template.rsplit_once('@')?;
    let domain = domain.trim();
    if domain.is_empty()
        || domain.contains("{{")
        || domain.contains('}')
        || domain
            .chars()
            .any(|c| c.is_whitespace() || c == '<' || c == '>')
    {
        return None;
    }
    Some(normalise_domain(domain))
}

/// The message IDs a message refers to, most recent first.
///
/// `In-Reply-To:` first — it names the message being answered — then
/// `References:` from its end, which RFC 5322 §3.6.4 defines as the most recent.
/// So when a thread has changed identity (its route was paused mid-thread), the
/// identity the recipient saw last is the one that wins.
///
/// Each value is scanned for `<…>` tokens after comments are removed. A value
/// with no angle brackets at all is split on whitespace and commas instead,
/// which is what some clients write; a token without an `@` is not an ID and is
/// dropped. At most [`MAX_IDS`] come back.
pub fn referenced_ids(in_reply_to: &[String], references: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for value in in_reply_to {
        let mut ids = ids_in(value);
        ids.reverse();
        out.extend(ids);
    }
    for value in references.iter().rev() {
        let mut ids = ids_in(value);
        ids.reverse();
        out.extend(ids);
    }
    out.truncate(MAX_IDS);
    out
}

/// The result of looking for a pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pin {
    /// Nothing to go on: affinity is off, or the message refers to no ID. The
    /// ordinary case for every first message, and counted nowhere.
    None,
    /// The message refers to IDs, and none of them was emitted by a route in
    /// this chain. `simmer_thread_affinity_total{outcome="unmatched"}`.
    Unmatched,
    /// Walk this route first.
    Route(String),
}

impl Pin {
    pub fn route(&self) -> Option<&str> {
        match self {
            Pin::Route(r) => Some(r),
            _ => None,
        }
    }

    /// For §9.5's log line.
    pub fn as_log(&self) -> &str {
        match self {
            Pin::None => "-",
            Pin::Unmatched => "unmatched",
            Pin::Route(r) => r,
        }
    }
}

/// Find the route, in `chain`, that emitted the most recent ID in `ids`.
pub fn pin_for(ramp: &Ramp, chain: &[String], ids: &[String]) -> Pin {
    if ids.is_empty() {
        return Pin::None;
    }

    let domains: Vec<(&str, String)> = chain
        .iter()
        .filter_map(|name| {
            let route = ramp.route(name)?;
            Some((name.as_str(), route_domain(&route.identity)?))
        })
        .collect();

    for id in ids {
        let Some(domain) = id_domain(id) else {
            continue;
        };
        if let Some((name, _)) = domains.iter().find(|(_, d)| *d == domain) {
            return Pin::Route((*name).to_string());
        }
    }
    Pin::Unmatched
}

/// Everything §3.2 step 2a needs from a buffered message: its threading
/// headers, parsed, and matched against the chain. [`Pin::None`] without
/// parsing anything when `thread_affinity` is off.
pub fn pin_for_message(ramp: &Ramp, chain: &[String], raw: &[u8]) -> Pin {
    if !ramp.thread_affinity {
        return Pin::None;
    }
    let headers = crate::rewrite::headers::split(raw).headers;
    let ids = referenced_ids(
        &headers.get_all("In-Reply-To"),
        &headers.get_all("References"),
    );
    pin_for(ramp, chain, &ids)
}

/// The chain in the order §3.2 step 3 should walk it: the pinned route first,
/// then the rest in configured order. The chain unchanged when nothing is
/// pinned, or when the pin is not in it.
pub fn order(chain: &[String], pin: &Pin) -> Vec<String> {
    let Some(pinned) = pin.route() else {
        return chain.to_vec();
    };
    if !chain.iter().any(|r| r == pinned) {
        return chain.to_vec();
    }
    std::iter::once(pinned.to_string())
        .chain(chain.iter().filter(|r| *r != pinned).cloned())
        .collect()
}

/// `simmer_thread_affinity_total`, once the walk has an answer. `selected` is
/// the route that reserved, or `None` for an exhausted chain; `over_cap` is
/// whether it was reserved past the day's cap.
pub fn observe(ramp: &str, pin: &Pin, selected: Option<&str>, over_cap: bool) {
    if let Some(outcome) = outcome(pin, selected, over_cap) {
        let route = pin.route().unwrap_or("-");
        crate::metrics::thread_affinity(ramp, route, outcome);
    }
}

/// The `outcome` label, or `None` when nothing is counted.
fn outcome(pin: &Pin, selected: Option<&str>, over_cap: bool) -> Option<&'static str> {
    match pin {
        Pin::None => None,
        Pin::Unmatched => Some("unmatched"),
        Pin::Route(route) if selected == Some(route.as_str()) => {
            Some(if over_cap { "over_cap" } else { "hit" })
        }
        Pin::Route(_) => Some("ineligible"),
    }
}

/// The domain of one message ID, lowercased, or `None` if it has no `@`.
fn id_domain(id: &str) -> Option<String> {
    let inner = id.trim().trim_start_matches('<').trim_end_matches('>');
    let (_, domain) = inner.rsplit_once('@')?;
    let domain = domain.trim();
    (!domain.is_empty()).then(|| normalise_domain(domain))
}

fn normalise_domain(domain: &str) -> String {
    domain.trim_end_matches('.').to_ascii_lowercase()
}

/// The IDs in one header value, in the order written.
fn ids_in(value: &str) -> Vec<String> {
    let text = strip_comments(value);
    if text.contains('<') {
        let mut out = Vec::new();
        let mut rest = text.as_str();
        while let Some(open) = rest.find('<') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('>') else {
                break;
            };
            let inner = after[..close].trim();
            if inner.contains('@') {
                out.push(format!("<{inner}>"));
            }
            rest = &after[close + 1..];
        }
        out
    } else {
        text.split(|c: char| c.is_whitespace() || c == ',')
            .filter(|t| t.contains('@'))
            .map(|t| format!("<{t}>"))
            .collect()
    }
}

/// RFC 5322 comments removed: `(…)`, nested, with `\`-escapes, outside
/// quoted strings. An unterminated comment runs to the end of the value, which
/// is what a reader of the header would take it to mean.
fn strip_comments(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for c in value.chars() {
        if escaped {
            escaped = false;
            if depth == 0 {
                out.push(c);
            }
            continue;
        }
        match c {
            '\\' if depth > 0 || quoted => {
                escaped = true;
                if depth == 0 {
                    out.push(c);
                }
            }
            '"' if depth == 0 => {
                quoted = !quoted;
                out.push(c);
            }
            '(' if !quoted => depth += 1,
            ')' if !quoted && depth > 0 => depth -= 1,
            _ if depth > 0 => {}
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    const CFG: &str = r#"
server:
  listeners:
    - address: "127.0.0.1:25"
  hostname: simmer.test
  max_message_bytes: 1024
  max_recipients: 1
  max_concurrent_sessions: 4
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth: { allow_insecure_auth: true }
database: { url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }
admin: { listen: "127.0.0.1:8080", auth_token: "t" }
default_ramp: main
ramps:
 main:
  thread_affinity: true
  domain_groups:
  - { name: catchall, domains: ["*"] }
  senders:
  - { match: "oldbrand.com", match_on: envelope, chain: [warming, overflow] }
  - { match: "other.com", match_on: envelope, chain: [elsewhere] }
  default_chain: [overflow]
  routes:
  - name: warming
    downstream:
      host: warm.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity:
      envelope_from: "b@newbrand.com"
      set_headers: { Message-ID: "<{{uuid}}@NewBrand.com>" }
    warmup:
      started: "2026-01-01T00:00:00Z"
      schedule: { default: [10] }
  - name: overflow
    overflow: true
    downstream:
      host: over.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity:
      envelope_from: "b@mail.established.com"
      set_headers: { Message-ID: "<{{uuid}}@mail.established.com>" }
  - name: elsewhere
    overflow: true
    downstream:
      host: else.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity:
      envelope_from: "b@elsewhere.com"
      set_headers: { Message-ID: "<{{uuid}}@elsewhere.com>" }
"#;

    fn cfg() -> Config {
        crate::config::from_str(CFG, "test").expect("fixture is valid")
    }

    fn chain() -> Vec<String> {
        s(&["warming", "overflow"])
    }

    // -- parsing ---------------------------------------------------------

    #[test]
    fn in_reply_to_comes_first_then_references_newest_first() {
        let ids = referenced_ids(
            &s(&["<reply@gmail.com>"]),
            &s(&["<root@newbrand.com> <mid@x.com> <reply@gmail.com>"]),
        );
        assert_eq!(
            ids,
            s(&[
                "<reply@gmail.com>",
                "<reply@gmail.com>",
                "<mid@x.com>",
                "<root@newbrand.com>"
            ])
        );
    }

    #[test]
    fn a_folded_references_header_arrives_unfolded_and_parses() {
        // `HeaderBlock::get_all` unfolds; this is what a folded value looks like
        // by the time it gets here.
        let ids = referenced_ids(&[], &s(&["<a@x.com>\t<b@y.com>  <c@z.com>"]));
        assert_eq!(ids, s(&["<c@z.com>", "<b@y.com>", "<a@x.com>"]));
    }

    #[test]
    fn several_references_instances_are_read_last_instance_first() {
        let ids = referenced_ids(&[], &s(&["<a@x.com>", "<b@y.com>"]));
        assert_eq!(ids, s(&["<b@y.com>", "<a@x.com>"]));
    }

    #[test]
    fn comments_are_ignored_including_ones_that_look_like_ids() {
        let ids = referenced_ids(
            &[],
            &s(&["(see <fake@evil.com>) <a@x.com> (nested (<b@evil.com>)) <c@z.com>"]),
        );
        assert_eq!(ids, s(&["<c@z.com>", "<a@x.com>"]));
    }

    #[test]
    fn ids_without_brackets_are_accepted() {
        let ids = referenced_ids(&s(&["reply@gmail.com"]), &s(&["a@x.com, b@y.com"]));
        assert_eq!(ids, s(&["<reply@gmail.com>", "<b@y.com>", "<a@x.com>"]));
    }

    #[test]
    fn tokens_that_are_not_ids_are_dropped() {
        let ids = referenced_ids(&[], &s(&["<no-at-sign> <a@x.com> <unterminated@y.com"]));
        assert_eq!(ids, s(&["<a@x.com>"]));
    }

    #[test]
    fn at_most_max_ids_are_considered() {
        let long: String = (0..1000).map(|i| format!("<{i}@x.com> ")).collect();
        let ids = referenced_ids(&[], &[long]);
        assert_eq!(ids.len(), MAX_IDS);
        assert_eq!(ids[0], "<999@x.com>", "the newest are the ones kept");
    }

    #[test]
    fn route_domain_is_the_literal_after_the_last_at() {
        let cfg = cfg();
        assert_eq!(
            route_domain(&cfg.default_ramp().route("warming").unwrap().identity).as_deref(),
            Some("newbrand.com"),
            "lowercased"
        );
    }

    #[test]
    fn route_domain_refuses_a_templated_or_absent_domain() {
        let mut identity = cfg()
            .default_ramp()
            .route("warming")
            .unwrap()
            .identity
            .clone();
        identity.set_headers.0 = vec![(
            "Message-ID".into(),
            "<{{uuid}}@{{original.from.domain}}>".into(),
        )];
        assert_eq!(route_domain(&identity), None);

        identity.set_headers.0 = vec![("Message-ID".into(), "{{original.message_id}}".into())];
        assert_eq!(route_domain(&identity), None);

        identity.set_headers.0 = Vec::new();
        assert_eq!(route_domain(&identity), None);
    }

    #[test]
    fn route_domain_accepts_a_template_without_angle_brackets() {
        let mut identity = cfg()
            .default_ramp()
            .route("warming")
            .unwrap()
            .identity
            .clone();
        identity.set_headers.0 = vec![("message-id".into(), "{{uuid}}@newbrand.com".into())];
        assert_eq!(route_domain(&identity).as_deref(), Some("newbrand.com"));
    }

    // -- the pin ---------------------------------------------------------

    #[test]
    fn no_ids_is_no_pin() {
        assert_eq!(pin_for(cfg().default_ramp(), &chain(), &[]), Pin::None);
    }

    #[test]
    fn an_id_simmer_emitted_pins_its_route() {
        let ids = referenced_ids(
            &s(&["<CAx9@mail.gmail.com>"]),
            &s(&["<3f2a@mail.established.com> <CAx9@mail.gmail.com>"]),
        );
        assert_eq!(
            pin_for(cfg().default_ramp(), &chain(), &ids),
            Pin::Route("overflow".into())
        );
    }

    #[test]
    fn domains_match_case_insensitively_and_ignore_a_trailing_dot() {
        let ids = s(&["<3f2a@NEWBRAND.COM.>"]);
        assert_eq!(
            pin_for(cfg().default_ramp(), &chain(), &ids),
            Pin::Route("warming".into())
        );
    }

    #[test]
    fn the_most_recent_route_wins_when_a_thread_changed_identity() {
        // The first message went out warming, a later one via overflow (warming
        // was paused at the time). The recipient last saw overflow's identity.
        let ids = referenced_ids(
            &[],
            &s(&["<1@newbrand.com> <r1@gmail.com> <2@mail.established.com> <r2@gmail.com>"]),
        );
        assert_eq!(
            pin_for(cfg().default_ramp(), &chain(), &ids),
            Pin::Route("overflow".into())
        );
    }

    #[test]
    fn a_route_outside_the_chain_pins_nothing() {
        // `elsewhere` exists and its domain matches, but the sender rule did not
        // give this message that chain.
        let ids = s(&["<1@elsewhere.com>"]);
        assert_eq!(
            pin_for(cfg().default_ramp(), &chain(), &ids),
            Pin::Unmatched
        );
    }

    #[test]
    fn ids_from_other_domains_are_unmatched() {
        let ids = s(&["<CAx9@mail.gmail.com>"]);
        assert_eq!(
            pin_for(cfg().default_ramp(), &chain(), &ids),
            Pin::Unmatched
        );
    }

    #[test]
    fn a_subdomain_is_not_the_domain() {
        let ids = s(&["<1@x.newbrand.com>", "<2@established.com>"]);
        assert_eq!(
            pin_for(cfg().default_ramp(), &chain(), &ids),
            Pin::Unmatched
        );
    }

    #[test]
    fn nothing_is_parsed_when_affinity_is_off() {
        let mut cfg = cfg();
        cfg.default_ramp_mut().thread_affinity = false;
        let raw = b"From: a@oldbrand.com\r\nReferences: <1@newbrand.com>\r\n\r\nhi\r\n";
        assert_eq!(
            pin_for_message(cfg.default_ramp(), &chain(), raw),
            Pin::None
        );
    }

    #[test]
    fn a_message_is_read_for_its_threading_headers() {
        let raw = b"From: a@oldbrand.com\r\n\
            In-Reply-To: <CAx9@mail.gmail.com>\r\n\
            References: <3f2a@newbrand.com>\r\n <CAx9@mail.gmail.com>\r\n\
            \r\n\
            References: <body@mail.established.com>\r\n";
        assert_eq!(
            pin_for_message(cfg().default_ramp(), &chain(), raw),
            Pin::Route("warming".into()),
            "folded header read; the body is not headers"
        );
    }

    // -- the order -------------------------------------------------------

    #[test]
    fn the_pinned_route_goes_first_and_the_rest_keep_their_order() {
        let chain = s(&["a", "b", "c", "overflow"]);
        assert_eq!(
            order(&chain, &Pin::Route("c".into())),
            s(&["c", "a", "b", "overflow"])
        );
        assert_eq!(
            order(&chain, &Pin::Route("overflow".into())),
            s(&["overflow", "a", "b", "c"])
        );
    }

    #[test]
    fn no_pin_or_a_foreign_pin_leaves_the_chain_alone() {
        let chain = s(&["a", "b"]);
        assert_eq!(order(&chain, &Pin::None), chain);
        assert_eq!(order(&chain, &Pin::Unmatched), chain);
        assert_eq!(order(&chain, &Pin::Route("z".into())), chain);
    }

    #[test]
    fn outcomes_are_labelled_by_what_the_walk_did_with_the_pin() {
        let pin = Pin::Route("warming".into());
        assert_eq!(outcome(&pin, Some("warming"), false), Some("hit"));
        assert_eq!(outcome(&pin, Some("warming"), true), Some("over_cap"));
        assert_eq!(outcome(&pin, Some("overflow"), false), Some("ineligible"));
        assert_eq!(outcome(&pin, None, false), Some("ineligible"));
        assert_eq!(
            outcome(&Pin::Unmatched, Some("warming"), false),
            Some("unmatched")
        );
        assert_eq!(
            outcome(&Pin::None, Some("warming"), false),
            None,
            "a first message counts nothing"
        );
    }

    #[test]
    fn every_route_is_walked_exactly_once() {
        let chain = s(&["a", "b", "c"]);
        let mut walked = order(&chain, &Pin::Route("b".into()));
        walked.sort();
        assert_eq!(walked, chain);
    }
}
