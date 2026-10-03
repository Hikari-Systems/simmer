//! §5.8 — choose the ramp a message is routed in (D-099).
//!
//! Four rules, first match wins:
//!
//! 1. The listener has a `ramp` and `header_overrides_affinity` is false: that
//!    ramp.
//! 2. `X-Simmer-Ramp` gives a usable value the session may name: that ramp.
//! 3. The listener's `ramp`.
//! 4. `default_ramp`.
//!
//! Pure: no clock, no I/O, no metrics. The session, the early check and §9.4's
//! dry run all call [`select`], so the three cannot disagree, and the caller
//! records the metric and the `WARN` line from what it returns.
//!
//! **An unusable header is never a refusal.** It is ignored and selection falls
//! through to rule 3 or 4 as if it were absent. A `550` would be §14.1's hazard
//! for a typo, and a `451` would defer a message that has a perfectly good ramp
//! the operator chose.
//!
//! **Rights are the only thing authentication decides here** (§5.3 as amended).
//! A session may name exactly its user's `grants.ramps`; an unauthenticated one
//! may name none. Within a ramp, who authenticated still plays no part in
//! routing (D-071).

use crate::config::{Config, Ramp};

/// The longest header value that can be a ramp name (§4.2 caps names at 64).
/// Also the length a rejected value is truncated to in the `WARN` line.
pub const MAX_VALUE_CHARS: usize = 64;

/// The header a client names a ramp in.
pub const HEADER: &str = "X-Simmer-Ramp";

/// What the session arrived with: the listener's affinity and the rights the
/// authenticated user holds. Everything [`select`] needs apart from the header.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ingress<'a> {
    /// The listener's `ramp`, if it has one.
    pub affinity: Option<&'a str>,
    pub header_overrides_affinity: bool,
    /// The user's `grants.ramps`. Empty for an unauthenticated session.
    pub permitted: &'a [String],
}

/// Which rule chose the ramp — `simmer_ramp_selected_total{source}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Affinity,
    Header,
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Affinity => "affinity",
            Source::Header => "header",
            Source::Default => "default",
        }
    }
}

/// Why a header was ignored — `simmer_ramp_header_rejected_total{reason}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// Names no ramp.
    Unknown,
    /// Names a ramp the session may not name.
    NotPermitted,
    /// Empty, over 64 characters, not ASCII, or an RFC 2047 encoded-word.
    Malformed,
    /// Occurs more than once with different values.
    Conflicting,
    /// Names a ramp other than the listener's while its affinity wins.
    AffinityLocked,
}

impl Rejection {
    pub fn as_str(self) -> &'static str {
        match self {
            Rejection::Unknown => "unknown",
            Rejection::NotPermitted => "not_permitted",
            Rejection::Malformed => "malformed",
            Rejection::Conflicting => "conflicting",
            Rejection::AffinityLocked => "affinity_locked",
        }
    }
}

/// What became of the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderUse {
    /// There was none.
    Absent,
    /// It chose the ramp (rule 2).
    Used,
    /// It named the ramp the listener's affinity chose anyway. Neither used nor
    /// wrong, so not counted as a rejection.
    Redundant,
    /// Ignored. `value` is what was sent, truncated to [`MAX_VALUE_CHARS`], for
    /// the `WARN` line only — never a metric label, because the client chooses
    /// it (D-093).
    Rejected { reason: Rejection, value: String },
}

impl HeaderUse {
    pub fn as_str(&self) -> &'static str {
        match self {
            HeaderUse::Absent => "absent",
            HeaderUse::Used => "used",
            HeaderUse::Redundant => "redundant",
            HeaderUse::Rejected { reason, .. } => reason.as_str(),
        }
    }
}

/// A chosen ramp, the rule that chose it, and what became of the header.
#[derive(Debug, Clone)]
pub struct Selection<'a> {
    pub ramp: &'a Ramp,
    pub source: Source,
    pub header: HeaderUse,
}

