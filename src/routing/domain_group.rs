//! §3.2 step 2 — resolve a recipient to a domain group.
//!
//! "Exact, case-insensitive match of the recipient domain against each group's
//! domain list; fall back to the catch-all group."
//!
//! D-100 adds one step between the two: a domain no group lists literally is
//! looked up in DNS, and joins the first group (in configuration order) one of
//! whose `mx` suffixes matches its lowest-preference MX host. That is what puts
//! a Google Workspace company domain in `google` rather than the catch-all —
//! the gap §14.3 named, and the one ramp #2 hit: Google refused Workspace
//! domains with the same reputation error it gave gmail.com, and holding the
//! `google` group at 0 could not stop them.
//!
//! Three properties the rest of Simmer relies on, kept deliberately:
//!
//! - **A literal match never touches DNS.** gmail.com resolves exactly as it
//!   did before D-100, and a ramp with no `mx` lists does no lookups at all.
//! - **DNS never defers mail.** A lookup that fails or times out lands in the
//!   catch-all, as the recipient would have before D-100. It is counted
//!   (`simmer_mx_lookups_total{result="error"|"timeout"}`) rather than retried
//!   on the message's time.
//! - **Steering by MX can only tighten.** Anyone can point a domain's MX at
//!   Google; doing so only puts that domain under the `google` group's
//!   allowance. It cannot move a Google-hosted domain out of it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{DomainGroup, Ramp};
use crate::preflight::resolver::MxResolver;

/// How long one MX lookup may hold a message up. A lookup that has not
/// answered by then is a `timeout`, and the recipient goes to the catch-all.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// A positive answer is kept for its TTL, clamped to this range: the floor so
/// a 60-second TTL does not become a lookup per message, the ceiling so a
/// migration to or from Workspace is noticed within a day.
const MIN_TTL: Duration = Duration::from_secs(300);
const MAX_TTL: Duration = Duration::from_secs(86_400);
/// "Resolved, no MX" (and NXDOMAIN).
const NEGATIVE_TTL: Duration = Duration::from_secs(3_600);
/// A failed or timed-out lookup. Short, so recovery is quick, but long enough
/// that a DNS outage does not add `LOOKUP_TIMEOUT` to every message.
const FAILURE_TTL: Duration = Duration::from_secs(60);
/// A bound on the cache. Reaching it drops the expired entries, then, if that
/// was not enough, all of them: a cold cache costs lookups, not correctness.
const MAX_ENTRIES: usize = 50_000;

/// Why a recipient is in the group it is in. §9.4's dry run reports it, so an
/// operator can see why a company domain was counted as `google`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Basis {
    /// The recipient domain is in the group's `domains` list.
    Literal,
    /// The domain's lowest-preference MX host (the one named) matched one of
    /// the group's `mx` suffixes (D-100).
    Mx(String),
    /// No group claims the domain, literally or by MX.
    CatchAll,
    /// Some group has `mx` suffixes but the domain's MX could not be found out
    /// — a failed or timed-out lookup, or no resolver. The catch-all, as
    /// before D-100.
    MxUnavailable,
}

impl Basis {
    pub fn describe(&self) -> String {
        match self {
            Basis::Literal => "literal".to_string(),
            Basis::Mx(host) => format!("mx:{host}"),
            Basis::CatchAll => "fallback".to_string(),
            Basis::MxUnavailable => "fallback:mx-unavailable".to_string(),
        }
    }
}

/// A recipient's group, and why.
#[derive(Debug, Clone)]
pub struct Resolved<'a> {
    pub group: &'a DomainGroup,
    pub basis: Basis,
}

/// §3.2 step 2 with D-100's MX step. One per process, shared by every session,
/// the dry run and the early check, so all three see the same cache.
pub struct Grouper {
    resolver: Option<Arc<dyn MxResolver>>,
    timeout: Duration,
    cache: Mutex<HashMap<String, Cached>>,
}

#[derive(Clone)]
struct Cached {
    /// The lowest-preference exchange hosts; `None` if the lookup failed.
    hosts: Option<Vec<String>>,
    expires: Instant,
}

