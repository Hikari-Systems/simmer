//! Every configuration the manual ships must load.
//!
//! `docs/manual/examples/*.yaml` are what a new user copies first, so a key the
//! schema renames or a rule §4.2 adds must fail here rather than at somebody's
//! first `docker run`. Like `tests/shipped_config.rs`, this goes through the
//! real `${ENV_VAR}` path.

use std::path::Path;

use simmer::config;

/// Every variable the examples reference, with a value that passes §4.2.
const VARS: &[(&str, &str)] = &[
    ("SIMMER_HOSTNAME", "simmer.internal"),
    ("DATABASE_URL", "postgres://simmer:simmer@localhost/simmer"),
    ("SIMMER_ADMIN_TOKEN", "a-long-random-admin-token"),
    ("SIMMER_SMTP_USER", "app"),
    (
        "SIMMER_SMTP_PASSWORD_HASH",
        "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ),
    ("SIMMER_CURRENT_DOMAIN", "oldbrand.example"),
    ("SIMMER_NEW_DOMAIN", "newbrand.example"),
    ("SIMMER_WARM_SMTP_HOST", "smtp.warm.example"),
    ("SIMMER_WARM_SMTP_USER", "warm"),
    ("SIMMER_WARM_SMTP_PASSWORD", "warm-secret"),
    ("SIMMER_WARMUP_STARTED", "2026-11-02T09:00:00Z"),
    ("SIMMER_OVERFLOW_SMTP_HOST", "smtp.overflow.example"),
    ("SIMMER_OVERFLOW_SMTP_USER", "overflow"),
    ("SIMMER_OVERFLOW_SMTP_PASSWORD", "overflow-secret"),
];

#[test]
fn every_manual_example_loads() {
    for (k, v) in VARS {
        std::env::set_var(k, v);
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/manual/examples");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("docs/manual/examples exists") {
        let path = entry.expect("readable").path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        seen += 1;
        if let Err(e) = config::load(&path) {
            panic!("{} does not load:\n{e}", path.display());
        }
    }
    assert!(seen > 0, "no examples found in {}", dir.display());
}

/// The test vectors `11-observability.md` gives for reimplementing
/// `simmer.recipient.hash` elsewhere (D-114). Checked against simmer itself, so
/// an implementation that passes them matches what the export carries.
#[test]
fn the_recipient_hash_vectors_are_what_simmer_exports() {
    use simmer::config::RecipientExposure;
    use simmer::frequency::normalise;
    use simmer::telemetry::recipient::Exposure;

    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs/manual/examples/recipient-hash-vectors.json");
    let text = std::fs::read_to_string(&path).expect("the vectors file exists");
    let cases: Vec<serde_json::Value> = serde_json::from_str(&text).expect("valid JSON");
    assert!(!cases.is_empty());

    for case in &cases {
        let s = |k: &str| case[k].as_str().expect(k).to_string();
        let domains: Vec<String> = case["domains"]
            .as_array()
            .expect("domains")
            .iter()
            .map(|d| d.as_str().expect("a domain").to_string())
            .collect();
        let (key, address) = (s("key"), s("address"));

        assert_eq!(
            normalise(&address, config::FrequencyMode::ToAddress, &domains),
            s("normalised"),
            "normalised form of {address:?}"
        );
        assert_eq!(
            &s("hmac_full")[..32],
            s("hash"),
            "{address:?}: hash is hmac_full truncated"
        );

        let exposure = Exposure::new(RecipientExposure::Hashed, Some(key.as_bytes()), &domains)
            .expect("hashed with a key is on");
        let exported = exposure
            .attributes(&address)
            .into_iter()
            .find(|kv| kv.key.as_str() == "simmer.recipient.hash")
            .map(|kv| kv.value.as_str().into_owned());
        assert_eq!(exported, Some(s("hash")), "exported hash of {address:?}");
    }
}
