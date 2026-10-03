//! §6.7 — the DNS preflight.
//!
//! ```text
//! For each route with `preflight.enabled: true`, Simmer resolves and checks the
//! outbound identity's domain: SPF, DKIM, DMARC. Checks run at startup and on an
//! interval (default 15 minutes). By default a failure produces a WARN log and
//! sets simmer_preflight_ok{route,check} 0; it does not prevent startup or block
//! mail, because a DNS blip would otherwise become an outage. With
//! preflight.strict: true a failing check makes the route ineligible for
//! selection, causing traffic to fall to the next link in the chain.
//! ```
//!
//! ## What this is actually for
//!
//! §6.5 names it: domain reputation accrues to the DKIM `d=` domain, so if the
//! downstream signs with `d=sendgrid.net` rather than `d=newbrand.com` — which is
//! what happens until the provider's domain authentication is completed — then
//! "a route can ramp flawlessly for weeks and build no domain reputation
//! whatsoever, and nothing in the mail flow would reveal it".
//!
//! That is a standing property of a domain's provisioning, not a property of any
//! message. It is why this runs on a timer rather than on the message path, and
//! why the answer is cached in a [`Registry`] the chain walk reads. Checking it
//! per message would be tens of thousands of lookups a day to re-learn a fact
//! that changes when somebody edits a DNS zone.
//!
//! ## Non-blocking is the design, not a shortcut
//!
//! Every failure path here ends at "this route is not eligible" or at "nothing is
//! known about this route", never at "this message is refused". A `strict` route
//! that fails makes the message *steer* to the next link exactly as §7.3 does, and
//! a chain with no link left is §10.3's `451` — which §14.1 requires, because a
//! `5xx` would make the client suppress a deliverable recipient over a DNS
//! problem.
//!
//! **Fail open when nothing is known.** A route with no report yet is eligible.
//! The alternative — treating "not yet checked" as a failure — turns a slow
//! resolver at boot into a chain-wide outage, which is the outcome §6.7's own
//! wording exists to prevent.

pub mod resolver;

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::config::{Config, Route};
use crate::metrics;
use crate::smtp::Shutdown;
use resolver::TxtResolver;

/// §6.7's interval. §4.1 defines no key for it, so it is a constant rather than
/// configuration — adding a key would be a schema divergence for a value nothing
/// has asked to vary (D-063).
const INTERVAL: Duration = Duration::from_secs(15 * 60);

// ---------------------------------------------------------------------------
// The three checks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Check {
    Spf,
    Dkim,
    Dmarc,
}

impl Check {
    /// The `check` label of `simmer_preflight_ok{route,check}`.
    pub fn as_str(self) -> &'static str {
        match self {
            Check::Spf => "spf",
            Check::Dkim => "dkim",
            Check::Dmarc => "dmarc",
        }
    }
}

/// §6.7: "A `v=spf1` TXT record exists and contains the configured
/// `spf_include`".
///
/// Deliberately a substring test rather than a parse of the SPF grammar. The
/// configured value is a mechanism as the operator wrote it (`include:spf.foo`
/// or just `spf.foo`), and §6.7 asks whether the record *contains* it. Parsing
/// would mean taking a position on macro expansion, `redirect=` and nested
/// `include:` resolution — a resolver of its own, to answer a question §6.7 did
/// not ask.
pub fn spf_ok(records: &[String], include: &str) -> bool {
    records.iter().any(|r| {
        let r = r.trim();
        r.len() >= 6 && r[..6].eq_ignore_ascii_case("v=spf1") && r.contains(include)
    })
}

/// §6.7: "resolves to a TXT record containing `p=` with a non-empty key".
///
/// A published-but-revoked DKIM key is exactly `p=` with an empty value, and it
/// is the shape that matters here: a revoked selector resolves, so a test for
/// mere existence would pass while every message went unsigned.
pub fn dkim_ok(records: &[String]) -> bool {
    records.iter().any(|record| {
        record.split(';').any(|tag| {
            tag.trim()
                .strip_prefix("p=")
                .is_some_and(|key| !key.trim().is_empty())
        })
    })
}

/// §6.7: "resolves to a `v=DMARC1` record".
pub fn dmarc_ok(records: &[String]) -> bool {
    records
        .iter()
        .any(|r| r.trim().len() >= 8 && r.trim()[..8].eq_ignore_ascii_case("v=dmarc1"))
}

