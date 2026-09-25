//! `SPEC.md` §4.2 startup validation, end to end through the real loader.
//!
//! One test per rule, plus the one that matters most in practice: a config with
//! several independent faults must report *all* of them. §4.2 says "Report all
//! violations, not just the first", and the reason is operational — a service
//! that takes minutes to build should not be discovered to be misconfigured one
//! error at a time.

use simmer::config::{self, AutoShare, LoadError, ShareSchedule, Tail};

/// A minimal configuration that passes every rule. Tests mutate one thing.
const BASE: &str = r#"
server:
  listeners:
    - address: "127.0.0.1:25"
      auth: required
  hostname: "simmer.test"
  max_message_bytes: 26214400
  max_recipients: 100
  max_concurrent_sessions: 64
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth:
    allow_insecure_auth: true
    mechanisms: [PLAIN, LOGIN]
    users:
      - username: "cfapp"
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        grants: { send_as: ["oldbrand.com"] }
database:
  url: "postgres://simmer:simmer@localhost/simmer"
  max_connections: 10
  connect_timeout: 5s
  fail_closed: true
admin:
  listen: "127.0.0.1:8080"
  auth_token: "tok"
logging: { level: info, format: json }
default_ramp: main
ramps:
 main:
  domain_groups:
  - { name: google, domains: ["gmail.com"] }
  - { name: catchall, domains: ["*"] }
  senders:
  - match: "oldbrand.com"
    match_on: from_header
    chain: [warming, overflow]
  default_chain: [overflow]
  strict_senders: false
  routes:
  - name: warming
    downstream:
      host: "smtp.postal.internal"
      port: 587
      tls: required_verify
      pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }
    identity:
      envelope_from: "bounce@newbrand.com"
      set_headers:
        From: "Sales <sales@newbrand.com>"
        Reply-To: "{{original.from.address}}"
      unstable_headers: ["Reply-To"]
    preflight:
      enabled: true
      spf_include: "spf.postal.internal"
      dkim_selector: "s1"
      require_dmarc: true
    warmup:
      started: "2020-01-01T09:00:00Z"
      schedule:
        default: [50, 100, 200]
        overrides:
          google: [20, 50, 100]
  - name: overflow
    overflow: true
    downstream:
      host: "smtp.sendgrid.net"
      port: 587
      pool: { max_connections: 8, idle_ttl: 60s, max_messages_per_connection: 100 }
    identity:
      envelope_from: "bounce@mail.established.com"
      set_headers:
        From: "News <news@mail.established.com>"
"#;

fn load(yaml: &str) -> Result<config::Config, LoadError> {
    config::from_str(yaml, "<test>")
}

/// Assert the config is rejected, and that some violation mentions `needle`.
#[track_caller]
fn rejected_for(yaml: &str, needle: &str) {
    match load(yaml) {
        Ok(_) => panic!("expected rejection mentioning '{needle}', but the config was accepted"),
        Err(LoadError::Invalid(v)) => assert!(
            v.mentions(needle),
            "no violation mentioned '{needle}'. Got:\n{v}"
        ),
        Err(other) => panic!("expected a validation failure, got: {other}"),
    }
}

#[test]
fn the_base_fixture_is_valid() {
    // If this fails, every other test in the file is testing the wrong thing.
    load(BASE).expect("base fixture should be valid");
}

// -- §4.2: referenced route names ----------------------------------------

#[test]
fn rejects_a_chain_referencing_an_undefined_route() {
    let yaml = BASE.replace("chain: [warming, overflow]", "chain: [warming, nonesuch]");
    rejected_for(&yaml, "nonesuch");
}

#[test]
fn rejects_a_default_chain_referencing_an_undefined_route() {
    let yaml = BASE.replace("default_chain: [overflow]", "default_chain: [nonesuch]");
    rejected_for(&yaml, "nonesuch");
}

// -- §4.2: overflow position and count -----------------------------------

#[test]
fn rejects_an_overflow_route_that_is_not_last() {
    let yaml = BASE.replace("chain: [warming, overflow]", "chain: [overflow, warming]");
    rejected_for(&yaml, "must be last");
}

#[test]
fn rejects_more_than_one_overflow_route_in_a_chain() {
    let yaml = BASE.replace(
        "chain: [warming, overflow]",
        "chain: [warming, overflow, overflow]",
    );
    rejected_for(&yaml, "overflow routes");
}

// -- §4.2: warmup presence -----------------------------------------------

#[test]
fn rejects_an_overflow_route_carrying_a_warmup_block() {
    let yaml = BASE.replace(
        r#"    identity:
      envelope_from: "bounce@mail.established.com""#,
        r#"    warmup:
      started: "2020-01-01T09:00:00Z"
      schedule: { default: [10] }
    identity:
      envelope_from: "bounce@mail.established.com""#,
    );
    rejected_for(&yaml, "must not carry a warm-up schedule");
}

#[test]
fn rejects_a_non_overflow_route_without_a_warmup_block() {
    let yaml = BASE.replace(
        r#"    warmup:
      started: "2020-01-01T09:00:00Z"
      schedule:
        default: [50, 100, 200]
        overrides:
          google: [20, 50, 100]"#,
        "",
    );
    rejected_for(&yaml, "must carry a warm-up schedule");
}

// -- §4.2: domain groups -------------------------------------------------

#[test]
fn rejects_no_catchall_group() {
    let yaml = BASE.replace(
        r#"{ name: catchall, domains: ["*"] }"#,
        r#"{ name: other, domains: ["example.com"] }"#,
    );
    rejected_for(&yaml, "no group contains '*'");
}

#[test]
fn rejects_more_than_one_catchall_group() {
    let yaml = BASE.replace(
        r#"  - { name: catchall, domains: ["*"] }"#,
        "  - { name: catchall, domains: [\"*\"] }\n  - { name: another, domains: [\"*\"] }",
    );
    rejected_for(&yaml, "groups contain '*'");
}

#[test]
fn rejects_a_domain_in_more_than_one_group() {
    let yaml = BASE.replace(
        r#"  - { name: google, domains: ["gmail.com"] }"#,
        "  - { name: google, domains: [\"gmail.com\"] }\n  - { name: dup, domains: [\"gmail.com\"] }",
    );
    rejected_for(&yaml, "also appears in group");
}

// -- D-100: MX suffixes ----------------------------------------------------

fn with_groups(groups: &str) -> String {
    let yaml = BASE.replace(
        "  - { name: google, domains: [\"gmail.com\"] }\n  - { name: catchall, domains: [\"*\"] }\n",
        groups,
    );
    assert_ne!(yaml, BASE, "the fixture rewrite must have applied");
    yaml
}

#[test]
fn accepts_mx_suffixes_and_defaults_them_to_none() {
    let cfg = load(&with_groups(
        "  - { name: google, domains: [\"gmail.com\"], mx: [\"google.com\", \"googlemail.com\"] }\n  - { name: catchall, domains: [\"*\"] }\n",
    ))
    .expect("valid");
    let groups = &cfg.default_ramp().domain_groups;
    assert_eq!(groups[0].mx, ["google.com", "googlemail.com"]);
    assert!(groups[1].mx.is_empty());
}

#[test]
fn rejects_an_mx_suffix_in_more_than_one_group() {
    rejected_for(
        &with_groups(
            "  - { name: google, domains: [\"gmail.com\"], mx: [\"google.com\"] }\n  - { name: other, domains: [\"x.example\"], mx: [\"Google.com\"] }\n  - { name: catchall, domains: [\"*\"] }\n",
        ),
        "also appears in group",
    );
}

#[test]
fn rejects_mx_suffixes_on_the_catch_all() {
    rejected_for(
        &with_groups(
            "  - { name: google, domains: [\"gmail.com\"] }\n  - { name: catchall, domains: [\"*\"], mx: [\"google.com\"] }\n",
        ),
        "catch-all group must not list MX suffixes",
    );
}

#[test]
fn rejects_malformed_mx_suffixes() {
    for (suffix, needle) in [
        ("*.google.com", "is a suffix, not a pattern"),
        (".google.com", "must not start or end with '.'"),
        ("com", "at least two labels"),
        ("goo gle.com", "must be a host name"),
        ("", "must not be empty"),
    ] {
        rejected_for(
            &with_groups(&format!(
                "  - {{ name: google, domains: [\"gmail.com\"], mx: [\"{suffix}\"] }}\n  - {{ name: catchall, domains: [\"*\"] }}\n"
            )),
            needle,
        );
    }
}

// -- §4.2: schedules -----------------------------------------------------

#[test]
fn rejects_an_empty_schedule() {
    let yaml = BASE.replace("default: [50, 100, 200]", "default: []");
    rejected_for(&yaml, "must not be empty");
}

#[test]
fn rejects_a_negative_schedule_value() {
    // Typed i64 rather than u64 precisely so this arrives here as a validation
    // violation, reportable alongside others, rather than as a parse error that
    // aborts on the first bad element.
    let yaml = BASE.replace("default: [50, 100, 200]", "default: [50, -100, 200]");
    rejected_for(&yaml, "is negative");
}

#[test]
fn rejects_an_override_naming_an_undefined_domain_group() {
    let yaml = BASE.replace(
        "          google: [20, 50, 100]",
        "          nosuchgroup: [20, 50, 100]",
    );
    rejected_for(&yaml, "nosuchgroup");
}

// -- §4.2: auth ----------------------------------------------------------

