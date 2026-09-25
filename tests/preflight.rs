//! §6.7 through the chain walk, against real Postgres.
//!
//! `src/preflight/mod.rs`'s unit tests own the record parsing, the domain
//! extraction and the registry's rules. What only the walk can show is here: that
//! a failing `strict` route **steers** rather than refusing, that a non-strict
//! one does not steer at all, and that a chain with nothing left is §10.3's `451`
//! and never a `5xx` — which is the §14.1 line this feature could most easily
//! cross, since a DNS problem is the definition of something that should not make
//! a client suppress a recipient.

// Postgres-backed: the storage layer under test is `PgQuotaStore`. The SQL
// Server build runs the backend-neutral suite instead (tests/store_mssql.rs,
// D-084).
#![cfg(feature = "postgres")]

mod support;

use std::sync::Arc;

use simmer::config::Config;
use simmer::frequency::Frequency;
use simmer::preflight::resolver::{Fake, TxtResolver};
use simmer::preflight::{self, Registry};
use simmer::quota::store::QuotaStore;
use simmer::quota::PgQuotaStore;
use simmer::routing::chain::{self, SkipReason, Walk};
use sqlx::PgPool;

const BODY: &str = "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n";

/// A warming route that preflights `newbrand.com`, in front of an uncapped
/// overflow that does not preflight at all. Allowances are far above anything
/// these tests send, so a route skipped for quota could never be mistaken for one
/// skipped by §6.7.
fn config(strict: bool) -> Config {
    simmer::config::from_str(&raw_config(strict), "test").expect("fixture is valid")
}

/// The same, with nothing to steer to — so chain exhaustion is reachable.
/// `strict_senders` rather than a `default_chain`, because §4.2 will not accept a
/// default chain that does not end in an overflow route, for the same reason this
/// test exists.
fn raw_config_without_overflow() -> String {
    raw_config(true)
        .replace("chain: [warming, overflow] }", "chain: [warming] }")
        .replace("default_chain: [overflow]", "strict_senders: true")
}

fn config_without_overflow() -> Config {
    simmer::config::from_str(&raw_config_without_overflow(), "test").expect("fixture is valid")
}

fn raw_config(strict: bool) -> String {
    format!(
        r#"
server:
  listeners:
    - address: "127.0.0.1:0"
  hostname: "simmer.test"
  max_message_bytes: 100000
  max_recipients: 1
  max_concurrent_sessions: 16
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
  - {{ match: "oldbrand.com", match_on: envelope, chain: [warming, overflow] }}
  default_chain: [overflow]
  routes:
  - name: warming
    downstream:
      host: "127.0.0.1"
      port: 2525
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
      timeouts: {{ connect: 2s, command: 2s, data: 2s }}
    identity: {{ envelope_from: "bounce@newbrand.com" }}
    preflight:
      enabled: true
      spf_include: "spf.postal.internal"
      dkim_selector: "s1"
      require_dmarc: true
      strict: {strict}
    warmup:
      started: "2020-01-01T00:00:00Z"
      schedule: {{ default: [1000] }}
  - name: overflow
    overflow: true
    downstream:
      host: "127.0.0.1"
      port: 2526
      tls: off
      pool: {{ max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }}
      timeouts: {{ connect: 2s, command: 2s, data: 2s }}
    identity: {{ envelope_from: "bounce@established.com" }}
"#
    )
}

/// DNS as a correctly provisioned `newbrand.com` looks.
fn healthy_dns() -> Fake {
    Fake::new()
        .with("newbrand.com", &["v=spf1 include:spf.postal.internal ~all"])
        .with(
            "s1._domainkey.newbrand.com",
            &["v=DKIM1; k=rsa; p=MIIBIjANBgkqhkiG9w0BAQ"],
        )
        .with("_dmarc.newbrand.com", &["v=DMARC1; p=none"])
}

/// The §6.5 failure this whole feature exists to catch: everything resolves, and
/// the DKIM selector is not there — so the downstream signs with its own `d=` and
/// the ramp builds no reputation for `newbrand.com` at all.
fn dns_without_dkim() -> Fake {
    Fake::new()
        .with("newbrand.com", &["v=spf1 include:spf.postal.internal ~all"])
        .with("_dmarc.newbrand.com", &["v=DMARC1; p=none"])
}

fn store(pool: PgPool) -> Arc<dyn QuotaStore> {
    Arc::new(PgQuotaStore::new(pool))
}

async fn registry_after(cfg: &Config, dns: &dyn TxtResolver) -> Registry {
    let registry = Registry::new();
    preflight::check_once(&preflight::plan(cfg), dns, &registry).await;
    registry
}