impl Selection<'_> {
    /// The same choice with no borrow of the config (D-110).
    pub fn to_owned_selection(&self) -> OwnedSelection {
        OwnedSelection {
            ramp: self.ramp.name.clone(),
            source: self.source,
            header: self.header.clone(),
        }
    }
}

/// A [`Selection`] that names its ramp instead of borrowing it (D-110), so it
/// can outlive the config reference it was made from. It records a choice
/// already made at the final dot; [`OwnedSelection::resolve`] looks the ramp up
/// again and never re-selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSelection {
    pub ramp: String,
    pub source: Source,
    pub header: HeaderUse,
}

impl OwnedSelection {
    /// The borrowed form against `cfg`. `None` only if `cfg` has no ramp of this
    /// name — impossible while the config cannot change under a running process.
    pub fn resolve<'a>(&self, cfg: &'a Config) -> Option<Selection<'a>> {
        Some(Selection {
            ramp: cfg.ramps.get(&self.ramp)?,
            source: self.source,
            header: self.header.clone(),
        })
    }
}

/// §5.8. `header_values` is every `X-Simmer-Ramp` field in the message, unfolded,
/// in order; empty when there is none.
///
/// # Panics
///
/// Never on a validated configuration: §4.2 guarantees `default_ramp` and every
/// listener's `ramp` name a configured ramp.
pub fn select<'a>(
    cfg: &'a Config,
    ingress: &Ingress<'_>,
    header_values: &[String],
) -> Selection<'a> {
    let affinity = ingress.affinity.and_then(|name| cfg.ramps.get(name));
    let locked = affinity.is_some() && !ingress.header_overrides_affinity;

    let header = judge(
        cfg,
        ingress,
        header_values,
        locked.then_some(affinity).flatten(),
    );

    let (ramp, source) = match (&header, affinity) {
        // Rule 1.
        (_, Some(a)) if locked => (a, Source::Affinity),
        // Rule 2.
        (Judged::Usable(r), _) => (*r, Source::Header),
        // Rule 3.
        (_, Some(a)) => (a, Source::Affinity),
        // Rule 4.
        _ => (cfg.default_ramp(), Source::Default),
    };

    let header = match header {
        Judged::Absent => HeaderUse::Absent,
        Judged::Usable(_) if source == Source::Header => HeaderUse::Used,
        Judged::Usable(_) => HeaderUse::Redundant,
        Judged::Rejected(reason, value) => HeaderUse::Rejected { reason, value },
    };

    Selection {
        ramp,
        source,
        header,
    }
}

/// The session's inputs to [`select`]: its listener's affinity, and the
/// `grants.ramps` of the user it authenticated as. An unauthenticated session,
/// or a username no longer configured, may name nothing.
pub fn ingress<'a>(
    cfg: &'a Config,
    affinity: Option<&'a str>,
    header_overrides_affinity: bool,
    user: Option<&str>,
) -> Ingress<'a> {
    let permitted = user
        .and_then(|name| cfg.server.auth.users.iter().find(|u| u.username == name))
        .map(|u| u.grants.ramps.as_slice())
        .unwrap_or(&[]);
    Ingress {
        affinity,
        header_overrides_affinity,
        permitted,
    }
}

/// Count a selection, and `WARN` about an ignored header, as §5.8 asks. Kept
/// out of [`select`] so that §9.4's dry run can call it without counting.
pub fn record(selection: &Selection<'_>, correlation_id: &str) {
    crate::metrics::ramp_selected(&selection.ramp.name, selection.source.as_str());
    if let HeaderUse::Rejected { reason, value } = &selection.header {
        crate::metrics::ramp_header_rejected(reason.as_str());
        tracing::warn!(
            correlation_id,
            reason = reason.as_str(),
            // Truncated to 64 characters by `select`.
            value = %value,
            ramp = %selection.ramp.name,
            ramp_source = selection.source.as_str(),
            "X-Simmer-Ramp ignored; routed as if it were absent (§5.8)"
        );
    }
}

