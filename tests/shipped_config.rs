//! The configuration this repository actually ships must load.
//!
//! `simmer.yaml` mirrors the `SPEC.md` §4.1 example, so this doubles as the
//! round-trip test for the documented schema: if the spec grows a key and the
//! types do not, this fails. It also exercises the real `${ENV_VAR}` path, which
//! the unit tests deliberately stub out.

use simmer::config;

/// Every variable `simmer.yaml` references. Kept here rather than read from
/// `.env.example` so that adding a secret to the config without documenting it
/// is a test failure.
const REQUIRED_VARS: [(&str, &str); 6] = [
    ("DATABASE_URL", "postgres://simmer:simmer@localhost/simmer"),
    (
        "SIMMER_CFAPP_HASH",
        "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ),
    ("SIMMER_ADMIN_TOKEN", "test-token"),
    ("POSTAL_USER", "postal"),
    ("POSTAL_PASS", "postal-secret"),
    ("SENDGRID_KEY", "SG.test"),
];

/// The process environment is global, and one test here deliberately removes a
/// variable. Without this every test in the file would be racing the others for
/// the same `SENDGRID_KEY`, which is exactly the kind of flake that gets a suite
/// ignored. Held for the duration of each test rather than just the mutation,
/// because the read (`config::load`) is the half that observes the race.
static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the environment lock, tolerating a previous test having panicked while
/// holding it — a poisoned lock here means an earlier failure, not corrupt state.
fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_all_vars() {
    for (k, v) in REQUIRED_VARS {
        std::env::set_var(k, v);
    }
}

fn load_shipped() -> config::Config {
    set_all_vars();
    config::load("simmer.yaml").unwrap_or_else(|e| panic!("shipped simmer.yaml is invalid:\n{e}"))
}

#[test]
fn the_shipped_config_loads_and_validates() {
    let _env = env_guard();
    let cfg = load_shipped();

    // Spot-check that interpolation actually happened rather than leaving the
    // reference in place.
    assert!(!cfg.database.url.contains("${"));
    assert_eq!(
        cfg.database.url,
        "postgres://simmer:simmer@localhost/simmer"
    );

    // §4.1 shape.
    assert_eq!(cfg.domain_groups.len(), 4);
    assert_eq!(cfg.routes.len(), 2);
    assert_eq!(cfg.senders.len(), 4);
    assert!(cfg.catchall_group().is_some());

    // §5.6 default.
    assert!(cfg.server.single_recipient_only);
    // §7.5 default.
    assert!(cfg.database.fail_closed);
    // §10.3 default.
    assert_eq!(
        cfg.exhausted_chain_reply,
        config::ExhaustedChainReply::Temporary
    );
}

#[test]
fn every_field_of_the_spec_example_survives_the_round_trip() {
    let _env = env_guard();
    let cfg = load_shipped();

    let warming = cfg.route("warming-newbrand").expect("warming route");
    assert!(!warming.overflow);
    assert_eq!(warming.downstream.port, 587);
    assert_eq!(warming.downstream.tls, config::TlsMode::RequiredVerify);
    assert_eq!(warming.downstream.pool.max_connections, 4);

    // Header order is part of the byte-equivalence the §12.3 acceptance test
    // asserts, so it must survive parsing.
    let names: Vec<&str> = warming
        .identity
        .set_headers
        .iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        names,
        [
            "From",
            "Reply-To",
            "Message-ID",
            "List-Unsubscribe",
            "List-Unsubscribe-Post",
            "X-Simmer-Route",
            "X-Simmer-Correlation-Id",
        ]
    );

    // §6.6: Reply-To is migration-only and declared.
    assert_eq!(warming.identity.unstable_headers, ["Reply-To"]);
    assert_eq!(warming.identity.remove_headers, ["Return-Path", "X-Mailer"]);
    assert_eq!(warming.identity.body_rewrites.len(), 1);

    // §6.7.
    assert!(warming.preflight_enabled());
    let pf = warming.preflight.as_ref().expect("preflight");
    assert_eq!(pf.dkim_selector.as_deref(), Some("s1"));
    assert!(pf.require_dmarc);
    assert!(
        !pf.strict,
        "a DNS blip must not become an outage by default"
    );

    // §7.2 / §7.3.
    let warmup = warming.warmup.as_ref().expect("warmup");
    assert_eq!(warmup.schedule.default.len(), 8);
    assert_eq!(warmup.schedule.overrides.len(), 2);
    let freq = warming.recipient_frequency.as_ref().expect("frequency");
    assert_eq!(freq.threshold, 3);
    assert_eq!(freq.mode, config::FrequencyMode::ToAddress);

    // The overflow route carries no warm-up and is never quota-limited (§3.1).
    let overflow = cfg.route("overflow-established").expect("overflow route");
    assert!(overflow.overflow);
    assert!(overflow.warmup.is_none());
}

#[test]
fn the_schedule_repeats_its_final_value_rather_than_uncapping() {
    let _env = env_guard();
    // §7.2: "When day_index exceeds the array bounds, the final value repeats
    // indefinitely. Routes do not auto-graduate to uncapped."
    let cfg = load_shipped();
    let schedule = &cfg
        .route("warming-newbrand")
        .expect("warming route")
        .warmup
        .as_ref()
        .expect("warmup")
        .schedule;

    assert_eq!(schedule.allowance_for("catchall", 0), Some(50));
    assert_eq!(schedule.allowance_for("catchall", 7), Some(5000));
    assert_eq!(schedule.allowance_for("catchall", 8), Some(5000));
    assert_eq!(schedule.allowance_for("catchall", 10_000), Some(5000));

    // Per-group overrides apply where present, and fall back to the default
    // series where absent.
    assert_eq!(schedule.allowance_for("google", 0), Some(20));
    assert_eq!(schedule.allowance_for("google", 999), Some(4000));
    assert_eq!(schedule.allowance_for("yahoo", 0), Some(50));
}

#[test]
fn the_shipped_config_produces_the_expected_startup_warnings() {
    let _env = env_guard();
    let cfg = load_shipped();
    let warnings = config::validate::warnings(&cfg);
    let rendered: Vec<String> = warnings.iter().map(|w| w.to_string()).collect();
    let all = rendered.join("\n");

    // §6.6: each declared migration-only header is named at startup.
    assert!(
        all.contains("Reply-To") && all.contains("migration-only"),
        "expected a WARN naming the declared unstable header:\n{all}"
    );
    // §14.2: strict_senders is false in the shipped config, which is the
    // documented default but carries a real risk worth restating at startup.
    assert!(
        all.contains("strict_senders"),
        "expected a WARN about unmatched senders:\n{all}"
    );
}

#[test]
fn a_missing_secret_is_a_fatal_startup_error() {
    let _env = env_guard();
    // §4: "an unresolvable reference is a fatal startup error".
    set_all_vars();
    std::env::remove_var("SENDGRID_KEY");

    match config::load("simmer.yaml") {
        Err(config::LoadError::Unresolved(missing)) => {
            assert_eq!(missing.len(), 1);
            assert_eq!(missing[0].var, "SENDGRID_KEY");
            // The report must say where, not just what.
            assert!(
                missing[0].path.contains("routes"),
                "unhelpful path: {}",
                missing[0].path
            );
        }
        other => panic!("expected an unresolved-reference failure, got: {other:?}"),
    }

    // Restore, so a later test in this binary is not affected.
    set_all_vars();
}