pub fn dkim_name(selector: &str, domain: &str) -> String {
    format!("{selector}._domainkey.{domain}")
}

pub fn dmarc_name(domain: &str) -> String {
    format!("_dmarc.{domain}")
}

// ---------------------------------------------------------------------------
// Which domain to check
// ---------------------------------------------------------------------------

/// The domain preflight should ask about, when there is exactly one.
///
/// `identity.envelope_from` is a §6.3 template, so its domain is not necessarily
/// a constant: `bounce@{{original.envelope_from.domain}}` means "whatever domain
/// the message arrived with", and there is then no single domain to check on a
/// timer. Note that §6.6 does not rule this out — that template gives the same
/// answer applied twice, which is the property §6.6 tests.
///
/// Returns `None` for that case, and the caller warns and skips the route
/// (D-064). It is deliberately *not* a startup error: §6.7's whole posture is
/// that preflight must not be able to stop the service, and a config quirk is a
/// poor reason to break that.
///
/// The test is textual rather than a render against a probe message, because a
/// probe would produce a real-looking answer about a domain no real message uses
/// — a check that reports on the wrong domain is worse than one that declines.
pub fn literal_domain(envelope_from: &str) -> Option<&str> {
    let at = envelope_from.rfind('@')?;
    let domain = envelope_from[at + 1..].trim();
    if domain.is_empty() || domain.contains("{{") {
        return None;
    }
    Some(domain)
}

/// One route's checks, resolved from configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The route's ramp (D-099), for the metric's `ramp` label.
    pub ramp: String,
    pub route: String,
    pub domain: String,
    pub spf_include: Option<String>,
    pub dkim_selector: Option<String>,
    pub require_dmarc: bool,
}

/// Every route preflight can actually check.
///
/// Skips routes with preflight disabled or absent (D-009), and routes whose
/// identity domain is not a constant — [`warnings`] is what tells the operator
/// about the second case.
pub fn plan(cfg: &Config) -> Vec<Plan> {
    // Keyed by route name: sound only while §4.2 allows one ramp (D-099).
    cfg.all_routes()
        .filter(|r| r.preflight_enabled())
        .filter_map(|route| {
            let domain = literal_domain(&route.identity.envelope_from)?;
            let p = route.preflight.as_ref()?;
            Some(Plan {
                ramp: route.ramp.clone(),
                route: route.name.clone(),
                domain: domain.to_string(),
                spf_include: p.spf_include.clone(),
                dkim_selector: p.dkim_selector.clone(),
                require_dmarc: p.require_dmarc,
            })
        })
        .collect()
}

pub fn any_enabled(cfg: &Config) -> bool {
    !plan(cfg).is_empty()
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    pub check: Check,
    pub ok: bool,
    /// Why, in one line, for `/routes` and the `WARN`. Never a record's full
    /// contents: a TXT record can be long and this ends up in a log line.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteReport {
    pub domain: String,
    pub checked_at: DateTime<Utc>,
    pub checks: Vec<CheckReport>,
}

impl RouteReport {
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
}

/// The last answer for each route, read by the chain walk and by §9.2's
/// `/routes`.
#[derive(Default)]
pub struct Registry {
    /// Keyed `(ramp, route)` (D-099).
    routes: RwLock<HashMap<(String, String), RouteReport>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn report(&self, ramp: &str, route: &str) -> Option<RouteReport> {
        self.routes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(ramp.to_string(), route.to_string()))
            .cloned()
    }

    fn put(&self, ramp: &str, route: &str, report: RouteReport) {
        self.routes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert((ramp.to_string(), route.to_string()), report);
    }

    /// §6.7's `strict`: does preflight make this route ineligible right now?
    ///
    /// False unless the route is strict **and** has a report **and** that report
    /// failed. No report means no opinion — see the module comment on failing
    /// open, and note that a route whose domain is not a constant never gets one,
    /// so `strict` has no effect there (D-064, and `warnings` says so out loud).
    pub fn blocks(&self, route: &Route) -> bool {
        let Some(p) = &route.preflight else {
            return false;
        };
        if !p.enabled || !p.strict {
            return false;
        }
        self.report(&route.ramp, &route.name)
            .is_some_and(|r| !r.all_ok())
    }
}

// ---------------------------------------------------------------------------
// Running the checks
// ---------------------------------------------------------------------------