async fn walk(
    cfg: &Config,
    store: &Arc<dyn QuotaStore>,
    registry: &Registry,
    chain: &[&str],
) -> (Option<String>, Vec<chain::Step>) {
    let mut evaluation = Vec::new();
    let chain: Vec<String> = chain.iter().map(|s| s.to_string()).collect();
    let walked = chain::walk_and_reserve(
        cfg.default_ramp(),
        &simmer::routing::domain_group::Grouper::literal(),
        &cfg.dot_insensitive_domains,
        store,
        &Frequency::new(),
        registry,
        &chain,
        None,
        &["someone@example.com".to_string()],
        "preflight-test",
        &mut evaluation,
    )
    .await
    .expect("walk");

    let selected = match walked {
        Walk::Selected(s) => Some(s.route.name.clone()),
        Walk::Exhausted => None,
    };
    (selected, evaluation)
}

// ---------------------------------------------------------------------------
// The checks themselves, through `check_once`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_healthy_domain_passes_all_three_checks() {
    let cfg = config(false);
    let registry = registry_after(&cfg, &healthy_dns()).await;

    let report = registry.report("main", "warming").expect("a report");
    assert_eq!(report.domain, "newbrand.com");
    assert!(report.all_ok(), "{:?}", report.checks);
    assert_eq!(report.checks.len(), 3, "SPF, DKIM and DMARC were all asked");
}

#[tokio::test]
async fn a_missing_dkim_selector_is_the_failure_6_5_is_about() {
    let cfg = config(false);
    let registry = registry_after(&cfg, &dns_without_dkim()).await;

    let report = registry.report("main", "warming").expect("a report");
    assert!(!report.all_ok());

    let dkim = report
        .checks
        .iter()
        .find(|c| c.check == preflight::Check::Dkim)
        .expect("a DKIM check");
    assert!(!dkim.ok);
    assert!(
        dkim.detail.contains("s1._domainkey.newbrand.com"),
        "the detail names what was looked up, so an operator can go and look at \
         it themselves: {}",
        dkim.detail
    );

    assert!(
        report
            .checks
            .iter()
            .filter(|c| c.check != preflight::Check::Dkim)
            .all(|c| c.ok),
        "the other two still pass — a partial failure is the realistic case, and \
         it is exactly the one that looks fine from the mail flow"
    );
}

#[tokio::test]
async fn dmarc_is_not_checked_unless_required() {
    let mut cfg = config(false);
    for route in &mut cfg.default_ramp_mut().routes {
        if let Some(p) = route.preflight.as_mut() {
            p.require_dmarc = false;
        }
    }
    // DNS with no DMARC record at all.
    let dns = Fake::new()
        .with("newbrand.com", &["v=spf1 include:spf.postal.internal ~all"])
        .with(
            "s1._domainkey.newbrand.com",
            &["v=DKIM1; k=rsa; p=MIIBIjANBgkqhkiG9w0BAQ"],
        );

    let registry = registry_after(&cfg, &dns).await;
    let report = registry.report("main", "warming").expect("a report");

    assert_eq!(report.checks.len(), 2, "only SPF and DKIM were asked");
    assert!(
        report.all_ok(),
        "§6.7 checks DMARC 'only if require_dmarc: true', so its absence is not a \
         failure when it was not required"
    );
}

#[tokio::test]
async fn a_resolver_failure_is_a_failed_check_and_says_so() {
    let cfg = config(false);
    let dns = Fake::new()
        .failing("newbrand.com", "connection timed out")
        .with(
            "s1._domainkey.newbrand.com",
            &["v=DKIM1; k=rsa; p=MIIBIjANBgkqhkiG9w0BAQ"],
        )
        .with("_dmarc.newbrand.com", &["v=DMARC1; p=none"]);

    let report = registry_after(&cfg, &dns)
        .await
        .report("main", "warming")
        .expect("a report");

    let spf = report
        .checks
        .iter()
        .find(|c| c.check == preflight::Check::Spf)
        .expect("an SPF check");
    assert!(!spf.ok);
    assert!(
        spf.detail.contains("connection timed out"),
        "'could not find out' and 'the record is wrong' are different problems \
         and the detail has to distinguish them: {}",
        spf.detail
    );
}

#[tokio::test]
async fn a_route_with_no_constant_domain_is_never_planned() {
    let mut cfg = config(true);
    cfg.default_ramp_mut().routes[0].identity.envelope_from =
        "bounce@{{original.envelope_from.domain}}".to_string();

    assert!(
        preflight::plan(&cfg).is_empty(),
        "there is no one domain to check, so the route is not planned (D-064)"
    );
}

// ---------------------------------------------------------------------------
// What it does to the walk (§6.7's `strict`)
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn a_failing_check_on_a_non_strict_route_does_not_steer(pool: PgPool) {
    let cfg = config(false);
    let store = store(pool);
    let registry = registry_after(&cfg, &dns_without_dkim()).await;

    let (selected, _) = walk(&cfg, &store, &registry, &["warming", "overflow"]).await;
    assert_eq!(
        selected.as_deref(),
        Some("warming"),
        "§6.7: without strict, a failure is a WARN and a gauge and nothing else. \
         Mail keeps flowing on a route whose DKIM is broken, which is the \
         deliberate default — a DNS blip must not become an outage"
    );
}