const USER: &str = r#"    users:
      - username: "cfapp"
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        grants: { send_as: ["oldbrand.com"] }"#;

#[test]
fn rejects_auth_required_with_no_users() {
    assert!(BASE.contains(USER), "fixture drifted from USER");
    let yaml = BASE.replace(USER, "    users: []");
    rejected_for(&yaml, "no client could ever authenticate");
}

#[test]
fn rejects_auth_required_where_auth_could_never_be_used() {
    // The inversion of the rule this replaced (D-070). §4.2 used to *require*
    // allow_insecure_auth: true, because without inbound TLS nothing else could
    // work. Now false is the default, and the mistake worth catching is the
    // listener that requires AUTH, cannot encrypt, and refuses plaintext AUTH —
    // one that would refuse every message.
    let yaml = BASE.replace("allow_insecure_auth: true", "allow_insecure_auth: false");
    rejected_for(&yaml, "AUTH could never be used");
}

#[test]
fn accepts_insecure_auth_false_when_the_listener_does_not_require_auth() {
    // `optional` on a plaintext port with plaintext AUTH refused is coherent:
    // unauthenticated clients may send, and nobody's password crosses in clear.
    let yaml = BASE
        .replace("allow_insecure_auth: true", "allow_insecure_auth: false")
        .replace("      auth: required\n", "      auth: optional\n");
    load(&yaml).expect("optional auth needs no usable AUTH");
}

#[test]
fn a_removed_key_says_what_replaced_it() {
    // `deny_unknown_fields` would refuse both anyway, with serde's bare "unknown
    // field". A config that worked yesterday deserves the reason.
    let yaml = BASE.replace(
        "  listeners:\n    - address: \"127.0.0.1:25\"\n      auth: required\n",
        "  listen: \"127.0.0.1:25\"\n",
    );
    rejected_for(&yaml, "replaced by server.listeners");

    let yaml = BASE.replace(
        "    allow_insecure_auth: true",
        "    required: true\n    allow_insecure_auth: true",
    );
    rejected_for(&yaml, "replaced by each listener's");
}

// -- §5.1 listeners (D-070) ------------------------------------------------

#[test]
fn rejects_no_listeners() {
    let yaml = BASE.replace(
        "  listeners:\n    - address: \"127.0.0.1:25\"\n      auth: required\n",
        "  listeners: []\n",
    );
    rejected_for(&yaml, "accept no mail at all");
}

#[test]
fn rejects_an_invalid_or_duplicated_listener_address() {
    let yaml = BASE.replace("- address: \"127.0.0.1:25\"", "- address: \"nonsense\"");
    rejected_for(&yaml, "not a valid host:port");

    let yaml = BASE.replace(
        "    - address: \"127.0.0.1:25\"\n      auth: required\n",
        "    - address: \"127.0.0.1:25\"\n      auth: required\n    - address: \"127.0.0.1:25\"\n",
    );
    rejected_for(&yaml, "duplicates server.listeners[0]");
}

#[test]
fn an_unknown_listener_mode_is_a_parse_failure() {
    let yaml = BASE.replace(
        "      auth: required\n",
        "      auth: required\n      tls: sometimes\n",
    );
    assert!(matches!(load(&yaml), Err(LoadError::Parse { .. })));
}

#[test]
fn rejects_a_tls_listener_with_no_certificate() {
    let yaml = BASE.replace(
        "      auth: required\n",
        "      auth: required\n      tls: starttls\n",
    );
    rejected_for(&yaml, "server.tls names no certificate");
}

#[test]
fn port_defaults_follow_the_rfcs_and_are_checked_like_explicit_values() {
    // 587 with nothing else said is RFC 6409 submission: starttls_required and
    // auth required. Without a certificate that is a violation, and the message
    // says the value came from the default, which is the part an operator would
    // otherwise not guess.
    let yaml = BASE.replace(
        "- address: \"127.0.0.1:25\"",
        "- address: \"127.0.0.1:587\"",
    );
    rejected_for(&yaml, "starttls_required (the default for this port)");

    let cfg = load(BASE).unwrap();
    let l = &cfg.server.listeners[0];
    assert_eq!(l.tls_mode(), config::IngressTls::Off);
    assert_eq!(l.auth_mode(), config::IngressAuth::Required);
}

#[test]
fn defaults_by_port() {
    use config::{IngressAuth as A, IngressTls as T, ListenerConfig};
    let at = |address: &str| ListenerConfig {
        address: address.to_string(),
        tls: None,
        auth: None,
    };
    assert_eq!(
        (at("0.0.0.0:25").tls_mode(), at("0.0.0.0:25").auth_mode()),
        (T::Off, A::Optional)
    );
    assert_eq!(
        (at("0.0.0.0:587").tls_mode(), at("0.0.0.0:587").auth_mode()),
        (T::StarttlsRequired, A::Required)
    );
    assert_eq!(
        (at("0.0.0.0:465").tls_mode(), at("0.0.0.0:465").auth_mode()),
        (T::Implicit, A::Required)
    );
    assert_eq!(
        (
            at("0.0.0.0:2525").tls_mode(),
            at("0.0.0.0:2525").auth_mode()
        ),
        (T::Off, A::Optional)
    );
    // An explicit value always wins over the port.
    let explicit = ListenerConfig {
        address: "0.0.0.0:465".into(),
        tls: Some(T::Off),
        auth: Some(A::Disabled),
    };
    assert_eq!(
        (explicit.tls_mode(), explicit.auth_mode()),
        (T::Off, A::Disabled)
    );
}

#[test]
fn a_missing_certificate_file_is_reported_with_everything_else() {
    // §4.2's "report all violations": the certificate problem arrives in the
    // same list as an unrelated routing fault, not after fixing it.
    let yaml = BASE
        .replace("      auth: required\n", "      auth: required\n      tls: starttls\n")
        .replace(
            "  auth:\n    allow_insecure_auth",
            "  tls: { certificate: \"/nonexistent/c.pem\", private_key: \"/nonexistent/k.pem\" }\n  auth:\n    allow_insecure_auth",
        )
        .replace("chain: [warming, overflow]", "chain: [warming, nonesuch]");
    match load(&yaml) {
        Err(LoadError::Invalid(v)) => {
            assert!(v.mentions("server.tls.certificate"), "{v}");
            assert!(v.mentions("server.tls.private_key"), "{v}");
            assert!(v.mentions("nonesuch"), "{v}");
        }
        other => panic!("expected violations, got {:?}", other.map(|_| ())),
    }
}

// -- §5.3 grants (D-071) ---------------------------------------------------

#[test]
fn a_user_without_grants_is_a_parse_failure() {
    // Required, not defaulted: default deny with no grants is a user who can log
    // in and send nothing.
    let yaml = BASE.replace("        grants: { send_as: [\"oldbrand.com\"] }\n", "");
    assert!(matches!(load(&yaml), Err(LoadError::Parse { .. })));
}

#[test]
fn rejects_an_empty_grant_list() {
    let yaml = BASE.replace("send_as: [\"oldbrand.com\"]", "send_as: []");
    rejected_for(&yaml, "could authenticate and then send nothing");
}

#[test]
fn an_unknown_capability_is_refused_not_ignored() {
    // Where Simmer differs from Slater on purpose (D-071): read once at startup,
    // a misspelt grant that silently grants nothing is D-013's failure mode.
    let yaml = BASE.replace(
        "grants: { send_as: [\"oldbrand.com\"] }",
        "grants: { send_as: [\"oldbrand.com\"], sendas: [\"x.com\"] }",
    );
    assert!(matches!(load(&yaml), Err(LoadError::Parse { .. })));
}

#[test]
fn rejects_grant_patterns_that_can_never_mean_what_they_say() {
    for (pattern, needle) in [
        ("\"\"", "is empty"),
        ("\"old brand.com\"", "whitespace"),
        ("\"*\"", "every sender identity"),
        ("\"*.x@y.com\"", "not a pattern"),
        ("\"sales*@x.com\"", "'*' somewhere other than"),
        ("\"@x.com\"", "not a full address"),
        ("\"sales@\"", "not a full address"),
    ] {
        let yaml = BASE.replace(
            "send_as: [\"oldbrand.com\"]",
            &format!("send_as: [\"oldbrand.com\", {pattern}]"),
        );
        rejected_for(&yaml, needle);
    }
}

#[test]
fn accepts_all_three_section_5_4_forms_as_grants() {
    let yaml = BASE.replace(
        "send_as: [\"oldbrand.com\"]",
        "send_as: [\"oldbrand.com\", \"*.oldbrand.com\", \"marketing@newbrand.com\"]",
    );
    load(&yaml).expect("§5.4's three forms are all valid grants");
}

// -- §9.3 admin credentials (D-053) --------------------------------------

#[test]
fn accepts_named_admin_tokens_alongside_the_scalar() {
    // D-053 — `auth_token` keeps working exactly as §4.1 specifies, and named
    // tokens are additive.
    let yaml = BASE.replace(
        "  auth_token: \"tok\"",
        "  auth_token: \"tok\"\n  tokens:\n    - { name: oncall, token: \"aaaa\" }\n    - { name: deploybot, token: \"bbbb\" }",
    );
    let cfg = load(&yaml).expect("named tokens are valid");
    assert_eq!(
        cfg.admin.credentials(),
        vec![
            ("default", "tok"),
            ("oncall", "aaaa"),
            ("deploybot", "bbbb")
        ]
    );
}