/// One pass over one route. Split out so a test can drive it against a fake
/// resolver without a timer or a network.
pub async fn check_route(plan: &Plan, resolver: &dyn TxtResolver) -> RouteReport {
    let mut checks = Vec::new();

    if let Some(include) = &plan.spf_include {
        checks.push(match resolver.txt(&plan.domain).await {
            Ok(records) if spf_ok(&records, include) => CheckReport {
                check: Check::Spf,
                ok: true,
                detail: format!("v=spf1 record includes {include}"),
            },
            Ok(records) if records.is_empty() => CheckReport {
                check: Check::Spf,
                ok: false,
                detail: format!("no TXT record at {}", plan.domain),
            },
            Ok(_) => CheckReport {
                check: Check::Spf,
                ok: false,
                detail: format!("no v=spf1 record at {} includes {include}", plan.domain),
            },
            Err(e) => CheckReport {
                check: Check::Spf,
                ok: false,
                detail: format!("resolving {}: {e}", plan.domain),
            },
        });
    }

    if let Some(selector) = &plan.dkim_selector {
        let name = dkim_name(selector, &plan.domain);
        checks.push(match resolver.txt(&name).await {
            Ok(records) if dkim_ok(&records) => CheckReport {
                check: Check::Dkim,
                ok: true,
                detail: format!("{name} publishes a non-empty p="),
            },
            Ok(records) if records.is_empty() => CheckReport {
                check: Check::Dkim,
                ok: false,
                detail: format!("no TXT record at {name}"),
            },
            // The revoked-key case, called out because it is the one that looks
            // like success from every other angle.
            Ok(_) => CheckReport {
                check: Check::Dkim,
                ok: false,
                detail: format!("{name} resolves but publishes no non-empty p= (revoked key?)"),
            },
            Err(e) => CheckReport {
                check: Check::Dkim,
                ok: false,
                detail: format!("resolving {name}: {e}"),
            },
        });
    }

    if plan.require_dmarc {
        let name = dmarc_name(&plan.domain);
        checks.push(match resolver.txt(&name).await {
            Ok(records) if dmarc_ok(&records) => CheckReport {
                check: Check::Dmarc,
                ok: true,
                detail: format!("{name} publishes v=DMARC1"),
            },
            Ok(_) => CheckReport {
                check: Check::Dmarc,
                ok: false,
                detail: format!("no v=DMARC1 record at {name}"),
            },
            Err(e) => CheckReport {
                check: Check::Dmarc,
                ok: false,
                detail: format!("resolving {name}: {e}"),
            },
        });
    }

    RouteReport {
        domain: plan.domain.clone(),
        checked_at: Utc::now(),
        checks,
    }
}

/// One pass over every planned route, updating the registry and the gauges.
pub async fn check_once(plans: &[Plan], resolver: &dyn TxtResolver, registry: &Registry) {
    // §9.6 (D-126) — one span per pass, at startup and on the interval. A
    // failing check is a WARN event inside it.
    let span = tracing::info_span!(
        "simmer.preflight",
        otel.name = "simmer.preflight",
        routes = plans.len(),
        failed = tracing::field::Empty,
    );
    let failed =
        tracing::Instrument::instrument(check_all(plans, resolver, registry), span.clone()).await;
    span.record("failed", failed);
}

/// [`check_once`]'s body. Returns how many checks failed.
async fn check_all(plans: &[Plan], resolver: &dyn TxtResolver, registry: &Registry) -> usize {
    let mut failed = 0;
    for plan in plans {
        let report = check_route(plan, resolver).await;

        for c in &report.checks {
            metrics::preflight_ok(&plan.ramp, &plan.route, c.check.as_str(), c.ok);
            if !c.ok {
                failed += 1;
                // WARN, not ERROR: on a non-strict route nothing has stopped, and
                // §6.5 is explicit that this is the failure nothing else in the
                // mail flow reveals — so it has to be visible without being an
                // alarm that a DNS blip trips.
                tracing::warn!(
                    route = %plan.route,
                    domain = %plan.domain,
                    check = c.check.as_str(),
                    detail = %c.detail,
                    "preflight check failed (§6.7)"
                );
            }
        }

        registry.put(&plan.ramp, &plan.route, report);
    }
    failed
}

