//! `SPEC.md` §5.4 sender matching.
//!
//! | Form | Example | Matches |
//! |---|---|---|
//! | Exact domain | `oldbrand.com` | `anyone@oldbrand.com` |
//! | Subdomain wildcard | `*.oldbrand.com` | `x@mail.oldbrand.com`; **not** `x@oldbrand.com` |
//! | Full address | `marketing@newbrand.com` | that address only |
//!
//! Matching is case-insensitive, rules are evaluated in configuration order, and
//! first match wins. There is deliberately **no specificity ranking** — a
//! full-address rule does not beat a domain rule by being more specific, it wins
//! only by being written first. §5.4 says as much ("Full-address rules should
//! therefore be placed above domain rules where both could match"), and inferring
//! precedence instead would mean the order in the file stopped predicting
//! behaviour.
//!
//! This module is pure and lands in phase 1 rather than phase 3 because wildcard
//! precedence is the single easiest thing here to get subtly wrong, and it is
//! cheap to pin.

use crate::config::{MatchOn, Ramp, SenderRule};

/// One parsed `match` pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// `oldbrand.com` — the domain itself, not its subdomains.
    Domain(String),
    /// `*.oldbrand.com` — strict subdomains only.
    Subdomain(String),
    /// `marketing@newbrand.com` — that address alone.
    Address(String),
}

impl Pattern {
    /// Classify a `match` string. Normalises to lowercase once, here, so the hot
    /// path never lowercases the pattern.
    pub fn parse(raw: &str) -> Pattern {
        let s = raw.trim().to_ascii_lowercase();
        if let Some(rest) = s.strip_prefix("*.") {
            Pattern::Subdomain(rest.to_string())
        } else if s.contains('@') {
            Pattern::Address(s)
        } else {
            Pattern::Domain(s)
        }
    }

    /// Does this pattern match `address`?
    ///
    /// `address` is a bare `local@domain` — display names and angle brackets are
    /// stripped by the caller, since §5.4 tests "the domain (or address)".
    pub fn matches(&self, address: &str) -> bool {
        let address = address.trim().to_ascii_lowercase();
        let Some(domain) = domain_of(&address) else {
            // No `@`, so not an address we can reason about.
            return false;
        };

        match self {
            Pattern::Address(want) => address == *want,
            Pattern::Domain(want) => domain == *want,
            Pattern::Subdomain(parent) => is_strict_subdomain(domain, parent),
        }
    }
}

/// The domain part of `local@domain`, lowercase in, lowercase out.
///
/// Uses the **last** `@`, per RFC 5321: a quoted local part may legally contain
/// one, and `"a@b"@example.com` has domain `example.com`.
fn domain_of(address: &str) -> Option<&str> {
    let at = address.rfind('@')?;
    let domain = &address[at + 1..];
    if domain.is_empty() || at == 0 {
        None
    } else {
        Some(domain)
    }
}

/// `mail.oldbrand.com` is a strict subdomain of `oldbrand.com`; `oldbrand.com`
/// is not a subdomain of itself, and `notoldbrand.com` is not one either.
fn is_strict_subdomain(domain: &str, parent: &str) -> bool {
    if parent.is_empty() {
        return false;
    }
    match domain.len().checked_sub(parent.len()) {
        // Must be strictly longer, and the extra must end in a label separator,
        // or `eviloldbrand.com` would match `*.oldbrand.com`.
        Some(prefix_len) if prefix_len >= 2 => {
            domain.as_bytes()[prefix_len - 1] == b'.' && &domain[prefix_len..] == parent
        }
        _ => false,
    }
}

/// The senders Simmer resolved for one message.
#[derive(Debug, Clone, Default)]
pub struct Senders {
    /// From `MAIL FROM`. `None` for a null sender (`MAIL FROM:<>`).
    pub envelope: Option<String>,
    /// First address of the `From:` header. `None` when absent or unparseable.
    pub from_header: Option<String>,
}

impl Senders {
    pub fn new(envelope: Option<&str>, from_header: Option<&str>) -> Self {
        Self {
            envelope: envelope.map(str::to_string),
            from_header: from_header.map(str::to_string),
        }
    }

    /// Which sender(s) a rule with this `match_on` should test.
    fn candidates(&self, match_on: MatchOn) -> [Option<&String>; 2] {
        match match_on {
            MatchOn::Envelope => [self.envelope.as_ref(), None],
            MatchOn::FromHeader => [self.from_header.as_ref(), None],
            MatchOn::Either => [self.envelope.as_ref(), self.from_header.as_ref()],
        }
    }