#[test]
fn rejects_a_configuration_with_no_admin_credential_at_all() {
    // "No token configured" must never mean "no token required" — the direction
    // that mistake usually goes.
    let yaml = BASE.replace("  auth_token: \"tok\"\n", "");
    rejected_for(&yaml, "no admin credential");
}

#[test]
fn rejects_two_admin_tokens_sharing_a_name() {
    let yaml = BASE.replace(
        "  auth_token: \"tok\"",
        "  tokens:\n    - { name: oncall, token: \"aaaa\" }\n    - { name: oncall, token: \"bbbb\" }",
    );
    rejected_for(&yaml, "duplicates");
}

#[test]
fn rejects_two_admin_tokens_sharing_a_secret() {
    // §9.3 identifies the actor by the token presented, so two names behind one
    // secret make the audit line a coin flip.
    let yaml = BASE.replace(
        "  auth_token: \"tok\"",
        "  auth_token: \"same\"\n  tokens:\n    - { name: oncall, token: \"same\" }",
    );
    rejected_for(&yaml, "shares its token");
}

#[test]
fn rejects_an_empty_admin_token() {
    let yaml = BASE.replace(
        "  auth_token: \"tok\"",
        "  tokens:\n    - { name: oncall, token: \"\" }",
    );
    rejected_for(&yaml, "must not be empty");
}

// -- §6.7: preflight warnings (phase 8) ----------------------------------

#[test]
fn a_route_whose_identity_domain_is_not_a_literal_is_refused() {
    // D-069, and it inverts what phase 8 did here. This configuration used to
    // load with a warning that preflight could not check it; §4.2 now refuses it
    // outright, because the problem is not that we cannot check the domain — it
    // is that a route whose domain varies per message warms nothing. The ramp,
    // the allowance and the quota row are all keyed on there being one domain.
    let yaml = BASE.replace(
        r#"envelope_from: "bounce@newbrand.com""#,
        r#"envelope_from: "bounce@{{original.envelope_from.domain}}""#,
    );
    rejected_for(&yaml, "domain that is not a literal");
}

#[test]
fn the_literal_domain_rule_is_about_the_domain_and_not_the_local_part() {
    // The rule has to be exactly this narrow. A templated *local* part is a
    // different question — §6.6's stability rule owns it, and rejects the
    // relative ones — so this rule must not quietly take that decision too.
    // `{{uuid}}` in a local part is stable under §6.6's probe and legitimate.
    let yaml = BASE.replace(
        r#"envelope_from: "bounce@newbrand.com""#,
        r#"envelope_from: "bounce+{{uuid}}@newbrand.com""#,
    );
    let cfg = load(&yaml).expect("a constant domain is all §4.2 asks for");
    assert_eq!(
        cfg.default_ramp()
            .route("warming")
            .unwrap()
            .identity
            .envelope_from,
        "bounce+{{uuid}}@newbrand.com"
    );
}

#[test]
fn a_templated_subdomain_is_refused_too() {
    // The failure this catches is a multi-tenant shape — one route, a domain per
    // tenant — which is the case where "the ledger looks healthy while nothing is
    // being warmed" is most likely to go unnoticed.
    let yaml = BASE.replace(
        r#"envelope_from: "bounce@newbrand.com""#,
        r#"envelope_from: "bounce@mail.{{original.from.domain}}.com""#,
    );
    rejected_for(&yaml, "domain that is not a literal");
}

#[test]
fn warns_when_preflight_strict_is_on_the_last_link_of_a_chain() {
    // D-052's reasoning applied to §6.7: on the last link there is nothing to
    // steer to, so a failing check answers 451 instead of routing around itself —
    // the one outcome §6.7's non-blocking default exists to avoid.
    let yaml = BASE.replace(
        "  - name: overflow\n    overflow: true",
        "  - name: overflow\n    overflow: true\n    preflight: { enabled: true, spf_include: \"a.b\", dkim_selector: \"s1\", strict: true }",
    );
    let cfg = load(&yaml).expect("a warning, not a violation");
    let warnings = config::validate::warnings(&cfg);

    assert!(
        warnings
            .iter()
            .any(|w| w.message.contains("preflight.strict") && w.message.contains("451")),
        "expected a last-link strict warning naming the consequence, got: {warnings:?}"
    );
}

#[test]
fn a_preflighted_route_with_a_constant_domain_warns_about_nothing() {
    // The base fixture already preflights `newbrand.com`, which is the ordinary
    // case: no warning should be produced for it, or the two above are noise.
    let cfg = load(BASE).expect("valid");
    let warnings = config::validate::warnings(&cfg);

    assert!(
        !warnings.iter().any(|w| w.path.contains("preflight")),
        "the ordinary case must be silent, got: {warnings:?}"
    );
}

#[test]
fn warns_about_a_short_admin_token_without_refusing_it() {
    // A judgement rather than a rule: §4.1 sets no length, and the base fixture
    // has been using "tok" since phase 1.
    let cfg = load(BASE).expect("valid");
    let warnings = config::validate::warnings(&cfg);
    assert!(
        warnings
            .iter()
            .any(|w| w.path == "admin.auth_token" && w.message.contains("451")),
        "expected a short-token warning naming the consequence, got: {warnings:?}"
    );
}

// -- §4.2: body rewrites -------------------------------------------------

#[test]
fn rejects_a_body_rewrite_pattern_that_does_not_compile() {
    let yaml = BASE.replace(
        r#"      unstable_headers: ["Reply-To"]"#,
        "      unstable_headers: [\"Reply-To\"]\n      body_rewrites:\n        - pattern: '[unclosed'\n          replacement: 'x'",
    );
    rejected_for(&yaml, "does not compile");
}

#[test]
fn rejects_body_rewrites_that_are_not_a_fixed_point() {
    // D-046. §6.6 applied to the body: a rule whose pattern matches its own
    // replacement grows the message every time it passes through, and corrupts
    // traffic from an application that has already been cut over. The body is
    // not a header, so `unstable_headers` cannot excuse it and nothing does.
    let yaml = BASE.replace(
        r#"      unstable_headers: ["Reply-To"]"#,
        "      unstable_headers: [\"Reply-To\"]\n      body_rewrites:\n        - pattern: 'https://newbrand\\.com/x'\n          replacement: 'https://newbrand.com/x?ref=1'",
    );
    rejected_for(&yaml, "are not stable");
}

#[test]
fn accepts_body_rewrites_where_one_rule_consumes_anothers_output() {
    // The false positive the check has to avoid: applying the *chain* twice is a
    // no-op even though rule 2 reads what rule 1 wrote.
    let yaml = BASE.replace(
        r#"      unstable_headers: ["Reply-To"]"#,
        "      unstable_headers: [\"Reply-To\"]\n      body_rewrites:\n        - pattern: 'oldbrand\\.example'\n          replacement: 'interim.example'\n        - pattern: 'interim\\.example'\n          replacement: 'newbrand.example'",
    );
    load(&yaml).expect("a chain that settles should be accepted");
}

#[test]
fn accepts_a_valid_body_rewrite_pattern() {
    let yaml = BASE.replace(
        r#"      unstable_headers: ["Reply-To"]"#,
        "      unstable_headers: [\"Reply-To\"]\n      body_rewrites:\n        - pattern: 'https://oldbrand\\.com/'\n          replacement: 'https://newbrand.com/'",
    );
    load(&yaml).expect("a valid pattern should be accepted");
}

// -- §6.6 / §4.2: unstable_headers ---------------------------------------

#[test]
fn rejects_an_identity_field_named_in_unstable_headers() {
    // §6.6: identity-field stability is not overridable. Naming one here is a
    // fatal configuration error, and this half of the rule is purely syntactic
    // so it is enforced from phase 1.
    let yaml = BASE.replace(
        r#"unstable_headers: ["Reply-To"]"#,
        r#"unstable_headers: ["From"]"#,
    );
    rejected_for(&yaml, "not overridable");
}

#[test]
fn rejects_envelope_from_named_in_unstable_headers() {
    let yaml = BASE.replace(
        r#"unstable_headers: ["Reply-To"]"#,
        r#"unstable_headers: ["envelope_from"]"#,
    );
    rejected_for(&yaml, "not overridable");
}

#[test]
fn warns_about_a_stale_unstable_header_declaration_rather_than_rejecting_it() {
    // §6.6: "Naming a header that is in fact stable is also a startup WARN — it
    // means either the declaration is stale or the intent was misunderstood, and
    // both are worth surfacing." A header the route never sets is stable by
    // definition, so it belongs here and not in the violation list.
    //
    // Phase 1 rejected it outright, having no stability engine to tell a stale
    // declaration from a live one. Phase 4 does, so the spec's severity applies.
    let yaml = BASE.replace(
        r#"unstable_headers: ["Reply-To"]"#,
        r#"unstable_headers: ["Reply-To", "X-Never-Set"]"#,
    );
    let cfg = load(&yaml).expect("a stale declaration is a warning, not a violation");
    let warnings = config::validate::warnings(&cfg);
    assert!(
        warnings
            .iter()
            .any(|w| w.message.contains("X-Never-Set") && w.message.contains("stale")),
        "expected a stale-declaration warning, got: {warnings:?}"
    );
}