impl Grouper {
    /// No resolver: literal matching and the catch-all, exactly §3.2 step 2 as
    /// it was before D-100. A ramp with `mx` lists resolves every domain it
    /// does not list literally to the catch-all, as `MxUnavailable`.
    pub fn literal() -> Self {
        Self {
            resolver: None,
            timeout: LOOKUP_TIMEOUT,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn new(resolver: Arc<dyn MxResolver>) -> Self {
        Self {
            resolver: Some(resolver),
            ..Self::literal()
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The group `recipient` belongs to in `ramp`, and why.
    ///
    /// `None` only for a configuration with no catch-all, which §4.2 refuses.
    pub async fn resolve<'a>(&self, ramp: &'a Ramp, recipient: &str) -> Option<Resolved<'a>> {
        if let Some(group) = literal(ramp, recipient) {
            return Some(Resolved {
                group,
                basis: Basis::Literal,
            });
        }
        let catchall = ramp.catchall_group()?;
        let fallback = |basis| {
            Some(Resolved {
                group: catchall,
                basis,
            })
        };

        let Some(domain) = domain_of(recipient) else {
            return fallback(Basis::CatchAll);
        };
        if !ramp.domain_groups.iter().any(|g| !g.mx.is_empty()) {
            return fallback(Basis::CatchAll);
        }
        // An address literal (`bob@[192.0.2.1]`) has no MX to look up.
        if domain.starts_with('[') {
            return fallback(Basis::CatchAll);
        }
        let Some(hosts) = self.lowest_mx_hosts(domain).await else {
            return fallback(Basis::MxUnavailable);
        };

        // Configuration order decides, as it does for literal domains; §4.2
        // already refuses a suffix listed in two groups.
        for group in &ramp.domain_groups {
            if let Some(host) = hosts.iter().find(|h| group.matches_mx_host(h)) {
                return Some(Resolved {
                    group,
                    basis: Basis::Mx(host.clone()),
                });
            }
        }
        fallback(Basis::CatchAll)
    }

    /// The group's name, for the walk: [`resolve`](Self::resolve) with the
    /// catch-all's conventional name as the last resort.
    pub async fn group_name(&self, ramp: &Ramp, recipient: &str) -> String {
        self.resolve(ramp, recipient)
            .await
            .map(|r| r.group.name.clone())
            .unwrap_or_else(|| "catchall".to_string())
    }

    /// The exchange hosts at the lowest preference `domain` has — the ones a
    /// sender actually talks to. `Some(vec![])` for no MX (or a null MX,
    /// RFC 7505); `None` if the answer could not be found out.
    async fn lowest_mx_hosts(&self, domain: &str) -> Option<Vec<String>> {
        let key = domain.trim_end_matches('.').to_ascii_lowercase();
        let now = Instant::now();

        if let Some(hit) = self.cache_get(&key, now) {
            crate::metrics::mx_lookup("cached");
            return hit.hosts;
        }

        let resolver = self.resolver.as_ref()?;
        let (hosts, ttl, result) = match tokio::time::timeout(self.timeout, resolver.mx(&key)).await
        {
            Ok(Ok(answer)) => {
                let lowest = answer.records.iter().map(|r| r.preference).min();
                let hosts: Vec<String> = answer
                    .records
                    .iter()
                    .filter(|r| Some(r.preference) == lowest && !r.exchange.is_empty())
                    .map(|r| r.exchange.clone())
                    .collect();
                let ttl = if hosts.is_empty() {
                    NEGATIVE_TTL
                } else {
                    Duration::from_secs(u64::from(answer.ttl_secs)).clamp(MIN_TTL, MAX_TTL)
                };
                (Some(hosts), ttl, "ok")
            }
            Ok(Err(e)) => {
                tracing::warn!(domain = %key, error = %e, "MX lookup failed; recipient counted in the catch-all");
                (None, FAILURE_TTL, "error")
            }
            Err(_) => {
                tracing::warn!(domain = %key, timeout_ms = self.timeout.as_millis() as u64, "MX lookup timed out; recipient counted in the catch-all");
                (None, FAILURE_TTL, "timeout")
            }
        };
        crate::metrics::mx_lookup(result);

        self.cache_put(
            key,
            Cached {
                hosts: hosts.clone(),
                expires: now + ttl,
            },
            now,
        );
        hosts
    }

    fn cache_get(&self, key: &str, now: Instant) -> Option<Cached> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.get(key).filter(|c| c.expires > now).cloned()
    }

    fn cache_put(&self, key: String, entry: Cached, now: Instant) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= MAX_ENTRIES && !cache.contains_key(&key) {
            cache.retain(|_, c| c.expires > now);
            if cache.len() >= MAX_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(key, entry);
    }
}

/// §3.2 step 2's literal match: the group whose `domains` list contains the
/// recipient's domain, if any. No DNS, and no catch-all.
pub fn literal<'a>(ramp: &'a Ramp, recipient: &str) -> Option<&'a DomainGroup> {
    let d = domain_of(recipient)?;
    ramp.domain_groups.iter().find(|group| {
        group
            .domains
            .iter()
            .any(|configured| configured != "*" && configured.eq_ignore_ascii_case(d))
    })
}