#[sqlx::test]
async fn a_failing_check_on_a_strict_route_steers_to_the_next_link(pool: PgPool) {
    let cfg = config(true);
    let store = store(pool);
    let registry = registry_after(&cfg, &dns_without_dkim()).await;

    let (selected, evaluation) = walk(&cfg, &store, &registry, &["warming", "overflow"]).await;
    assert_eq!(selected.as_deref(), Some("overflow"));
    assert_eq!(
        evaluation[0].outcome,
        Err(SkipReason::Preflight),
        "the reason must be `preflight`, not `quota` or `frequency`: the three \
         call for entirely different responses"
    );
    assert_eq!(
        chain::render(&evaluation),
        "warming=preflight,overflow=selected"
    );
}

#[sqlx::test]
async fn a_passing_check_on_a_strict_route_selects_it(pool: PgPool) {
    let cfg = config(true);
    let store = store(pool);
    let registry = registry_after(&cfg, &healthy_dns()).await;

    let (selected, _) = walk(&cfg, &store, &registry, &["warming", "overflow"]).await;
    assert_eq!(selected.as_deref(), Some("warming"));
}

#[sqlx::test]
async fn a_strict_route_with_no_report_yet_is_still_selected(pool: PgPool) {
    // Fail open. The registry is empty because no pass has completed — a slow
    // resolver at boot, or one that could not be built at all. §6.7's whole
    // posture is that this must not stop mail.
    let cfg = config(true);
    let store = store(pool);

    let (selected, _) = walk(&cfg, &store, &Registry::new(), &["warming", "overflow"]).await;
    assert_eq!(
        selected.as_deref(),
        Some("warming"),
        "no verdict is not a failing verdict"
    );
}

#[tokio::test]
async fn a_chain_with_nothing_left_is_451_and_never_550() {
    // §10.3 and §14.1 together, and the reason this test drives a real client
    // rather than asserting on the walk's return value: the one thing that must
    // never happen is a *client* recording a permanent failure for a recipient
    // because our DNS was misconfigured. That is a claim about the bytes on the
    // wire, so it is tested on the wire.
    let cfg = config_without_overflow();
    let registry = Arc::new(registry_after(&cfg, &dns_without_dkim()).await);
    assert!(
        registry
            .report("main", "warming")
            .is_some_and(|r| !r.all_ok()),
        "precondition: the only route in the chain is failing preflight"
    );

    let simmer = support::Simmer::start_with(
        &raw_config_without_overflow(),
        Arc::new(support::GrantAllQuota::new()),
        registry,
    )
    .await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c
        .deliver("jane@oldbrand.com", "bob@example.com", BODY)
        .await;

    assert_eq!(r.code, 451, "never a 5xx over a DNS problem: {r:?}");
    assert!(r.contains("4.7.1"), "{r:?}");
}

// ---------------------------------------------------------------------------
// §9.2 — what the control plane says about it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_route_view_reports_the_checks_and_whether_they_block() {
    let cfg = config(true);
    let registry = registry_after(&cfg, &dns_without_dkim()).await;

    let view = simmer::admin::view::project_route(
        cfg.default_ramp(),
        &cfg.default_ramp().routes[0],
        Default::default(),
        &simmer::admin::view::UsageByRoute::new(),
        &registry,
        &simmer::downstream::Pool::build(&cfg),
        chrono::Utc::now(),
    );

    let p = view.preflight.expect("a preflight block");
    assert_eq!(p.domain, "newbrand.com");
    assert!(!p.ok);
    assert!(
        p.strict,
        "an operator has to be able to tell a warning from a block: the same \
         failing report means 'mail still flows' on one route and 'this route is \
         out' on another"
    );
    assert_eq!(p.checks.len(), 3);
}

#[tokio::test]
async fn a_route_that_was_never_checked_reports_null_rather_than_a_pass() {
    let cfg = config(true);

    let view = simmer::admin::view::project_route(
        cfg.default_ramp(),
        &cfg.default_ramp().routes[1], // overflow: no preflight block at all
        Default::default(),
        &simmer::admin::view::UsageByRoute::new(),
        &registry_after(&cfg, &healthy_dns()).await,
        &simmer::downstream::Pool::build(&cfg),
        chrono::Utc::now(),
    );

    assert!(
        view.preflight.is_none(),
        "reporting a pass that was never established is the §9 lie the control \
         plane must not tell"
    );
}

#[tokio::test]
async fn no_read_endpoint_leaks_a_recipient_through_the_preflight_block() {
    // §7.3's standing rule, re-asserted for the field phase 8 added. A detail
    // line is built from configuration and DNS, never from a message.
    let cfg = config(true);
    let registry = registry_after(&cfg, &dns_without_dkim()).await;

    let view = simmer::admin::view::project_routes(
        cfg.default_ramp(),
        &Default::default(),
        &simmer::admin::view::UsageByRoute::new(),
        &registry,
        &simmer::downstream::Pool::build(&cfg),
        chrono::Utc::now(),
    );
    let json = serde_json::to_string(&view).expect("serialises");

    assert!(
        !json.contains('@'),
        "no read endpoint emits so much as an @: {json}"
    );
}