/// §5.4's early decision: the ramp, if it is already fixed at `RCPT TO`, before
/// the header has arrived. It is fixed when the listener's affinity wins
/// outright, or when nothing the session may name could move it off the ramp it
/// would otherwise get, which includes every unauthenticated session.
///
/// `None` means wait for the final dot: the early refusal is then not attempted,
/// so a header-selected ramp is never refused because a different ramp is
/// exhausted.
pub fn fixed_at_rcpt<'a>(cfg: &'a Config, ingress: &Ingress<'_>) -> Option<&'a Ramp> {
    let affinity = ingress.affinity.and_then(|name| cfg.ramps.get(name));
    if let Some(a) = affinity {
        if !ingress.header_overrides_affinity {
            return Some(a);
        }
    }
    let fallback = affinity.unwrap_or_else(|| cfg.default_ramp());
    ingress
        .permitted
        .iter()
        .all(|name| name == &fallback.name)
        .then_some(fallback)
}

enum Judged<'a> {
    Absent,
    Usable(&'a Ramp),
    Rejected(Rejection, String),
}

fn judge<'a>(
    cfg: &'a Config,
    ingress: &Ingress<'_>,
    header_values: &[String],
    locked_to: Option<&Ramp>,
) -> Judged<'a> {
    let Some(first) = header_values.first() else {
        return Judged::Absent;
    };
    let value = first.trim();
    let reject = |reason| Judged::Rejected(reason, truncate(value));

    // Identical repeats count once; a disagreement has no winner. "First"
    // depends on where each copy was added, and a second, unauthorised copy
    // could otherwise shadow or promote the first (D-099).
    if header_values.iter().any(|v| v.trim() != value) {
        return reject(Rejection::Conflicting);
    }
    if value.is_empty()
        || value.chars().count() > MAX_VALUE_CHARS
        || !value.is_ascii()
        || value.contains("=?")
    {
        return reject(Rejection::Malformed);
    }
    // Exact and case-sensitive, like the names themselves.
    let Some(ramp) = cfg.ramps.get(value) else {
        return reject(Rejection::Unknown);
    };
    if let Some(locked) = locked_to {
        return if ramp.name == locked.name {
            Judged::Usable(ramp)
        } else {
            reject(Rejection::AffinityLocked)
        };
    }
    if !ingress.permitted.iter().any(|p| p == value) {
        return reject(Rejection::NotPermitted);
    }
    Judged::Usable(ramp)
}