    /// §5.4: "If the envelope and header senders disagree, log at WARN with both
    /// values and increment simmer_sender_mismatch_total."
    ///
    /// Compared on the full address, case-insensitively. A null envelope sender
    /// is not a disagreement — it is a bounce-ish message with nothing to compare.
    pub fn disagree(&self) -> bool {
        match (&self.envelope, &self.from_header) {
            (Some(e), Some(f)) => !e.eq_ignore_ascii_case(f),
            _ => false,
        }
    }
}

/// The outcome of §3.2 step 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match<'a> {
    /// Index into the ramp's `senders`, plus the rule itself.
    Rule { index: usize, rule: &'a SenderRule },
    /// No rule matched. §3.2 step 1: use the default chain, or reject when
    /// `strict_senders`.
    Unmatched,
}

/// First matching rule in configuration order wins (§5.4).
pub fn match_sender<'a>(ramp: &'a Ramp, senders: &Senders) -> Match<'a> {
    for (index, rule) in ramp.senders.iter().enumerate() {
        let pattern = Pattern::parse(&rule.pattern);
        let matched = senders
            .candidates(rule.match_on)
            .into_iter()
            .flatten()
            .any(|addr| pattern.matches(addr));
        if matched {
            return Match::Rule { index, rule };
        }
    }
    Match::Unmatched
}

