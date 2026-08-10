//! `SPEC.md` §4.2 startup validation, end to end through the real loader.
//!
//! One test per rule, plus the one that matters most in practice: a config with
//! several independent faults must report *all* of them. §4.2 says "Report all
//! violations, not just the first", and the reason is operational — a service
//! that takes minutes to build should not be discovered to be misconfigured one
//! error at a time.

use simmer::config::{self, LoadError};

/// A minimal configuration that passes every rule. Tests mutate one thing.
const BASE: &str = r#"
server:
  listen: "127.0.0.1:25"
  hostname: "simmer.test"
  max_message_bytes: 26214400
  max_recipients: 100
  max_concurrent_sessions: 64
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth:
    required: true
    allow_insecure_auth: true
    mechanisms: [PLAIN, LOGIN]
    users:
      - username: "cfapp"
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
database:
  url: "postgres://simmer:simmer@localhost/simmer"
  max_connections: 10
  connect_timeout: 5s
  fail_closed: true
admin:
  listen: "127.0.0.1:8080"
  auth_token: "tok"
logging: { level: info, format: json }
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

#[test]
fn rejects_auth_required_with_no_users() {
    let yaml = BASE.replace(
        r#"    users:
      - username: "cfapp"
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa""#,
        "    users: []",
    );
    rejected_for(&yaml, "no client could ever authenticate");
}

#[test]
fn rejects_allow_insecure_auth_false() {
    // There is no inbound TLS (§5.1), so AUTH is only usable over plaintext and
    // that has to be an acknowledged choice rather than a default.
    let yaml = BASE.replace("allow_insecure_auth: true", "allow_insecure_auth: false");
    rejected_for(&yaml, "must be explicitly true");
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
    let yaml = BASE.replace("default_chain: [overflow]\n", "");
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
        .replace("default_chain: [overflow]\n", "")
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