/// The domain part of `local@domain`, lowercased by the caller's comparison.
///
/// Uses the **last** `@`: a quoted local part may legally contain one, and
/// `"a@b"@example.com` has domain `example.com`. Same rule as §5.4's sender
/// matching, deliberately.
fn domain_of(address: &str) -> Option<&str> {
    let at = address.rfind('@')?;
    let domain = &address[at + 1..];
    (!domain.is_empty() && at > 0).then_some(domain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const CFG: &str = r#"
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
database: { url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }
admin: { listen: "127.0.0.1:8080", auth_token: "t" }
default_ramp: main
ramps:
 main:
  domain_groups:
  - { name: google, domains: ["gmail.com", "googlemail.com"] }
  - { name: microsoft, domains: ["outlook.com", "hotmail.co.uk", "live.com"] }
  - { name: catchall, domains: ["*"] }
  senders:
  - { match: "oldbrand.com", match_on: envelope, chain: [overflow] }
  default_chain: [overflow]
  routes:
  - name: overflow
    overflow: true
    downstream:
      host: o.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@established.com" }
"#;

    fn config() -> Config {
        crate::config::from_str(CFG, "test").expect("fixture is valid")
    }

    /// Pre-D-100 behaviour: literal, else the catch-all — what `Grouper::literal`
    /// gives for a ramp with no `mx` lists, checked separately below.
    fn group_of(cfg: &Config, recipient: &str) -> String {
        let ramp = cfg.default_ramp();
        literal(ramp, recipient)
            .or_else(|| ramp.catchall_group())
            .expect("catch-all always exists")
            .name
            .clone()
    }

    #[test]
    fn resolves_a_configured_domain_to_its_group() {
        let cfg = config();
        assert_eq!(group_of(&cfg, "bob@gmail.com"), "google");
        assert_eq!(group_of(&cfg, "bob@googlemail.com"), "google");
        assert_eq!(group_of(&cfg, "bob@outlook.com"), "microsoft");
        assert_eq!(group_of(&cfg, "bob@live.com"), "microsoft");
    }

    #[test]
    fn matching_is_case_insensitive_on_both_sides() {
        let cfg = config();
        assert_eq!(group_of(&cfg, "Bob@GMAIL.COM"), "google");
        assert_eq!(group_of(&cfg, "bob@Hotmail.CO.UK"), "microsoft");
    }

    #[test]
    fn an_unlisted_domain_falls_to_the_catch_all() {
        let cfg = config();
        assert_eq!(group_of(&cfg, "bob@example.com"), "catchall");
        assert_eq!(group_of(&cfg, "bob@yahoo.com"), "catchall");
    }

    #[test]
    fn matching_is_exact_not_by_suffix() {
        // The bug this guards: a suffix match would put `notgmail.com` and
        // `gmail.com.evil.net` in the google bucket, and a warm-up would be
        // steered by an attacker's choice of recipient domain.
        let cfg = config();
        assert_eq!(group_of(&cfg, "bob@notgmail.com"), "catchall");
        assert_eq!(group_of(&cfg, "bob@gmail.com.evil.net"), "catchall");
        assert_eq!(group_of(&cfg, "bob@mail.gmail.com"), "catchall");
    }

    #[test]
    fn a_literal_star_in_a_recipient_does_not_match_the_catch_all_entry() {
        // `*` is the catch-all marker, not a domain. An address at the literal
        // domain "*" must land in the catch-all by fallback, not by matching its
        // own list entry — otherwise a group whose list is `["*"]` would appear
        // to have a real domain in it.
        let cfg = config();
        assert_eq!(group_of(&cfg, "bob@*"), "catchall");
    }

    #[test]
    fn first_matching_group_wins() {
        // Config order decides, and §4.2 already rejects a domain listed in more
        // than one group — so this only fixes the behaviour, it does not invite
        // the ambiguity.
        let cfg = config();
        assert_eq!(group_of(&cfg, "bob@gmail.com"), "google");
    }

    #[test]
    fn an_address_with_no_domain_falls_to_the_catch_all() {
        // RFC 5321 §4.5.1 requires bare `postmaster` to be accepted, and §5.2's
        // parser passes it through. It has to be counted somewhere.
        let cfg = config();
        assert_eq!(group_of(&cfg, "postmaster"), "catchall");
        assert_eq!(group_of(&cfg, "bob@"), "catchall");
        assert_eq!(group_of(&cfg, "@gmail.com"), "catchall");
        assert_eq!(group_of(&cfg, ""), "catchall");
    }

    #[test]
    fn a_quoted_local_part_containing_an_at_uses_the_last_one() {
        let cfg = config();
        assert_eq!(group_of(&cfg, r#""a@b"@gmail.com"#), "google");
    }

    // -- D-100: MX grouping ------------------------------------------------

    use crate::preflight::resolver::Fake;

    fn mx_config() -> Config {
        let yaml = CFG
            .replace(
                r#"- { name: google, domains: ["gmail.com", "googlemail.com"] }"#,
                r#"- { name: google, domains: ["gmail.com", "googlemail.com"], mx: ["google.com", "googlemail.com"] }"#,
            )
            .replace(
                r#"- { name: microsoft, domains: ["outlook.com", "hotmail.co.uk", "live.com"] }"#,
                r#"- { name: microsoft, domains: ["outlook.com", "hotmail.co.uk", "live.com"], mx: ["mail.protection.outlook.com"] }"#,
            );
        assert_ne!(yaml, CFG, "the fixture rewrite must have applied");
        crate::config::from_str(&yaml, "test").expect("fixture is valid")
    }

    fn workspace_and_m365() -> Fake {
        Fake::new()
            .with_mx(
                "acme.example",
                &[(1, "aspmx.l.google.com."), (5, "alt1.aspmx.l.google.com.")],
            )
            .with_mx("newer-workspace.example", &[(1, "smtp.google.com.")])
            .with_mx(
                "hospital.example",
                &[(0, "hospital-example.mail.protection.outlook.com.")],
            )
            .with_mx("selfhosted.example", &[(10, "mx.selfhosted.example.")])
            .with_mx("notgoogle.example", &[(10, "mx.notgoogle.com.")])
            // Filtered first, Google behind it: a sender talks to the filter.
            .with_mx(
                "filtered.example",
                &[(10, "mx1.pphosted.com."), (20, "aspmx.l.google.com.")],
            )
            .with_mx("nullmx.example", &[(0, ".")])
            .failing_mx("broken.example", "SERVFAIL")
    }

    async fn mx_group_of(grouper: &Grouper, cfg: &Config, recipient: &str) -> (String, Basis) {
        let r = grouper
            .resolve(cfg.default_ramp(), recipient)
            .await
            .expect("catch-all always exists");
        (r.group.name.clone(), r.basis)
    }

    #[tokio::test]
    async fn a_workspace_domain_joins_google_by_mx() {
        let cfg = mx_config();
        let g = Grouper::new(Arc::new(workspace_and_m365()));
        assert_eq!(
            mx_group_of(&g, &cfg, "cfo@acme.example").await,
            ("google".into(), Basis::Mx("aspmx.l.google.com".into()))
        );
        assert_eq!(
            mx_group_of(&g, &cfg, "cfo@newer-workspace.example").await,
            ("google".into(), Basis::Mx("smtp.google.com".into()))
        );
    }

    #[tokio::test]
    async fn a_microsoft_365_tenant_joins_microsoft_by_mx() {
        let cfg = mx_config();
        let g = Grouper::new(Arc::new(workspace_and_m365()));
        assert_eq!(
            mx_group_of(&g, &cfg, "ward7@hospital.example").await.0,
            "microsoft"
        );
    }

    #[tokio::test]
    async fn a_literal_match_wins_and_does_no_lookup() {
        let cfg = mx_config();
        let fake = Arc::new(workspace_and_m365());
        let g = Grouper::new(fake.clone());
        assert_eq!(
            mx_group_of(&g, &cfg, "bob@gmail.com").await,
            ("google".into(), Basis::Literal)
        );
        assert_eq!(fake.mx_lookups(), 0);
    }

    #[tokio::test]
    async fn suffixes_match_on_a_label_boundary_only() {
        let cfg = mx_config();
        let g = Grouper::new(Arc::new(workspace_and_m365()));
        assert_eq!(
            mx_group_of(&g, &cfg, "a@notgoogle.example").await,
            ("catchall".into(), Basis::CatchAll)
        );
        assert_eq!(
            mx_group_of(&g, &cfg, "a@selfhosted.example").await,
            ("catchall".into(), Basis::CatchAll)
        );
    }

    #[tokio::test]
    async fn only_the_lowest_preference_hosts_count() {
        // Google is a backup here; the filter is who receives the mail.
        let cfg = mx_config();
        let g = Grouper::new(Arc::new(workspace_and_m365()));
        assert_eq!(
            mx_group_of(&g, &cfg, "a@filtered.example").await.0,
            "catchall"
        );
    }

    #[tokio::test]
    async fn no_mx_a_null_mx_and_a_failed_lookup_all_land_in_the_catch_all() {
        let cfg = mx_config();
        let g = Grouper::new(Arc::new(workspace_and_m365()));
        assert_eq!(
            mx_group_of(&g, &cfg, "a@unlisted.example").await,
            ("catchall".into(), Basis::CatchAll)
        );
        assert_eq!(
            mx_group_of(&g, &cfg, "a@nullmx.example").await,
            ("catchall".into(), Basis::CatchAll)
        );
        assert_eq!(
            mx_group_of(&g, &cfg, "a@broken.example").await,
            ("catchall".into(), Basis::MxUnavailable)
        );
    }

    #[tokio::test]
    async fn a_slow_lookup_times_out_to_the_catch_all() {
        struct Slow;
        #[async_trait::async_trait]
        impl MxResolver for Slow {
            async fn mx(
                &self,
                _: &str,
            ) -> Result<
                crate::preflight::resolver::MxAnswer,
                crate::preflight::resolver::ResolveError,
            > {
                tokio::time::sleep(Duration::from_secs(60)).await;
                unreachable!("the grouper's timeout fires first")
            }
        }
        let cfg = mx_config();
        let g = Grouper::new(Arc::new(Slow)).with_timeout(Duration::from_millis(20));
        assert_eq!(
            mx_group_of(&g, &cfg, "a@acme.example").await,
            ("catchall".into(), Basis::MxUnavailable)
        );
    }

    #[tokio::test]
    async fn a_second_recipient_at_the_same_domain_is_answered_from_the_cache() {
        let cfg = mx_config();
        let fake = Arc::new(workspace_and_m365());
        let g = Grouper::new(fake.clone());
        mx_group_of(&g, &cfg, "a@acme.example").await;
        mx_group_of(&g, &cfg, "b@ACME.example").await;
        mx_group_of(&g, &cfg, "a@broken.example").await;
        mx_group_of(&g, &cfg, "b@broken.example").await;
        assert_eq!(fake.mx_lookups(), 2, "one per domain, failures included");
    }

    #[tokio::test]
    async fn a_ramp_without_mx_lists_never_looks_anything_up() {
        let cfg = config();
        let fake = Arc::new(workspace_and_m365());
        let g = Grouper::new(fake.clone());
        assert_eq!(
            mx_group_of(&g, &cfg, "cfo@acme.example").await,
            ("catchall".into(), Basis::CatchAll)
        );
        assert_eq!(fake.mx_lookups(), 0);
    }

    #[tokio::test]
    async fn the_literal_grouper_matches_the_pre_d100_rule_exactly() {
        let cfg = config();
        let g = Grouper::literal();
        for r in [
            "bob@gmail.com",
            "Bob@GMAIL.COM",
            "bob@live.com",
            "bob@example.com",
            "postmaster",
            "@gmail.com",
            r#""a@b"@gmail.com"#,
        ] {
            assert_eq!(
                g.group_name(cfg.default_ramp(), r).await,
                group_of(&cfg, r),
                "{r}"
            );
        }
    }
}