#[test]
fn warns_when_a_declared_header_turns_out_to_be_stable() {
    // The other half of the same §6.6 sentence: the header *is* set, but its
    // template reads nothing this route writes, so declaring it achieves nothing.
    let yaml = BASE.replace(
        r#"Reply-To: "{{original.from.address}}""#,
        r#"Reply-To: "support@newbrand.com""#,
    );
    let cfg = load(&yaml).expect("a stable declared header is a warning, not a violation");
    let warnings = config::validate::warnings(&cfg);
    assert!(
        warnings
            .iter()
            .any(|w| w.message.contains("Reply-To") && w.message.contains("stale")),
        "expected a declared-but-stable warning, got: {warnings:?}"
    );
}

#[test]
fn warns_when_max_recipients_is_set_above_one() {
    // D-047 — the key stays because §4.1 mandates it, but no value above 1 is
    // reachable. BASE says 100, which is §4.1's own example.
    let cfg = load(BASE).expect("valid");
    let warnings = config::validate::warnings(&cfg);
    assert!(
        warnings
            .iter()
            .any(|w| w.path == "server.max_recipients" && w.message.contains("no effect")),
        "expected a vestigial-max_recipients warning, got: {warnings:?}"
    );
}

#[test]
fn warns_when_the_last_route_in_a_chain_carries_a_frequency_constraint() {
    // §7.3 steers to the *next* link. On the last link there is no next link, so
    // a recipient over threshold gets §10.3's `451` instead of another route —
    // legitimate, but far more often a mistake, and invisible until someone
    // reaches the threshold.
    let yaml = BASE.replace(
        "  - name: overflow\n    overflow: true",
        "  - name: overflow\n    overflow: true\n    recipient_frequency:\n      \
         mode: to_address\n      window: { unit: daily, count: 1 }\n      threshold: 3",
    );
    let cfg = load(&yaml).expect("a constraint on the last link is a warning, not a violation");
    let warnings = config::validate::warnings(&cfg);
    assert!(
        warnings
            .iter()
            .any(|w| w.message.contains("nothing to fall through to")),
        "expected a last-link frequency warning, got: {warnings:?}"
    );
}

#[test]
fn does_not_warn_when_the_constraint_is_on_a_route_with_something_after_it() {
    // The normal arrangement: the warming route is constrained and the overflow
    // route catches what it turns away.
    let cfg = load(BASE).expect("valid");
    let warnings = config::validate::warnings(&cfg);
    assert!(
        !warnings
            .iter()
            .any(|w| w.message.contains("nothing to fall through to")),
        "unexpected last-link warning: {warnings:?}"
    );
}

// -- §6.6 / §4.2: the stability property itself --------------------------

#[test]
fn rejects_an_unstable_identity_field_with_no_override_available() {
    // §6.6: "A stability violation here is a fatal startup error with no
    // override." Sender: reads From:, which the same pass overwrites.
    let yaml = BASE
        .replace(
            r#"        Reply-To: "{{original.from.address}}""#,
            r#"        Sender: "{{original.from.address}}""#,
        )
        .replace(r#"      unstable_headers: ["Reply-To"]"#, "");
    rejected_for(&yaml, "not overridable");
}

#[test]
fn declaring_an_unstable_identity_field_does_not_rescue_it() {
    // Belt and braces: the syntactic rule already rejects naming an identity
    // field in unstable_headers, and the property would reject it anyway.
    let yaml = BASE
        .replace(
            r#"        Reply-To: "{{original.from.address}}""#,
            r#"        Sender: "{{original.from.address}}""#,
        )
        .replace(
            r#"      unstable_headers: ["Reply-To"]"#,
            r#"      unstable_headers: ["Sender"]"#,
        );
    rejected_for(&yaml, "not overridable");
}

#[test]
fn rejects_an_undeclared_unstable_header() {
    let yaml = BASE.replace(r#"      unstable_headers: ["Reply-To"]"#, "");
    rejected_for(&yaml, "unstable_headers");
}

#[test]
fn accepts_the_same_header_once_it_is_declared() {
    // BASE itself is this case: Reply-To reads From:, which the route rewrites,
    // and Reply-To is declared. §6.6's canonical migration-only construct.
    load(BASE).expect("the shipped idiom must validate");
}

#[test]
fn rejects_an_unstable_envelope_sender() {
    let yaml = BASE.replace(
        r#"envelope_from: "bounce@newbrand.com""#,
        r#"envelope_from: "{{original.envelope_from.local}}-x@newbrand.com""#,
    );
    rejected_for(&yaml, "not overridable");
}

// -- D-034: unknown template variables -----------------------------------

#[test]
fn rejects_an_unknown_template_variable() {
    // Not a §4.2 rule; D-034. A typo in an identity template would otherwise
    // emit a broken From: for as long as nobody looked.
    let yaml = BASE.replace("{{original.from.address}}", "{{original.frm.address}}");
    rejected_for(&yaml, "unknown template variable");
}

#[test]
fn names_the_key_a_bad_template_came_from() {
    let yaml = BASE.replace("{{original.from.address}}", "{{original.frm.address}}");
    rejected_for(&yaml, "set_headers.Reply-To");
}

// -- §4.2: default_chain -------------------------------------------------

#[test]
fn rejects_a_missing_default_chain_when_senders_are_not_strict() {
    let yaml = BASE.replace("  default_chain: [overflow]\n", "");
    rejected_for(&yaml, "default_chain");
}

#[test]
fn rejects_a_default_chain_not_ending_in_an_overflow_route() {
    let yaml = BASE.replace("default_chain: [overflow]", "default_chain: [warming]");
    rejected_for(&yaml, "is not an overflow route");
}

#[test]
fn accepts_a_missing_default_chain_when_senders_are_strict() {
    let yaml = BASE
        .replace("  default_chain: [overflow]\n", "")
        .replace("strict_senders: false", "strict_senders: true");
    load(&yaml).expect("strict_senders makes default_chain unnecessary");
}

// -- unknown keys --------------------------------------------------------

#[test]
fn rejects_an_unknown_key() {
    // §2.2 says there is no hot reload, so a typo'd key would otherwise sit in
    // the file until someone wondered why a setting had no effect. For a
    // component whose job is to not exceed a limit, "the limit you set was
    // ignored" is the worst available failure mode.
    let yaml = BASE.replace(
        "  max_recipients: 100",
        "  max_recipients: 100\n  max_recipent: 5",
    );
    match load(&yaml) {
        Err(LoadError::Parse { .. }) => {}
        other => panic!("expected a parse error for an unknown key, got: {other:?}"),
    }
}

#[test]
fn rejects_a_removed_key_by_name() {
    // D-047 removed `single_recipient_only`. `deny_unknown_fields` would already
    // refuse it, but with a bare "unknown field" — and this is a key that worked
    // in a previous version, so the operator gets told which decision took it
    // away and what to do instead.
    let yaml = BASE.replace(
        "  max_recipients: 100",
        "  max_recipients: 100\n  single_recipient_only: false",
    );
    match load(&yaml) {
        Err(LoadError::Invalid(v)) => {
            assert!(v.mentions("single_recipient_only"), "{v}");
            assert!(v.mentions("D-047"), "{v}");
        }
        other => panic!("expected a named violation for a removed key, got: {other:?}"),
    }
}

// -- the one that matters ------------------------------------------------

#[test]
fn reports_every_violation_not_just_the_first() {
    // §4.2: "Report all violations, not just the first."
    let yaml = BASE
        .replace("chain: [warming, overflow]", "chain: [overflow, warming]") // overflow not last
        .replace("default: [50, 100, 200]", "default: []") // empty schedule
        .replace(
            "allowed_cidrs: [\"10.0.0.0/8\"]",
            "allowed_cidrs: [\"not-a-cidr\"]",
        ) // bad CIDR
        .replace(
            r#"{ name: catchall, domains: ["*"] }"#,
            r#"{ name: catchall, domains: ["x.com"] }"#,
        ); // no catch-all

    let Err(LoadError::Invalid(v)) = load(&yaml) else {
        panic!("expected a validation failure");
    };

    assert!(
        v.mentions("must be last"),
        "missing overflow-position violation:\n{v}"
    );
    assert!(
        v.mentions("must not be empty"),
        "missing empty-schedule violation:\n{v}"
    );
    assert!(
        v.mentions("not a valid CIDR"),
        "missing CIDR violation:\n{v}"
    );
    assert!(
        v.mentions("no group contains '*'"),
        "missing catch-all violation:\n{v}"
    );
    assert!(
        v.len() >= 4,
        "expected at least 4 violations, got {}:\n{v}",
        v.len()
    );
}

#[test]
fn the_violation_report_is_readable() {
    let yaml = BASE.replace("default: [50, 100, 200]", "default: []");
    let Err(e) = load(&yaml) else {
        panic!("expected failure");
    };
    let rendered = e.to_string();
    // The operator needs the path, not just the complaint.
    assert!(
        rendered.contains("routes.warming.warmup.schedule.default"),
        "report does not name the offending path:\n{rendered}"
    );
}

// -- D-083: link_proxy ----------------------------------------------------

fn with_link_proxy(block: &str) -> String {
    format!("{BASE}link_proxy:\n{block}")
}

#[test]
fn link_proxy_is_optional_and_absent_by_default() {
    assert!(load(BASE).unwrap().link_proxy.is_none());
}

#[test]
fn accepts_a_link_proxy_with_defaults_and_with_a_path_prefix() {
    for upstream in [
        "https://link.domain2.com",
        "https://link.domain2.com/",
        "https://link.domain2.com/tracking",
        "http://10.0.0.9:8080/a/b/",
    ] {
        let cfg = load(&with_link_proxy(&format!(
            "  listen: \"0.0.0.0:80\"\n  upstream: \"{upstream}\"\n  allowed_cidrs: [\"10.0.0.0/8\"]\n"
        )))
        .unwrap_or_else(|e| panic!("{upstream}: {e}"));
        let lp = cfg.link_proxy.unwrap();
        assert_eq!(lp.public_scheme.as_str(), "https");
        assert_eq!(lp.max_request_bytes, 1024 * 1024);
        assert_eq!(lp.max_connections, 512);
        assert_eq!(
            lp.timeouts.upstream_response,
            std::time::Duration::from_secs(30)
        );
    }
}

#[test]
fn rejects_an_unusable_link_proxy_upstream() {
    for (upstream, needle) in [
        ("link.domain2.com", "absolute URI"),
        ("ftp://link.domain2.com", "http or https"),
        ("https://user:pw@link.domain2.com", "credentials"),
        ("https://link.domain2.com/t?x=1", "query"),
        ("https://link.domain2.com/t#frag", "fragment"),
        ("https:///path", "link_proxy.upstream"),
    ] {
        rejected_for(
            &with_link_proxy(&format!(
                "  listen: \"0.0.0.0:80\"\n  upstream: \"{upstream}\"\n  allowed_cidrs: [\"10.0.0.0/8\"]\n"
            )),
            needle,
        );
    }
}

#[test]
fn rejects_a_link_proxy_listen_that_clashes_with_another_listener() {
    rejected_for(
        &with_link_proxy(
            "  listen: \"127.0.0.1:25\"\n  upstream: \"https://l.example\"\n  allowed_cidrs: [\"10.0.0.0/8\"]\n",
        ),
        "duplicates server.listeners[0]",
    );
    rejected_for(
        &with_link_proxy(
            "  listen: \"127.0.0.1:8080\"\n  upstream: \"https://l.example\"\n  allowed_cidrs: [\"10.0.0.0/8\"]\n",
        ),
        "duplicates admin.listen",
    );
}

#[test]
fn reports_every_link_proxy_violation_at_once() {
    let yaml = with_link_proxy(
        "  listen: \"nowhere\"\n  upstream: \"ftp://x\"\n  allowed_cidrs: [\"not-a-cidr\"]\n  \
         max_connections: 0\n  max_request_bytes: 0\n  timeouts: { header_read: 0s }\n",
    );
    for needle in [
        "link_proxy.listen",
        "link_proxy.upstream",
        "link_proxy.allowed_cidrs[0]",
        "link_proxy.max_connections",
        "link_proxy.max_request_bytes",
        "link_proxy.timeouts.header_read",
    ] {
        rejected_for(&yaml, needle);
    }
    rejected_for(
        &with_link_proxy(
            "  listen: \"0.0.0.0:80\"\n  upstream: \"https://l.example\"\n  allowed_cidrs: []\n",
        ),
        "link_proxy.allowed_cidrs",
    );
}

#[test]
fn rejects_an_unknown_link_proxy_key() {
    assert!(load(&with_link_proxy(
        "  listen: \"0.0.0.0:80\"\n  upstream: \"https://l.example\"\n  allowed_cidrs: [\"10.0.0.0/8\"]\n  upstream_host: x\n"
    ))
    .is_err());
}

#[test]
fn warns_about_a_plaintext_link_proxy_upstream() {
    let cfg = load(&with_link_proxy(
        "  listen: \"0.0.0.0:80\"\n  upstream: \"http://l.example\"\n  allowed_cidrs: [\"10.0.0.0/8\"]\n",
    ))
    .unwrap();
    assert!(config::validate::warnings(&cfg)
        .iter()
        .any(|w| w.path == "link_proxy.upstream"));
}

// -- D-085: capture -------------------------------------------------------

fn with_capture(block: &str) -> String {
    format!("{BASE}capture:\n{block}")
}

/// A `capture:` block naming a directory that really exists and is really
/// writable, because `check_capture` probes it rather than reading mode bits.
fn capture_in(dir: &std::path::Path, extra: &str) -> String {
    with_capture(&format!("  directory: \"{}\"\n{extra}", dir.display()))
}

#[test]
fn capture_is_optional_and_absent_by_default() {
    assert!(load(BASE).unwrap().capture.is_none());
}

#[test]
fn a_capture_block_needs_only_a_directory_and_defaults_the_rest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = load(&capture_in(dir.path(), "")).expect("should load");
    let c = cfg.capture.expect("capture");
    assert_eq!(c.max_body_bytes, 1024 * 1024);
    assert_eq!(c.retention, std::time::Duration::from_secs(24 * 3600));
    assert_eq!(c.on_error, config::CaptureOnError::Continue);
    assert_eq!(c.queue_depth, 1024);
    assert_eq!(c.max_queue_bytes, 64 * 1024 * 1024);
}

#[test]
fn a_capture_directory_that_does_not_exist_yet_is_accepted_if_its_parent_is_writable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let child = dir.path().join("not-created-yet");
    assert!(load(&capture_in(&child, "")).is_ok());
}

