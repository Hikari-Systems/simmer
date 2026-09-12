//! The T2 server matrix: Simmer against real, differently-configured mail
//! servers (docs/TESTING.md; the test programme's step 3).
//!
//! Every Postfix variant in `test/compose/matrix.yml` relays into one Mailpit
//! trap, and `test/config/simmer.matrix.yaml` picks a variant by the sender's
//! domain — so each test chooses a server by choosing a sender, and reads the
//! result the same way.
//!
//! ```sh
//! docker compose -f docker-compose.yml -f test/compose/acceptance.yml \
//!   -f test/compose/matrix.yml --profile acceptance --profile matrix up -d --build
//! cargo test --test e2e_matrix -- --ignored --test-threads=1
//! ```

mod compose;

const MATRIX_CONFIG: &str = "test/config/simmer.matrix.yaml";

// ---------------------------------------------------------------------------
// the one test that needs no Docker
// ---------------------------------------------------------------------------

#[test]
fn the_matrix_config_is_valid_and_routes_each_server_by_sender() {
    // Loaded through Simmer's own `config::load`, so everything §4.2 would refuse
    // at the container's startup is refused here first — in seconds, not after a
    // build and a stack that will not come up.
    let cfg = compose::configs::load(MATRIX_CONFIG);

    let routes: Vec<&str> = cfg.routes.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        routes,
        [
            "mailpit-direct",
            "postfix-plain",
            "postfix-tls-auth",
            "postfix-login-only",
            "postfix-strict",
            "postfix-ratelimit"
        ]
    );

    // Each server is reached by exactly one sender domain, which is how the
    // matrix tests pick a server; anything else lands on the baseline.
    for (sender, route) in [
        ("jane@plain.matrix.test", "postfix-plain"),
        ("jane@tls.matrix.test", "postfix-tls-auth"),
        ("jane@login.matrix.test", "postfix-login-only"),
        ("jane@strict.matrix.test", "postfix-strict"),
        ("jane@ratelimit.matrix.test", "postfix-ratelimit"),
    ] {
        let senders = simmer::routing::sender_match::Senders::new(Some(sender), None);
        match simmer::routing::sender_match::match_sender(&cfg, &senders) {
            simmer::routing::sender_match::Match::Rule { rule, .. } => {
                assert_eq!(rule.chain, [route], "{sender}")
            }
            simmer::routing::sender_match::Match::Unmatched => {
                panic!("{sender} matched no rule")
            }
        }
    }
    assert_eq!(
        cfg.default_chain.as_deref(),
        Some(&["mailpit-direct".to_string()][..])
    );

    // The TLS route verifies: that is the point of it.
    let tls = cfg.route("postfix-tls-auth").expect("route");
    assert_eq!(tls.downstream.tls, simmer::config::TlsMode::RequiredVerify);
    assert!(tls.downstream.auth.is_some());
}