fn truncate(value: &str) -> String {
    value.chars().take(MAX_VALUE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three ramps. `main` is the default; `partner` is where the affinity
    /// listener points in most tests.
    fn config() -> Config {
        let route = |name: &str| {
            format!(
                "  - name: {name}\n    overflow: true\n    downstream: {{ host: o.example, port: 587, \
                 pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }} }}\n    \
                 identity: {{ envelope_from: \"b@established.com\" }}\n"
            )
        };
        let ramp = |name: &str| {
            format!(
                " {name}:\n  domain_groups:\n  - {{ name: catchall, domains: [\"*\"] }}\n  senders: []\n  \
                 default_chain: [overflow]\n  routes:\n{}",
                route("overflow")
            )
        };
        let yaml = format!(
            r#"
server:
  listeners:
    - address: "127.0.0.1:25"
  hostname: simmer.test
  max_message_bytes: 1024
  max_recipients: 10
  max_concurrent_sessions: 4
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: {{ command: 30s, data: 300s, session: 600s }}
  auth: {{ allow_insecure_auth: true }}
database: {{ url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }}
admin: {{ listen: "127.0.0.1:8080", auth_token: "t" }}
default_ramp: main
ramps:
{}{}{}"#,
            ramp("main"),
            ramp("partner"),
            ramp("brand-b")
        );
        crate::config::from_str(&yaml, "test").expect("fixture is valid")
    }

    fn h(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    fn grants(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn pick(cfg: &Config, ingress: Ingress<'_>, headers: &[&str]) -> (String, Source, String) {
        let s = select(cfg, &ingress, &h(headers));
        (s.ramp.name.clone(), s.source, s.header.as_str().to_string())
    }

    // -- D-110: the owned form -------------------------------------------

    #[test]
    fn an_owned_selection_resolves_to_the_same_choice_without_reselecting() {
        let cfg = config();
        let permitted = grants(&["partner"]);
        let ingress = Ingress {
            permitted: &permitted,
            ..Ingress::default()
        };
        let chosen = select(&cfg, &ingress, &h(&["partner"]));
        let owned = chosen.to_owned_selection();
        assert_eq!(owned.ramp, "partner");
        assert_eq!(owned.source, Source::Header);

        let back = owned.resolve(&cfg).expect("the ramp exists");
        assert!(
            std::ptr::eq(back.ramp, chosen.ramp),
            "the same configured ramp"
        );
        assert_eq!(back.source, chosen.source);
        assert_eq!(back.header, chosen.header);

        // A name the config does not have resolves to nothing, never to a
        // different ramp: the choice is recorded, not re-made.
        let missing = OwnedSelection {
            ramp: "gone".into(),
            ..owned
        };
        assert!(missing.resolve(&cfg).is_none());
    }

    // -- the four rules ---------------------------------------------------

    #[test]
    fn with_nothing_to_go_on_it_is_the_default_ramp() {
        let cfg = config();
        assert_eq!(
            pick(&cfg, Ingress::default(), &[]),
            ("main".into(), Source::Default, "absent".into())
        );
    }

    #[test]
    fn a_permitted_header_chooses_the_ramp() {
        let cfg = config();
        let permitted = grants(&["partner"]);
        let ingress = Ingress {
            permitted: &permitted,
            ..Ingress::default()
        };
        assert_eq!(
            pick(&cfg, ingress, &["partner"]),
            ("partner".into(), Source::Header, "used".into())
        );
    }

    #[test]
    fn a_listener_affinity_wins_over_a_permitted_header_by_default() {
        let cfg = config();
        let permitted = grants(&["brand-b"]);
        let ingress = Ingress {
            affinity: Some("partner"),
            permitted: &permitted,
            ..Ingress::default()
        };
        assert_eq!(
            pick(&cfg, ingress, &["brand-b"]),
            ("partner".into(), Source::Affinity, "affinity_locked".into())
        );
        // No header at all: the affinity, with nothing to report.
        assert_eq!(
            pick(&cfg, ingress, &[]),
            ("partner".into(), Source::Affinity, "absent".into())
        );
    }

    #[test]
    fn a_header_naming_the_locked_ramp_is_redundant_not_rejected() {
        let cfg = config();
        let ingress = Ingress {
            affinity: Some("partner"),
            ..Ingress::default()
        };
        assert_eq!(
            pick(&cfg, ingress, &["partner"]),
            ("partner".into(), Source::Affinity, "redundant".into())
        );
    }

    #[test]
    fn with_the_override_a_permitted_header_beats_the_affinity() {
        let cfg = config();
        let permitted = grants(&["brand-b"]);
        let ingress = Ingress {
            affinity: Some("partner"),
            header_overrides_affinity: true,
            permitted: &permitted,
        };
        assert_eq!(
            pick(&cfg, ingress, &["brand-b"]),
            ("brand-b".into(), Source::Header, "used".into())
        );
        // An unusable one falls back to the affinity (rule 3), not the default.
        assert_eq!(
            pick(&cfg, ingress, &["nonesuch"]),
            ("partner".into(), Source::Affinity, "unknown".into())
        );
    }

    // -- rights -------------------------------------------------------------

    #[test]
    fn an_unauthenticated_session_may_name_nothing() {
        let cfg = config();
        assert_eq!(
            pick(&cfg, Ingress::default(), &["partner"]),
            ("main".into(), Source::Default, "not_permitted".into())
        );
    }

    #[test]
    fn a_user_may_name_only_its_own_grants() {
        let cfg = config();
        let permitted = grants(&["partner"]);
        let ingress = Ingress {
            permitted: &permitted,
            ..Ingress::default()
        };
        assert_eq!(
            pick(&cfg, ingress, &["brand-b"]),
            ("main".into(), Source::Default, "not_permitted".into())
        );
    }

    // -- what makes a value unusable ----------------------------------------

    #[test]
    fn values_are_trimmed_and_compared_exactly() {
        let cfg = config();
        let permitted = grants(&["partner"]);
        let ingress = Ingress {
            permitted: &permitted,
            ..Ingress::default()
        };
        assert_eq!(pick(&cfg, ingress, &["  partner \t"]).1, Source::Header);
        assert_eq!(
            pick(&cfg, ingress, &["Partner"]).2,
            "unknown",
            "case-sensitive, like the names"
        );
    }

    #[test]
    fn malformed_values_are_ignored() {
        let cfg = config();
        let permitted = grants(&["partner"]);
        let ingress = Ingress {
            permitted: &permitted,
            ..Ingress::default()
        };
        let long = "p".repeat(65);
        for bad in ["", "   ", long.as_str(), "pärtner", "=?utf-8?q?partner?="] {
            assert_eq!(
                pick(&cfg, ingress, &[bad]),
                ("main".into(), Source::Default, "malformed".into()),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn disagreeing_repeats_have_no_winner_and_identical_ones_count_once() {
        let cfg = config();
        let permitted = grants(&["partner", "brand-b"]);
        let ingress = Ingress {
            permitted: &permitted,
            ..Ingress::default()
        };
        assert_eq!(
            pick(&cfg, ingress, &["partner", "brand-b"]),
            ("main".into(), Source::Default, "conflicting".into())
        );
        assert_eq!(
            pick(&cfg, ingress, &["partner", " partner"]),
            ("partner".into(), Source::Header, "used".into())
        );
    }

    #[test]
    fn a_rejected_value_is_truncated_for_the_log() {
        let cfg = config();
        let long = "x".repeat(200);
        let s = select(&cfg, &Ingress::default(), &h(&[&long]));
        match s.header {
            HeaderUse::Rejected { value, .. } => assert_eq!(value.len(), MAX_VALUE_CHARS),
            other => panic!("{other:?}"),
        }
    }

    // -- §5.4's early decision ---------------------------------------------

    #[test]
    fn the_ramp_is_fixed_at_rcpt_when_nothing_could_move_it() {
        let cfg = config();
        // Unauthenticated: the default, and nothing can change it.
        assert_eq!(
            fixed_at_rcpt(&cfg, &Ingress::default()).map(|r| r.name.as_str()),
            Some("main")
        );
        // A locked affinity.
        let permitted = grants(&["brand-b"]);
        let locked = Ingress {
            affinity: Some("partner"),
            permitted: &permitted,
            ..Ingress::default()
        };
        assert_eq!(
            fixed_at_rcpt(&cfg, &locked).map(|r| r.name.as_str()),
            Some("partner")
        );
        // A grant only for the ramp it would get anyway.
        let same = grants(&["main"]);
        let redundant = Ingress {
            permitted: &same,
            ..Ingress::default()
        };
        assert_eq!(
            fixed_at_rcpt(&cfg, &redundant).map(|r| r.name.as_str()),
            Some("main")
        );
    }

    #[test]
    fn the_ramp_waits_for_the_final_dot_when_a_header_could_move_it() {
        let cfg = config();
        let permitted = grants(&["partner"]);
        let ingress = Ingress {
            permitted: &permitted,
            ..Ingress::default()
        };
        assert!(fixed_at_rcpt(&cfg, &ingress).is_none());
        let overridable = Ingress {
            affinity: Some("partner"),
            header_overrides_affinity: true,
            permitted: &grants(&["brand-b"]),
        };
        assert!(fixed_at_rcpt(&cfg, &overridable).is_none());
    }
}