#[test]
fn rejects_a_relative_capture_directory() {
    // It would resolve against the working directory — `/app` in the container,
    // wherever the operator stood otherwise — so one config would capture to two
    // different places.
    rejected_for(&with_capture("  directory: \"capture\"\n"), "absolute");
}

#[test]
fn rejects_an_empty_capture_directory() {
    rejected_for(&with_capture("  directory: \"\"\n"), "capture.directory");
}

#[test]
fn rejects_a_capture_directory_that_is_a_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a-file");
    std::fs::write(&file, b"not a directory").expect("write");
    rejected_for(&capture_in(&file, ""), "not a directory");
}

#[test]
fn rejects_a_capture_directory_whose_parent_does_not_exist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let deep = dir.path().join("missing").join("deeper");
    rejected_for(&capture_in(&deep, ""), "neither does its parent");
}

#[test]
fn rejects_a_zero_max_body_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    rejected_for(
        &capture_in(dir.path(), "  max_body_bytes: 0\n"),
        "capture.max_body_bytes",
    );
}

#[test]
fn rejects_a_retention_shorter_than_one_bucket() {
    // A retention under ten minutes would have the sweeper delete the file the
    // writer is appending to.
    let dir = tempfile::tempdir().expect("tempdir");
    rejected_for(
        &capture_in(dir.path(), "  retention: 5m\n"),
        "capture.retention",
    );
    assert!(load(&capture_in(dir.path(), "  retention: 10m\n")).is_ok());
}

#[test]
fn rejects_a_zero_queue_depth_and_a_zero_queue_byte_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    rejected_for(
        &capture_in(dir.path(), "  queue_depth: 0\n"),
        "capture.queue_depth",
    );
    rejected_for(
        &capture_in(dir.path(), "  max_queue_bytes: 0\n"),
        "capture.max_queue_bytes",
    );
}

#[test]
fn rejects_a_queue_byte_budget_that_could_never_hold_a_maximum_size_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    rejected_for(
        &capture_in(dir.path(), "  max_queue_bytes: 1024\n"),
        "max_message_bytes",
    );
}

#[test]
fn rejects_an_unparseable_capture_on_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(load(&capture_in(dir.path(), "  on_error: maybe\n")).is_err());
    assert!(load(&capture_in(dir.path(), "  on_error: defer\n")).is_ok());
}

#[test]
fn rejects_an_unknown_capture_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(load(&capture_in(dir.path(), "  fsync: true\n")).is_err());
}

#[test]
fn rejects_a_bare_integer_capture_retention() {
    // The house rule: a duration carries its unit (`config::duration`).
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(load(&capture_in(dir.path(), "  retention: 3600\n")).is_err());
}

#[test]
fn reports_every_capture_violation_at_once() {
    let yaml = with_capture(
        "  directory: \"relative/path\"\n  max_body_bytes: 0\n  retention: 1m\n  \
         queue_depth: 0\n  max_queue_bytes: 0\n",
    );
    match load(&yaml) {
        Err(LoadError::Invalid(v)) => {
            for needle in [
                "capture.directory",
                "capture.max_body_bytes",
                "capture.retention",
                "capture.queue_depth",
                "capture.max_queue_bytes",
            ] {
                assert!(
                    v.mentions(needle),
                    "no violation mentioned '{needle}':\n{v}"
                );
            }
        }
        other => panic!("expected a validation failure, got: {other:?}"),
    }
}

#[test]
fn warns_whenever_capture_is_enabled_at_all() {
    // Not conditional on anything: §7.3 hashes recipients precisely so the
    // container does not accumulate a plaintext record of every address mailed,
    // and this accumulates exactly that. An operator who left it on after an
    // afternoon of debugging must see it on every start.
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = load(&capture_in(dir.path(), "")).unwrap();
    let warnings = config::validate::warnings(&cfg);
    let w = warnings
        .iter()
        .find(|w| w.path == "capture")
        .unwrap_or_else(|| panic!("no capture warning. Got: {warnings:?}"));
    assert!(w.message.contains("7.3"), "{}", w.message);
    assert!(w.message.contains(&dir.path().display().to_string()));
}

#[test]
fn does_not_warn_about_capture_when_it_is_off() {
    let cfg = load(BASE).unwrap();
    assert!(!config::validate::warnings(&cfg)
        .iter()
        .any(|w| w.path.starts_with("capture")));
}