/// Whether the routing decision can be made at `RCPT TO` rather than at the
/// final dot.
///
/// §5.4: "when any applicable rule uses `from_header` or `either`, the routing
/// decision cannot be made until the message body has been received... When all
/// rules use `envelope`, Simmer should decide early and reject at `RCPT TO` to
/// avoid a wasted body transfer."
///
/// Never with `thread_affinity` on (D-090). A pinned reply may take its route
/// past the day's cap, and whether a message *is* a pinned reply is in its
/// headers, which have not arrived at `RCPT TO`. Deciding early would refuse
/// exactly the reply the pin exists to let through, whenever the chain's
/// ordinary walk is spent.
pub fn can_decide_at_rcpt(ramp: &Ramp) -> bool {
    !ramp.thread_affinity
        && ramp
            .senders
            .iter()
            .all(|r| matches!(r.match_on, MatchOn::Envelope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    // -- Pattern classification ------------------------------------------

    #[test]
    fn classifies_the_three_forms() {
        assert_eq!(
            Pattern::parse("oldbrand.com"),
            Pattern::Domain("oldbrand.com".into())
        );
        assert_eq!(
            Pattern::parse("*.oldbrand.com"),
            Pattern::Subdomain("oldbrand.com".into())
        );
        assert_eq!(
            Pattern::parse("marketing@newbrand.com"),
            Pattern::Address("marketing@newbrand.com".into())
        );
    }

    #[test]
    fn classification_is_case_insensitive() {
        assert_eq!(
            Pattern::parse("OldBrand.COM"),
            Pattern::Domain("oldbrand.com".into())
        );
        assert_eq!(
            Pattern::parse("*.OldBrand.COM"),
            Pattern::Subdomain("oldbrand.com".into())
        );
    }

    // -- Exact domain ----------------------------------------------------

    #[test]
    fn exact_domain_matches_any_local_part() {
        let p = Pattern::parse("oldbrand.com");
        assert!(p.matches("anyone@oldbrand.com"));
        assert!(p.matches("marketing@oldbrand.com"));
        assert!(p.matches("a@oldbrand.com"));
    }

    #[test]
    fn exact_domain_does_not_match_subdomains() {
        // The complement of the wildcard rule; both directions matter.
        let p = Pattern::parse("oldbrand.com");
        assert!(!p.matches("x@mail.oldbrand.com"));
        assert!(!p.matches("x@a.b.oldbrand.com"));
    }

    #[test]
    fn exact_domain_does_not_match_a_suffix_lookalike() {
        let p = Pattern::parse("oldbrand.com");
        assert!(!p.matches("x@notoldbrand.com"));
        assert!(!p.matches("x@oldbrand.com.evil.net"));
    }

    // -- Subdomain wildcard ----------------------------------------------

    #[test]
    fn wildcard_matches_a_subdomain() {
        let p = Pattern::parse("*.oldbrand.com");
        assert!(p.matches("x@mail.oldbrand.com"));
    }

    #[test]
    fn wildcard_matches_a_deep_subdomain() {
        let p = Pattern::parse("*.oldbrand.com");
        assert!(p.matches("x@a.b.oldbrand.com"));
    }

    #[test]
    fn wildcard_does_not_match_the_bare_domain() {
        // Called out explicitly in the §5.4 table: `*.oldbrand.com` matches
        // `x@mail.oldbrand.com`, **not** `x@oldbrand.com`.
        let p = Pattern::parse("*.oldbrand.com");
        assert!(!p.matches("x@oldbrand.com"));
    }

    #[test]
    fn wildcard_does_not_match_a_label_suffix_lookalike() {
        // The bug this test exists for: naive `ends_with("oldbrand.com")` would
        // match `eviloldbrand.com` and hand an attacker the warming identity.
        let p = Pattern::parse("*.oldbrand.com");
        assert!(!p.matches("x@eviloldbrand.com"));
        assert!(!p.matches("x@notmail.oldbrand.com.evil.net"));
    }

    #[test]
    fn wildcard_does_not_match_an_empty_label() {
        // ".oldbrand.com" is not a real domain; it must not slip through as a
        // zero-length subdomain label.
        let p = Pattern::parse("*.oldbrand.com");
        assert!(!p.matches("x@.oldbrand.com"));
    }

    // -- Full address ----------------------------------------------------

    #[test]
    fn full_address_matches_only_itself() {
        let p = Pattern::parse("marketing@newbrand.com");
        assert!(p.matches("marketing@newbrand.com"));
        assert!(!p.matches("sales@newbrand.com"));
        assert!(!p.matches("marketing@other.com"));
    }

    #[test]
    fn matching_is_case_insensitive_on_both_sides() {
        assert!(Pattern::parse("OldBrand.com").matches("Anyone@OLDBRAND.COM"));
        assert!(Pattern::parse("Marketing@NewBrand.com").matches("MARKETING@newbrand.com"));
        assert!(Pattern::parse("*.OLDBRAND.com").matches("x@Mail.OldBrand.COM"));
    }

    #[test]
    fn a_quoted_local_part_containing_an_at_uses_the_last_one() {
        assert!(Pattern::parse("example.com").matches(r#""a@b"@example.com"#));
    }

    #[test]
    fn rejects_malformed_addresses() {
        let p = Pattern::parse("oldbrand.com");
        assert!(!p.matches("no-at-sign"));
        assert!(!p.matches("trailing@"));
        assert!(!p.matches("@leading.com"));
        assert!(!p.matches(""));
    }

    // -- Rule ordering ---------------------------------------------------

    fn config_with(rules: &[(&str, MatchOn, &str)]) -> Config {
        // Build the smallest config the type will accept, then swap in senders.
        let yaml = r#"
server:
  listeners:
    - address: "127.0.0.1:25"
  hostname: simmer.test
  max_message_bytes: 1024
  max_recipients: 10
  max_concurrent_sessions: 4
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth: { allow_insecure_auth: true }
database:
  url: "postgres://u:p@localhost/simmer"
  connect_timeout: 5s
admin:
  listen: "127.0.0.1:8080"
  auth_token: "t"
default_ramp: main
ramps:
 main:
  domain_groups:
  - { name: catchall, domains: ["*"] }
  senders: []
  default_chain: [overflow]
  routes:
  - name: overflow
    overflow: true
    downstream:
      host: d.example.com
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity:
      envelope_from: "b@example.com"
"#;
        let mut cfg: Config = serde_yaml_ng::from_str(yaml).expect("fixture parses");
        cfg.default_ramp_mut().senders = rules
            .iter()
            .map(|(pattern, match_on, chain)| SenderRule {
                pattern: (*pattern).to_string(),
                match_on: *match_on,
                chain: vec![(*chain).to_string()],
            })
            .collect();
        cfg
    }

    fn matched_chain<'a>(m: &Match<'a>) -> Option<&'a str> {
        match m {
            Match::Rule { rule, .. } => Some(rule.chain[0].as_str()),
            Match::Unmatched => None,
        }
    }

    #[test]
    fn first_matching_rule_wins_not_the_most_specific() {
        // §5.4: rules are evaluated in configuration order. Here the broad domain
        // rule is written first, so it beats the full-address rule below it —
        // which is exactly why the spec tells operators to put addresses on top.
        let cfg = config_with(&[
            ("newbrand.com", MatchOn::Envelope, "by-domain"),
            ("marketing@newbrand.com", MatchOn::Envelope, "by-address"),
        ]);
        let m = match_sender(
            cfg.default_ramp(),
            &Senders::new(Some("marketing@newbrand.com"), None),
        );
        assert_eq!(matched_chain(&m), Some("by-domain"));
    }

    #[test]
    fn address_rule_placed_first_wins() {
        let cfg = config_with(&[
            ("marketing@newbrand.com", MatchOn::Envelope, "by-address"),
            ("newbrand.com", MatchOn::Envelope, "by-domain"),
        ]);
        let m = match_sender(
            cfg.default_ramp(),
            &Senders::new(Some("marketing@newbrand.com"), None),
        );
        assert_eq!(matched_chain(&m), Some("by-address"));

        // ...and a different local part still falls to the domain rule.
        let m = match_sender(
            cfg.default_ramp(),
            &Senders::new(Some("sales@newbrand.com"), None),
        );
        assert_eq!(matched_chain(&m), Some("by-domain"));
    }

    #[test]
    fn unmatched_when_no_rule_applies() {
        let cfg = config_with(&[("oldbrand.com", MatchOn::Envelope, "c")]);
        let m = match_sender(
            cfg.default_ramp(),
            &Senders::new(Some("x@elsewhere.com"), None),
        );
        assert_eq!(m, Match::Unmatched);
    }

    // -- match_on --------------------------------------------------------

    #[test]
    fn match_on_envelope_ignores_the_from_header() {
        let cfg = config_with(&[("oldbrand.com", MatchOn::Envelope, "c")]);
        let m = match_sender(
            cfg.default_ramp(),
            &Senders::new(Some("x@other.com"), Some("y@oldbrand.com")),
        );
        assert_eq!(m, Match::Unmatched);
    }

    #[test]
    fn match_on_from_header_ignores_the_envelope() {
        let cfg = config_with(&[("oldbrand.com", MatchOn::FromHeader, "c")]);
        let m = match_sender(
            cfg.default_ramp(),
            &Senders::new(Some("x@oldbrand.com"), Some("y@other.com")),
        );
        assert_eq!(m, Match::Unmatched);
    }

    #[test]
    fn match_on_either_matches_from_either_side() {
        let cfg = config_with(&[("oldbrand.com", MatchOn::Either, "c")]);
        assert!(matches!(
            match_sender(
                cfg.default_ramp(),
                &Senders::new(Some("x@oldbrand.com"), Some("y@other.com"))
            ),
            Match::Rule { .. }
        ));
        assert!(matches!(
            match_sender(
                cfg.default_ramp(),
                &Senders::new(Some("x@other.com"), Some("y@oldbrand.com"))
            ),
            Match::Rule { .. }
        ));
        assert_eq!(
            match_sender(
                cfg.default_ramp(),
                &Senders::new(Some("x@a.com"), Some("y@b.com"))
            ),
            Match::Unmatched
        );
    }

    #[test]
    fn a_missing_sender_simply_does_not_match() {
        // MAIL FROM:<> is legal; it must not panic or match a domain rule.
        let cfg = config_with(&[("oldbrand.com", MatchOn::Envelope, "c")]);
        assert_eq!(
            match_sender(
                cfg.default_ramp(),
                &Senders::new(None, Some("y@oldbrand.com"))
            ),
            Match::Unmatched
        );
    }

    // -- mismatch detection and early decision ---------------------------

    #[test]
    fn detects_envelope_header_disagreement() {
        assert!(Senders::new(Some("a@x.com"), Some("b@y.com")).disagree());
        assert!(!Senders::new(Some("a@x.com"), Some("A@X.com")).disagree());
        assert!(!Senders::new(None, Some("b@y.com")).disagree());
        assert!(!Senders::new(Some("a@x.com"), None).disagree());
    }

    #[test]
    fn early_decision_only_when_every_rule_is_envelope_only() {
        assert!(can_decide_at_rcpt(
            config_with(&[
                ("a.com", MatchOn::Envelope, "c"),
                ("b.com", MatchOn::Envelope, "c"),
            ])
            .default_ramp()
        ));
        assert!(!can_decide_at_rcpt(
            config_with(&[
                ("a.com", MatchOn::Envelope, "c"),
                ("b.com", MatchOn::FromHeader, "c"),
            ])
            .default_ramp()
        ));
        assert!(!can_decide_at_rcpt(
            config_with(&[("a.com", MatchOn::Either, "c")]).default_ramp()
        ));
    }

    #[test]
    fn the_spec_example_config_behaves_as_documented() {
        // The four rules from §4.1, in the order they are written there.
        let cfg = config_with(&[
            ("oldbrand.com", MatchOn::FromHeader, "warming"),
            ("*.oldbrand.com", MatchOn::FromHeader, "warming"),
            ("marketing@newbrand.com", MatchOn::FromHeader, "warming"),
            ("newbrand.com", MatchOn::FromHeader, "warming"),
        ]);

        let hits = |addr: &str| match_sender(cfg.default_ramp(), &Senders::new(None, Some(addr)));

        // Both the old brand and its subdomains are covered, and the app-updated
        // arrangement (§1.1) sending as newbrand.com matches too.
        assert!(matches!(
            hits("a@oldbrand.com"),
            Match::Rule { index: 0, .. }
        ));
        assert!(matches!(
            hits("a@mail.oldbrand.com"),
            Match::Rule { index: 1, .. }
        ));
        assert!(matches!(
            hits("marketing@newbrand.com"),
            Match::Rule { index: 2, .. }
        ));
        assert!(matches!(
            hits("sales@newbrand.com"),
            Match::Rule { index: 3, .. }
        ));
        assert_eq!(hits("someone@unrelated.com"), Match::Unmatched);
    }
}
