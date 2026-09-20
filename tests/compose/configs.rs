//! Loading a test tier's Simmer configuration on the host.
//!
//! A tier's config reaches `app` inside the stack, but it is also loaded here,
//! in ordinary `cargo test`, through Simmer's own `config::load` — so a config
//! mistake fails in seconds rather than after an image build and a stack that
//! will not come up. §4.2 reads the TLS files and resolves every `${VAR}`, so the
//! stack's environment is stood in for: a certificate minted here, and harmless
//! values for the secrets.

use simmer::config::Config;

/// Load `path` (relative to the repository root) as `app` would, with the
/// stack's environment stood in for. Variables already set are left alone.
pub fn load(path: &str) -> Config {
    if std::env::var("SIMMER_TLS_DIR").is_err() {
        // Kept for the life of the process: the `Config` it validates does not
        // outlive it, and neither should the files.
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = rcgen::CertificateParams::new(vec!["simmer.acceptance".to_string()])
            .expect("params")
            .self_signed(&key)
            .expect("cert");
        std::fs::write(dir.join("cert.pem"), cert.pem()).expect("write cert");
        std::fs::write(dir.join("key.pem"), key.serialize_pem()).expect("write key");
        std::env::set_var("SIMMER_TLS_DIR", dir);
    }
    if std::env::var("SIMMER_CAPTURE_DIR").is_err() {
        // D-085's capture directory, for the generated capture twins. §4.2 probes
        // it for real rather than reading mode bits, so it has to exist and be
        // writable — a real temporary directory, not a plausible path. Kept for
        // the life of the process, like the certificate above.
        let dir = tempfile::tempdir().expect("tempdir").keep();
        std::env::set_var("SIMMER_CAPTURE_DIR", dir);
    }
    for (k, v) in [
        ("SIMMER_WARMUP_STARTED", "2026-08-01T00:00:00Z"),
        // test/config/simmer.stress.yaml's session timeout, which S9 overrides.
        ("SIMMER_SESSION_TIMEOUT", "120s"),
        ("DATABASE_URL", "postgres://simmer:simmer@127.0.0.1:5433/simmer"),
        ("SIMMER_ADMIN_TOKEN", "test-tier-admin-token"),
        (
            "SIMMER_CFAPP_HASH",
            "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ),
    ] {
        if std::env::var(k).is_err() {
            std::env::set_var(k, v);
        }
    }
    simmer::config::load(path).unwrap_or_else(|e| panic!("{path} is invalid:\n{e}"))
}