// -- D-089: header_rewrites ------------------------------------------------

/// BASE's warming route with a `header_rewrites` block appended.
fn with_header_rewrites(block: &str) -> String {
    BASE.replace(
        r#"      unstable_headers: ["Reply-To"]"#,
        &format!("      unstable_headers: [\"Reply-To\"]\n      header_rewrites:\n{block}"),
    )
}

const UNSUB_REWRITE: &str = "        - header: List-Unsubscribe
          pattern: '<https://www\\.meddoc\\.net/'
          replacement: '<https://link-pmps.healthcarematch.com/'\n";

#[test]
fn accepts_the_list_unsubscribe_host_rewrite() {
    let cfg = load(&with_header_rewrites(UNSUB_REWRITE)).expect("should be accepted");
    assert!(!config::validate::warnings(&cfg)
        .iter()
        .any(|w| w.path.contains("header_rewrites")));
}

#[test]
fn rejects_a_header_rewrite_of_every_identity_field() {
    // The treatment envelope_from gets: refused, and nothing overrides it.
    for field in ["From", "Sender", "Message-ID", "from"] {
        let yaml = with_header_rewrites(&format!(
            "        - header: {field}\n          pattern: 'oldbrand'\n          replacement: 'newbrand'\n"
        ));
        rejected_for(&yaml, &format!("names identity field '{field}'"));
    }
}