/// Run until `shutdown` fires. `quota::sweeper::run`'s shape, on the same token.
///
/// The caller does not start this at all when no route is checkable — the phase 6
/// sweeper's precedent, and for the same reason: waking every fifteen minutes to
/// discover there is nothing to do is a cost with no reader.
pub async fn run(
    plans: Vec<Plan>,
    resolver: std::sync::Arc<dyn TxtResolver>,
    registry: std::sync::Arc<Registry>,
    shutdown: Shutdown,
) {
    let mut ticker = tokio::time::interval(INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; §6.7's startup pass is the caller's,
    // so skip it here rather than checking twice in the first second.
    ticker.tick().await;

    tracing::info!(
        routes = plans.len(),
        interval_secs = INTERVAL.as_secs(),
        "preflight started"
    );

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => check_once(&plans, resolver.as_ref(), &registry).await,
        }
    }

    tracing::info!("preflight stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // §6.7's table, one test per row and per way of getting it wrong
    // -----------------------------------------------------------------------

    #[test]
    fn spf_needs_both_the_version_tag_and_the_include() {
        let include = "spf.postal.internal";
        assert!(spf_ok(
            &["v=spf1 include:spf.postal.internal ~all".into()],
            include
        ));
        assert!(
            !spf_ok(&["v=spf1 include:other.example ~all".into()], include),
            "an SPF record that does not name the configured include fails"
        );
        assert!(
            !spf_ok(&["include:spf.postal.internal".into()], include),
            "a TXT that mentions the include but is not an SPF record is not one"
        );
        assert!(!spf_ok(&[], include), "no records at all");
    }

    #[test]
    fn spf_version_tag_is_case_insensitive_but_anchored() {
        assert!(spf_ok(&["V=SPF1 include:a.b -all".into()], "a.b"));
        assert!(
            !spf_ok(&["x v=spf1 include:a.b".into()], "a.b"),
            "the version tag must start the record: DNS TXT at a domain holds all \
             sorts of things and one of them mentioning v=spf1 is not an SPF policy"
        );
    }

    #[test]
    fn spf_picks_the_right_record_out_of_several() {
        // A domain's TXT set routinely holds verification tokens next to SPF.
        let records = vec![
            "google-site-verification=abc123".to_string(),
            "v=spf1 include:spf.postal.internal ~all".to_string(),
        ];
        assert!(spf_ok(&records, "spf.postal.internal"));
    }

    #[test]
    fn dkim_needs_a_non_empty_key() {
        assert!(dkim_ok(&["v=DKIM1; k=rsa; p=MIIBIjANBg".into()]));
        assert!(
            !dkim_ok(&["v=DKIM1; k=rsa; p=".into()]),
            "an empty p= is a REVOKED key, and it resolves — which is exactly why \
             §6.7 says 'with a non-empty key' rather than 'exists'"
        );
        assert!(
            !dkim_ok(&["v=DKIM1; k=rsa; p=   ".into()]),
            "whitespace is not a key either"
        );
        assert!(!dkim_ok(&["v=DKIM1; k=rsa".into()]), "no p= tag at all");
        assert!(!dkim_ok(&[]));
    }

    #[test]
    fn dkim_does_not_mistake_another_tag_ending_in_p_for_the_key() {
        // `sp=` is a real DMARC tag and `p=` is a substring of it. A contains()
        // test would pass this; splitting on ';' and anchoring does not.
        assert!(
            !dkim_ok(&["v=DKIM1; k=rsa; sp=something".into()]),
            "sp= is not p="
        );
    }

    #[test]
    fn dmarc_needs_the_version_tag_at_the_front() {
        assert!(dmarc_ok(&["v=DMARC1; p=none; rua=mailto:a@b".into()]));
        assert!(dmarc_ok(&["V=dmarc1; p=reject".into()]), "case-insensitive");
        assert!(!dmarc_ok(&["p=none".into()]));
        assert!(!dmarc_ok(&[]));
    }

    // -----------------------------------------------------------------------
    // Names
    // -----------------------------------------------------------------------

    #[test]
    fn the_looked_up_names_are_the_ones_6_7_specifies() {
        assert_eq!(
            dkim_name("s1", "newbrand.com"),
            "s1._domainkey.newbrand.com"
        );
        assert_eq!(dmarc_name("newbrand.com"), "_dmarc.newbrand.com");
    }

    // -----------------------------------------------------------------------
    // Which domain
    // -----------------------------------------------------------------------

    #[test]
    fn a_literal_envelope_from_yields_its_domain() {
        assert_eq!(literal_domain("bounce@newbrand.com"), Some("newbrand.com"));
    }

    #[test]
    fn a_templated_local_part_still_has_a_literal_domain() {
        assert_eq!(
            literal_domain("bounce+{{original.envelope_from.local}}@newbrand.com"),
            Some("newbrand.com"),
            "the domain is what preflight asks about; the local part is not"
        );
    }

    #[test]
    fn a_templated_domain_has_no_single_answer() {
        assert_eq!(
            literal_domain("bounce@{{original.envelope_from.domain}}"),
            None,
            "there is no one domain to check on a timer, so preflight declines \
             rather than checking the wrong one (D-064)"
        );
        assert_eq!(literal_domain("bounce@mail.{{tenant}}.com"), None);
    }

    #[test]
    fn a_malformed_envelope_from_has_no_domain() {
        assert_eq!(literal_domain("not-an-address"), None);
        assert_eq!(literal_domain("bounce@"), None);
    }

    // -----------------------------------------------------------------------
    // The registry, and what `strict` does
    // -----------------------------------------------------------------------

    fn report(ok: bool) -> RouteReport {
        RouteReport {
            domain: "newbrand.com".into(),
            checked_at: Utc::now(),
            checks: vec![CheckReport {
                check: Check::Spf,
                ok,
                detail: "test".into(),
            }],
        }
    }

    fn route_with(preflight: Option<(bool, bool)>) -> Route {
        let strict_clause = match preflight {
            None => String::new(),
            Some((enabled, strict)) => format!(
                "    preflight: {{ enabled: {enabled}, spf_include: \"a.b\", \
                 dkim_selector: \"s1\", strict: {strict} }}\n"
            ),
        };
        let yaml = format!(
            r#"
server:
  listeners:
    - address: "127.0.0.1:0"
  hostname: simmer.test
  max_message_bytes: 1000
  max_recipients: 1
  max_concurrent_sessions: 1
  allowed_cidrs: ["127.0.0.0/8"]
  timeouts: {{ command: 5s, data: 5s, session: 60s }}
  auth: {{ allow_insecure_auth: true }}
database: {{ url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }}
admin: {{ listen: "127.0.0.1:0", auth_token: "t" }}
logging: {{ level: warn, format: text }}
default_ramp: main
ramps:
 main:
  domain_groups:
  - {{ name: catchall, domains: ["*"] }}
  senders:
  - {{ match: "oldbrand.com", match_on: envelope, chain: [overflow] }}
  default_chain: [overflow]
  routes:
  - name: overflow
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: 2525
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
      timeouts: {{ connect: 2s, command: 2s, data: 2s }}
    identity: {{ envelope_from: "b@newbrand.com" }}
{strict_clause}"#
        );
        let cfg = crate::config::from_str(&yaml, "test").expect("fixture is valid");
        cfg.default_ramp().routes.first().cloned().unwrap()
    }

    #[test]
    fn a_non_strict_route_is_never_blocked_however_badly_it_fails() {
        let route = route_with(Some((true, false)));
        let reg = Registry::new();
        reg.put(&route.ramp, &route.name, report(false));
        assert!(
            !reg.blocks(&route),
            "§6.7: without strict, a failure is a WARN and a gauge and nothing else"
        );
    }

    #[test]
    fn a_strict_route_is_blocked_only_by_an_actual_failure() {
        let route = route_with(Some((true, true)));
        let reg = Registry::new();

        assert!(
            !reg.blocks(&route),
            "no report yet must fail OPEN: a slow resolver at boot must not be a \
             chain-wide outage"
        );

        reg.put(&route.ramp, &route.name, report(true));
        assert!(!reg.blocks(&route), "a passing report does not block");

        reg.put(&route.ramp, &route.name, report(false));
        assert!(
            reg.blocks(&route),
            "a failing report on a strict route blocks"
        );
    }

    #[test]
    fn a_route_with_no_preflight_block_is_never_blocked() {
        let route = route_with(None);
        let reg = Registry::new();
        reg.put(&route.ramp, &route.name, report(false));
        assert!(!reg.blocks(&route));
    }

    #[test]
    fn a_disabled_route_is_never_blocked() {
        let route = route_with(Some((false, true)));
        let reg = Registry::new();
        reg.put(&route.ramp, &route.name, report(false));
        assert!(
            !reg.blocks(&route),
            "enabled: false wins over strict: true — D-009 is that an absent or \
             disabled block means preflight does not run"
        );
    }
}