#[test]
fn declaring_an_identity_field_unstable_does_not_admit_a_header_rewrite_of_it() {
    // §6.6 refuses the declaration itself, so there is no route to admission —
    // and the rewrite is refused in its own right, not only via the declaration.
    let yaml = with_header_rewrites(
        "        - header: Sender\n          pattern: 'old'\n          replacement: 'new'\n",
    )
    .replace(
        r#"unstable_headers: ["Reply-To"]"#,
        r#"unstable_headers: ["Reply-To", "Sender"]"#,
    );
    match load(&yaml) {
        Err(LoadError::Invalid(v)) => {
            assert!(v.mentions("header_rewrites[0].header"), "{v}");
            assert!(v.mentions("unstable_headers"), "{v}");
        }
        other => panic!("expected rejection, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn rejects_header_rewrites_that_are_not_a_fixed_point() {
    let yaml = with_header_rewrites(
        "        - header: List-Unsubscribe\n          pattern: 'meddoc\\.net'\n          replacement: 'meddoc.net.proxy.example'\n",
    );
    rejected_for(&yaml, "are not stable");
}

#[test]
fn declaring_an_unstable_header_rewrite_does_not_rescue_it() {
    // D-046's reasoning: no migration-only reading of a rule that re-matches
    // its own output, so unstable_headers is not an escape hatch for it.
    let yaml = with_header_rewrites(
        "        - header: List-Unsubscribe\n          pattern: 'meddoc\\.net'\n          replacement: 'meddoc.net.proxy.example'\n",
    )
    .replace(
        r#"unstable_headers: ["Reply-To"]"#,
        r#"unstable_headers: ["Reply-To", "List-Unsubscribe"]"#,
    );
    rejected_for(&yaml, "Not overridable");
}

#[test]
fn rejects_a_header_rewrite_whose_result_would_not_be_conformant() {
    let yaml = with_header_rewrites(
        "        - header: X-A\n          pattern: 'a'\n          replacement: \"b\\r\\nBcc: victim@example.com\"\n",
    );
    rejected_for(&yaml, "RFC 5322");
}

#[test]
fn rejects_a_header_rewrite_with_an_unusable_header_name_or_pattern() {
    let yaml = with_header_rewrites(
        "        - header: 'Bad Name'\n          pattern: 'a'\n          replacement: 'b'\n        - header: X-B\n          pattern: '[unclosed'\n          replacement: 'b'\n",
    );
    match load(&yaml) {
        Err(LoadError::Invalid(v)) => {
            assert!(v.mentions("header_rewrites[0].header"), "{v}");
            assert!(v.mentions("header_rewrites[1].pattern"), "{v}");
        }
        other => panic!("expected rejection, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn a_capture_reference_followed_by_text_says_how_to_write_it() {
    let yaml = with_header_rewrites(
        "        - header: X-A\n          pattern: '(a)'\n          replacement: '$1b'\n",
    );
    rejected_for(&yaml, "write ${1}b");
}

#[test]
fn warns_about_a_header_rewrite_that_can_never_take_effect() {
    // Not a violation — §6.2's order makes each case well defined — but the
    // operator wrote a rule expecting it to fire.
    let yaml = with_header_rewrites(
        "        - header: reply-to\n          pattern: 'a'\n          replacement: 'b'\n        - header: X-Mailer\n          pattern: 'a'\n          replacement: 'b'\n        - header: DKIM-Signature\n          pattern: 'a'\n          replacement: 'b'\n",
    )
    .replace(
        r#"unstable_headers: ["Reply-To"]"#,
        "unstable_headers: [\"Reply-To\"]\n      remove_headers: [\"X-Mailer\"]",
    );
    let cfg = load(&yaml).expect("dead rules warn, they do not refuse");
    let warnings = config::validate::warnings(&cfg);
    for (i, by) in [(0, "set_headers"), (1, "remove_headers"), (2, "§6.5")] {
        assert!(
            warnings
                .iter()
                .any(|w| w.path.ends_with(&format!("header_rewrites[{i}]"))
                    && w.message.contains(by)),
            "no warning for entry {i} naming {by}: {warnings:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// §3.2 step 2a — thread affinity (D-090)
// ---------------------------------------------------------------------------

/// BASE with `thread_affinity` on and each route given a `Message-ID:`.
fn with_affinity(warming_id: &str, overflow_id: &str) -> String {
    let mut yaml = BASE
        .replace(
            "        From: \"Sales <sales@newbrand.com>\"\n",
            &format!("        From: \"Sales <sales@newbrand.com>\"\n        Message-ID: \"{warming_id}\"\n"),
        )
        .replace(
            "        From: \"News <news@mail.established.com>\"\n",
            &format!("        From: \"News <news@mail.established.com>\"\n        Message-ID: \"{overflow_id}\"\n"),
        );
    yaml.push_str("  thread_affinity: true\n");
    yaml
}

#[test]
fn thread_affinity_accepts_routes_with_distinct_literal_message_id_domains() {
    load(&with_affinity(
        "<{{uuid}}@newbrand.com>",
        "<{{uuid}}@mail.established.com>",
    ))
    .expect("valid");
}

#[test]
fn thread_affinity_requires_every_route_to_set_a_message_id() {
    let mut yaml = BASE.to_string();
    yaml.push_str("  thread_affinity: true\n");
    match load(&yaml) {
        Err(LoadError::Invalid(v)) => {
            // §4.2: all of them, not the first.
            assert!(
                v.mentions("routes.warming.identity.set_headers.Message-ID"),
                "{v}"
            );
            assert!(
                v.mentions("routes.overflow.identity.set_headers.Message-ID"),
                "{v}"
            );
            let overflow = v
                .0
                .iter()
                .filter(|x| x.path == "ramps.main.routes.overflow.identity.set_headers.Message-ID")
                .count();
            assert_eq!(
                overflow, 1,
                "reported once, though two chains contain it: {v}"
            );
        }
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn thread_affinity_refuses_a_templated_message_id_domain() {
    rejected_for(
        &with_affinity(
            "<{{uuid}}@{{original.from.domain}}>",
            "<{{uuid}}@mail.established.com>",
        ),
        "routes.warming.identity.set_headers.Message-ID",
    );
}

#[test]
fn thread_affinity_refuses_two_routes_in_a_chain_sharing_a_domain() {
    rejected_for(
        &with_affinity("<{{uuid}}@newbrand.com>", "<{{uuid}}@NewBrand.com>"),
        "both emit Message-IDs at 'newbrand.com'",
    );
}

#[test]
fn without_thread_affinity_a_route_may_pass_the_message_id_through() {
    // The base fixture sets no Message-ID on either route.
    load(BASE).expect("valid");
}

// -- §4.2: warmup.schedule.share (D-091) ----------------------------------

/// `BASE` with a `share` list on the warming route's schedule.
fn with_share(share: &str) -> String {
    BASE.replace(
        "        default: [50, 100, 200]\n",
        &format!("        default: [50, 100, 200]\n        share: {share}\n"),
    )
}

#[test]
fn accepts_a_share_list_followed_by_another_route() {
    let cfg = load(&with_share("[0.1, 0.25, 0.5]")).expect("valid");
    let schedule = &cfg
        .default_ramp()
        .route("warming")
        .unwrap()
        .warmup
        .as_ref()
        .unwrap()
        .schedule;
    assert!(matches!(
        &schedule.share,
        ShareSchedule::Days(days) if *days == vec![0.1, 0.25, 0.5]
    ));

    // Indexed like the caps; past the end, and at 1, every message is offered.
    assert_eq!(schedule.share_for(-1), None);
    assert_eq!(schedule.share_for(0), Some(0.1));
    assert_eq!(schedule.share_for(2), Some(0.5));
    assert_eq!(
        schedule.share_for(3),
        None,
        "not §7.2's 'final value repeats'"
    );

    let ones = load(&with_share("[1.0, 0.5]")).expect("a share of 1 is valid");
    let schedule = &ones
        .default_ramp()
        .route("warming")
        .unwrap()
        .warmup
        .as_ref()
        .unwrap()
        .schedule;
    assert_eq!(schedule.share_for(0), None);
    assert_eq!(schedule.share_for(1), Some(0.5));

    // And absent, or empty, is no partial ramp at all.
    assert!(!load(BASE)
        .unwrap()
        .default_ramp()
        .route("warming")
        .unwrap()
        .warmup
        .as_ref()
        .unwrap()
        .schedule
        .has_partial_ramp());
    load(&with_share("[]")).expect("an empty list is no partial ramp");
}

#[test]
fn rejects_a_share_outside_zero_to_one() {
    for bad in ["0", "0.0", "-0.5", "1.01", "2", ".nan", ".inf"] {
        rejected_for(&with_share(&format!("[0.5, {bad}]")), "schedule.share[1]");
    }
}

#[test]
fn rejects_a_share_route_that_is_last_in_a_chain() {
    let yaml = with_share("[0.5]").replace("chain: [warming, overflow]", "chain: [warming]");
    rejected_for(
        &yaml,
        "has a warmup.schedule.share and is last in the chain",
    );
}

#[test]
fn rejects_a_share_route_that_is_last_in_the_default_chain() {
    // strict_senders so that §4.2's own "default_chain must end in overflow"
    // rule is not what refuses it.
    let yaml = with_share("[0.5]")
        .replace("default_chain: [overflow]", "default_chain: [warming]")
        .replace("strict_senders: false", "strict_senders: true");
    rejected_for(
        &yaml,
        "has a warmup.schedule.share and is last in the chain",
    );
}

#[test]
fn an_empty_share_list_may_be_last() {
    // Nothing is turned away, so there is nothing that needs a next link.
    let yaml = with_share("[]")
        .replace("default_chain: [overflow]", "default_chain: [warming]")
        .replace("strict_senders: false", "strict_senders: true");
    load(&yaml).expect("valid");
}

#[test]
fn share_violations_are_reported_together() {
    let yaml = with_share("[0, 2]").replace("chain: [warming, overflow]", "chain: [warming]");
    match load(&yaml) {
        Err(LoadError::Invalid(v)) => {
            for needle in [
                "schedule.share[0]",
                "schedule.share[1]",
                "is last in the chain",
            ] {
                assert!(v.mentions(needle), "missing '{needle}' in:\n{v}");
            }
        }
        Ok(_) => panic!("accepted"),
        Err(other) => panic!("expected a validation failure, got: {other}"),
    }
}

// -- §4.2: warmup.schedule.share: {mode: auto} (D-097) ---------------------

/// `BASE` with an `auto` share, given as inline YAML parameters.
fn with_auto_share(params: &str) -> String {
    with_share(&format!("{{mode: auto{params}}}"))
}

#[test]
fn accepts_an_auto_share_at_its_defaults() {
    let cfg = load(&with_auto_share("")).expect("valid");
    let schedule = &cfg
        .default_ramp()
        .route("warming")
        .unwrap()
        .warmup
        .as_ref()
        .unwrap()
        .schedule;

    // The shortest form is the whole feature: `mode` and nothing else.
    let auto = schedule.share.auto().expect("auto");
    assert_eq!(auto.floor, AutoShare::DEFAULT_FLOOR);
    assert_eq!(auto.ceiling, AutoShare::DEFAULT_CEILING);
    assert_eq!(auto.gain, AutoShare::DEFAULT_GAIN);
    assert_eq!(auto.fill_by, AutoShare::DEFAULT_FILL_BY);
    assert_eq!(auto.tail.below, Tail::DEFAULT_BELOW);
    assert_eq!(auto.tail.ceiling, Tail::DEFAULT_CEILING);

    assert!(schedule.has_partial_ramp());
    // The share is not a function of the day alone, so the day-indexed accessor
    // reports nothing. `routing::partial` is what resolves it.
    assert_eq!(schedule.share_for(0), None);
    assert_eq!(schedule.share_for(9), None);
}

#[test]
fn accepts_an_auto_share_with_every_parameter_set() {
    let yaml = with_auto_share(
        ", floor: 0.02, ceiling: 0.5, gain: 2.5, fill_by: 0.75,          tail: {below: 0.2, ceiling: 0.8}",
    );
    let cfg = load(&yaml).expect("valid");
    let auto = cfg
        .default_ramp()
        .route("warming")
        .unwrap()
        .warmup
        .as_ref()
        .unwrap()
        .schedule
        .share
        .auto()
        .expect("auto");
    assert_eq!(auto.floor, 0.02);
    assert_eq!(auto.ceiling, 0.5);
    assert_eq!(auto.gain, 2.5);
    assert_eq!(auto.fill_by, 0.75);
    assert_eq!(auto.tail.below, 0.2);
    assert_eq!(auto.tail.ceiling, 0.8);
}

#[test]
fn rejects_an_auto_share_parameter_outside_zero_to_one() {
    for bad in ["0", "-0.5", "1.01", ".nan", ".inf"] {
        for key in ["floor", "ceiling", "fill_by"] {
            rejected_for(
                &with_auto_share(&format!(", {key}: {bad}")),
                &format!("schedule.share.{key}"),
            );
        }
        rejected_for(
            &with_auto_share(&format!(", tail: {{ceiling: {bad}}}")),
            "schedule.share.tail.ceiling",
        );
    }
}

#[test]
fn rejects_a_tail_threshold_that_is_not_a_proportion_of_the_cap() {
    // Zero is allowed — it disables the release — but negative is not, and
    // neither is 1: releasing an empty cap is no ramp at all.
    load(&with_auto_share(", tail: {below: 0}")).expect("0 disables the release");
    for bad in ["-0.1", "1", "1.5", ".nan"] {
        rejected_for(
            &with_auto_share(&format!(", tail: {{below: {bad}}}")),
            "schedule.share.tail.below",
        );
    }
}

#[test]
fn rejects_a_gain_that_is_not_above_zero_and_finite() {
    for bad in ["0", "-1", ".nan", ".inf"] {
        rejected_for(
            &with_auto_share(&format!(", gain: {bad}")),
            "schedule.share.gain",
        );
    }
    load(&with_auto_share(", gain: 12")).expect("a high gain is legal, if brutal");
}

#[test]
fn rejects_a_floor_above_the_ceiling() {
    rejected_for(
        &with_auto_share(", floor: 0.6, ceiling: 0.4"),
        "above the ceiling",
    );
    // Equal is fine: a fixed share, expressed the long way round.
    load(&with_auto_share(", floor: 0.4, ceiling: 0.4")).expect("valid");
}

#[test]
fn rejects_an_auto_share_route_that_is_last_in_a_chain() {
    // The D-091 rule, for the same reason and more so: `auto` has no end, so
    // every message it turned away would be §10.3's `451` for good.
    let yaml = with_auto_share("").replace("chain: [warming, overflow]", "chain: [warming]");
    rejected_for(&yaml, "is last in the chain");
}

#[test]
fn rejects_an_unknown_key_by_name() {
    // The reason the `Deserialize` is written by hand: `#[serde(untagged)]`
    // would say only "did not match any variant".
    let err = load(&with_auto_share(", flor: 0.1")).expect_err("rejected");
    let text = err.to_string();
    assert!(text.contains("flor"), "must name the key, got: {text}");
}

#[test]
fn rejects_a_share_that_is_neither_a_list_nor_an_auto_map() {
    for bad in ["0.5", "\"auto\"", "true"] {
        load(&with_share(bad)).expect_err("neither a list nor a map");
    }
    // A map without `mode` is not an auto share either.
    load(&with_share("{floor: 0.1}")).expect_err("no mode");
    // And `mode` must be `auto`.
    load(&with_share("{mode: manual}")).expect_err("unknown mode");
}

#[test]
fn auto_share_violations_are_reported_together() {
    let yaml = with_auto_share(", floor: 0, gain: 0, fill_by: 2")
        .replace("chain: [warming, overflow]", "chain: [warming]");
    match load(&yaml) {
        Err(LoadError::Invalid(v)) => {
            for needle in [
                "schedule.share.floor",
                "schedule.share.gain",
                "schedule.share.fill_by",
                "is last in the chain",
            ] {
                assert!(v.mentions(needle), "missing '{needle}' in:\n{v}");
            }
        }
        Ok(_) => panic!("accepted"),
        Err(other) => panic!("expected a validation failure, got: {other}"),
    }
}

// -- §4.2: admin.metrics (D-093) --------------------------------------------

fn with_metrics(value: &str) -> String {
    BASE.replace(
        "  auth_token: \"tok\"\n",
        &format!("  auth_token: \"tok\"\n  metrics: {value}\n"),
    )
}

#[test]
fn metrics_is_off_by_default() {
    let m = load(BASE).expect("valid").admin.metrics();
    assert!(!m.enabled);
    assert_eq!(m.idle_timeout, config::Metrics::DEFAULT_IDLE_TIMEOUT);
}

#[test]
fn metrics_takes_a_boolean_or_a_map() {
    assert!(
        load(&with_metrics("true"))
            .expect("valid")
            .admin
            .metrics()
            .enabled
    );
    assert!(
        !load(&with_metrics("false"))
            .expect("valid")
            .admin
            .metrics()
            .enabled
    );

    let m = load(&with_metrics("{ enabled: true, idle_timeout: 6h }"))
        .expect("valid")
        .admin
        .metrics();
    assert!(m.enabled);
    assert_eq!(m.idle_timeout, std::time::Duration::from_secs(6 * 3600));

    let m = load(&with_metrics("{ enabled: true }"))
        .expect("valid")
        .admin
        .metrics();
    assert_eq!(m.idle_timeout, config::Metrics::DEFAULT_IDLE_TIMEOUT);
}

#[test]
fn a_malformed_metrics_value_is_a_parse_failure() {
    for bad in [
        "{ idle_timeout: 6h }",        // `enabled` is required in the map form
        "{ enabled: true, idle: 6h }", // unknown key
        "\"yes\"",                     // not a boolean
        "{ enabled: true, idle_timeout: soon }",
    ] {
        assert!(
            matches!(load(&with_metrics(bad)), Err(LoadError::Parse { .. })),
            "{bad}"
        );
    }
}

#[test]
fn rejects_an_idle_timeout_under_a_minute() {
    rejected_for(
        &with_metrics("{ enabled: true, idle_timeout: 30s }"),
        "admin.metrics.idle_timeout",
    );
    // Checked when off too, so switching it on later surfaces nothing new.
    rejected_for(
        &with_metrics("{ enabled: false, idle_timeout: 30s }"),
        "admin.metrics.idle_timeout",
    );
    load(&with_metrics("{ enabled: true, idle_timeout: 1m }")).expect("1m is the floor");
}

#[test]
fn an_unset_metrics_key_warns_and_an_explicit_one_does_not() {
    // D-093: the likely reader is an operator upgrading past v0.7.0 whose
    // scrapes just stopped, so the warning says what happened and how to undo
    // it. `false` is a decision, and is not second-guessed.
    let unset = config::validate::warnings(&load(BASE).expect("valid"));
    let w = unset
        .iter()
        .find(|w| w.path == "admin.metrics")
        .unwrap_or_else(|| panic!("expected an admin.metrics warning, got: {unset:?}"));
    assert!(w.message.contains("404") && w.message.contains("admin.metrics: true"));

    for value in ["true", "false", "{ enabled: false }"] {
        let explicit = config::validate::warnings(&load(&with_metrics(value)).expect("valid"));
        assert!(
            !explicit.iter().any(|w| w.path == "admin.metrics"),
            "{value}: {explicit:?}"
        );
    }
}

// -- §3.4 / §4.2: ramps (D-099) ------------------------------------------

/// The seven keys D-099 moved under `ramps.<name>`.
const RAMP_KEYS: [&str; 7] = [
    "domain_groups",
    "senders",
    "default_chain",
    "strict_senders",
    "thread_affinity",
    "exhausted_chain_reply",
    "routes",
];

/// [`BASE`] as a pre-D-099 document: the routing block back at the top level.
fn pre_ramps(extra: &str) -> String {
    let mut yaml = BASE.replace("default_ramp: main\nramps:\n main:\n", "");
    for key in RAMP_KEYS {
        yaml = yaml.replace(&format!("\n  {key}:"), &format!("\n{key}:"));
    }
    yaml + extra
}

#[test]
fn a_pre_ramps_config_is_refused_with_a_pointer_for_every_moved_key() {
    let yaml = pre_ramps("thread_affinity: false\nexhausted_chain_reply: \"451\"\n");
    match load(&yaml) {
        Err(LoadError::Invalid(v)) => {
            // All seven, from one failed start (§4.2), each saying where it went.
            for key in RAMP_KEYS {
                let hit = v.0.iter().find(|x| x.path == key);
                let hit = hit.unwrap_or_else(|| panic!("no violation for '{key}':\n{v}"));
                assert!(hit.message.contains("ramps"), "{hit}");
                assert!(hit.message.contains("default_ramp"), "{hit}");
                assert!(hit.message.contains("D-099"), "{hit}");
            }
        }
        other => panic!("expected the moved keys to be named, got {other:?}"),
    }
}

#[test]
fn a_single_moved_key_is_named_on_its_own() {
    // An operator who has migrated all but one key is told which one.
    rejected_for(&format!("{BASE}strict_senders: true\n"), "strict_senders");
}

#[test]
fn default_ramp_is_required() {
    let yaml = BASE.replace("default_ramp: main\n", "");
    match load(&yaml) {
        Err(LoadError::Parse { source, .. }) => {
            assert!(source.to_string().contains("default_ramp"), "{source}")
        }
        other => panic!("expected a missing-field error, got {other:?}"),
    }
}

#[test]
fn default_ramp_must_name_a_declared_ramp() {
    let yaml = BASE.replace("default_ramp: main\n", "default_ramp: mian\n");
    match load(&yaml) {
        Err(LoadError::Invalid(v)) => {
            let hit =
                v.0.iter()
                    .find(|x| x.path == "default_ramp")
                    .expect("named");
            // Says what it named and what exists, so the typo is obvious.
            assert!(
                hit.message.contains("'mian'") && hit.message.contains("main"),
                "{hit}"
            );
        }
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn ramps_must_not_be_empty() {
    let empty = "default_ramp: main\nramps: {}\n";
    let head = &BASE[..BASE.find("default_ramp: main").expect("fixture")];
    rejected_for(&format!("{head}{empty}"), "at least one ramp");
}

#[test]
fn a_ramp_name_must_be_short_and_url_safe() {
    for bad in ["-leading", "has space", "slash/ed", "ünicode"] {
        let yaml = BASE
            .replace(
                "default_ramp: main\n",
                &format!("default_ramp: \"{bad}\"\n"),
            )
            .replace("\n main:\n", &format!("\n \"{bad}\":\n"));
        rejected_for(&yaml, "a ramp name must start with a letter or digit");
    }

    let long = "r".repeat(65);
    let yaml = BASE
        .replace("default_ramp: main\n", &format!("default_ramp: {long}\n"))
        .replace("\n main:\n", &format!("\n {long}:\n"));
    rejected_for(&yaml, "at most 64 characters");

    for good in ["a", "transactional", "brand-2.eu_west", "0"] {
        let yaml = BASE
            .replace(
                "default_ramp: main\n",
                &format!("default_ramp: \"{good}\"\n"),
            )
            .replace("\n main:\n", &format!("\n \"{good}\":\n"));
        let cfg = load(&yaml).unwrap_or_else(|e| panic!("'{good}' should be valid: {e}"));
        assert_eq!(cfg.default_ramp().name, good);
    }
}

#[test]
fn a_ramp_declared_twice_is_refused() {
    // The YAML layer refuses a repeated mapping key before validation runs, so a
    // second `main:` cannot silently replace the first.
    let tail = &BASE[BASE.find("\n main:\n").expect("fixture")..];
    let yaml = format!("{BASE}{tail}");
    match load(&yaml) {
        Err(LoadError::Parse { source, .. }) => {
            assert!(source.to_string().contains("duplicate"), "{source}")
        }
        other => panic!("expected a duplicate-key error, got {other:?}"),
    }
}

#[test]
fn more_than_one_ramp_is_refused_until_state_is_keyed_by_ramp() {
    // D-099's phasing: the scaffolding rule that goes when storage, pools and
    // metrics carry the ramp.
    let second =
        &BASE[BASE.find("\n main:\n").expect("fixture")..].replace("\n main:\n", "\n other:\n");
    rejected_for(&format!("{BASE}{second}"), "exactly one");
}

#[test]
fn a_violation_inside_a_ramp_names_the_ramp() {
    let yaml = BASE.replace("chain: [warming, overflow]", "chain: [warming, nonesuch]");
    match load(&yaml) {
        Err(LoadError::Invalid(v)) => {
            let hit =
                v.0.iter()
                    .find(|x| x.message.contains("nonesuch"))
                    .expect("reported");
            assert!(hit.path.starts_with("ramps.main.senders[0]"), "{hit}");
        }
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn ramp_level_and_per_ramp_violations_are_reported_together() {
    // §4.2's "all, not the first", across the new boundary: a bad default_ramp
    // does not hide what is wrong inside the ramp.
    let yaml = BASE
        .replace("default_ramp: main\n", "default_ramp: nonesuch\n")
        .replace("default_chain: [overflow]", "default_chain: [warming]");
    match load(&yaml) {
        Err(LoadError::Invalid(v)) => {
            assert!(v.0.iter().any(|x| x.path == "default_ramp"), "{v}");
            assert!(
                v.0.iter().any(|x| x.path == "ramps.main.default_chain"),
                "{v}"
            );
        }
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn a_ramp_not_called_main_is_the_one_routed_in() {
    let yaml = BASE
        .replace("default_ramp: main\n", "default_ramp: transactional\n")
        .replace("\n main:\n", "\n transactional:\n");
    let cfg = load(&yaml).expect("valid");
    assert_eq!(cfg.ramps.len(), 1);
    assert_eq!(cfg.default_ramp().name, "transactional");
    assert!(cfg.ramp("transactional").is_some());
    assert!(cfg.ramp("main").is_none());
    assert_eq!(cfg.default_ramp().routes.len(), 2);
}

#[test]
fn warnings_inside_a_ramp_name_the_ramp() {
    // strict_senders: false is warned about (§14.2), now under its ramp.
    let warnings = config::validate::warnings(&load(BASE).expect("valid"));
    assert!(
        warnings
            .iter()
            .any(|w| w.path == "ramps.main.strict_senders"),
        "{warnings:?}"
    );
}
