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

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use compose::findings::judge;
use compose::loadgen::Reply;
use compose::mail::header;
use compose::stack::MATRIX;
use compose::traps;

const MATRIX_CONFIG: &str = "test/config/simmer.matrix.yaml";

/// The admin API, published by `docker-compose.yml`.
const ADMIN: &str = "http://127.0.0.1:8080";

/// Every server that takes Simmer's mail as configured: the sender subdomain that
/// selects it, and the route that carries it.
const DELIVERING: [(&str, &str); 5] = [
    ("mailpit", "mailpit-direct"),
    ("plain", "postfix-plain"),
    ("tls", "postfix-tls-auth"),
    ("strict", "postfix-strict"),
    ("ratelimit", "postfix-ratelimit"),
];

/// postfix-ratelimit's `smtpd_timeout` (`PF_SMTPD_TIMEOUT` in matrix.yml): how
/// long an idle pooled connection survives at the far end.
const RATELIMIT_REAPS_AFTER: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// every server
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the matrix compose profile"]
fn every_server_delivers_once_under_its_own_route_identity() {
    let _logs = MATRIX.logs_on_failure();
    await_rate_window();
    traps::MATRIX.reset();

    for (domain, route) in DELIVERING {
        let replies = send(domain, &format!("once-{domain}"), 1, &[]);
        assert!(
            replies.iter().all(|r| r.code == 250),
            "{route} did not accept: {replies:?}"
        );
    }
    assert_eq!(
        traps::MATRIX.wait_for_count(DELIVERING.len()),
        DELIVERING.len()
    );

    let mut seen = Vec::new();
    for (return_path, raw) in traps::MATRIX.envelopes() {
        let route =
            header(&raw, "X-Simmer-Route").unwrap_or_else(|| panic!("no X-Simmer-Route:\n{raw}"));
        // The envelope the last server was handed, per the trap's own record: a
        // Postfix relay keeps Simmer's MAIL FROM, so every route's identity
        // survives a real MTA in between.
        assert_eq!(
            return_path,
            format!("bounce@{route}.out.test"),
            "{route}'s envelope sender"
        );
        // §6.5, whichever server carried it — and nothing in between re-added one.
        for artefact in simmer::rewrite::AUTH_ARTEFACTS {
            assert!(
                header(&raw, artefact).is_none(),
                "{route} delivered {artefact}:\n{raw}"
            );
        }
        // Physically carried by the server the route names, not just labelled so.
        if route.starts_with("postfix-") {
            assert!(
                postfix_received(&raw, &route).is_some(),
                "{route}'s message never passed through {route}:\n{raw}"
            );
        }
        seen.push(route);
    }
    seen.sort();
    let mut want: Vec<String> = DELIVERING.iter().map(|(_, r)| r.to_string()).collect();
    want.sort();
    assert_eq!(seen, want, "each server should deliver exactly once");
}

#[test]
#[ignore = "needs the matrix compose profile"]
fn every_inbound_mode_reaches_a_real_mta() {
    // 25 in plaintext, 587 with STARTTLS, 465 with implicit TLS — both TLS modes
    // verified by the loadgen through its OS trust store — each relayed through
    // postfix-plain. Simmer's own Received: says which it was (RFC 3848).
    let _logs = MATRIX.logs_on_failure();
    traps::MATRIX.reset();

    let modes: [(&str, &[&str], &str); 3] = [
        ("smtp25", &[], "with ESMTPA id"),
        (
            "starttls587",
            &["--port", "587", "--starttls", "--ca", "os"],
            "with ESMTPSA id",
        ),
        (
            "implicit465",
            &["--port", "465", "--mode", "implicit", "--ca", "os"],
            "with ESMTPSA id",
        ),
    ];
    for (tag, extra, _) in modes {
        let replies = send("plain", tag, 1, extra);
        assert!(
            replies.iter().all(|r| r.code == 250),
            "{tag} was not accepted: {replies:?}"
        );
    }
    assert_eq!(traps::MATRIX.wait_for_count(modes.len()), modes.len());

    let raws = traps::MATRIX.raw_messages();
    for (tag, _, protocol) in modes {
        let raw = raws
            .iter()
            .find(|r| header(r, "To").is_some_and(|to| to.starts_with(&format!("{tag}-"))))
            .unwrap_or_else(|| panic!("{tag}'s message did not arrive"));
        let ours = received_lines(raw)
            .into_iter()
            .find(|l| l.contains("by simmer.acceptance"))
            .unwrap_or_else(|| panic!("no Received: from Simmer:\n{raw}"));
        assert!(ours.contains(protocol), "{tag}: {ours}");
        assert!(
            postfix_received(raw, "postfix-plain").is_some(),
            "{tag} never passed through postfix-plain:\n{raw}"
        );
    }
}

#[test]
#[ignore = "needs the matrix compose profile"]
fn simmer_introduces_itself_by_its_own_name() {
    // §4.1: `server.hostname` is Simmer's EHLO identity, and RFC 5321 §4.1.1.1
    // wants the client's own name there — finding F14 (D-077) was the
    // downstream's own name in that slot. Postfix records the name it was given
    // as the `from` of its Received:, and records the *last* one, so the TLS
    // route checks the EHLO Simmer re-issues after STARTTLS.
    let _logs = MATRIX.logs_on_failure();
    traps::MATRIX.reset();

    for domain in ["plain", "tls"] {
        let replies = send(domain, &format!("helo-{domain}"), 1, &[]);
        assert!(replies.iter().all(|r| r.code == 250), "{replies:?}");
    }
    assert_eq!(traps::MATRIX.wait_for_count(2), 2);
    for raw in traps::MATRIX.raw_messages() {
        let route =
            header(&raw, "X-Simmer-Route").unwrap_or_else(|| panic!("no X-Simmer-Route:\n{raw}"));
        let received = postfix_received(&raw, &route)
            .unwrap_or_else(|| panic!("no Received: from {route}:\n{raw}"));
        // "Received: from <EHLO name> (<reverse DNS> [<address>]) by <server> …"
        let helo = received
            .trim_start_matches("Received:")
            .trim()
            .strip_prefix("from ")
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or_default();
        assert_eq!(helo, "simmer.acceptance", "{route}: {received}");
    }
}

// ---------------------------------------------------------------------------
// one server each
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the matrix compose profile"]
fn the_verified_tls_route_reaches_postfix_encrypted_and_authenticated() {
    // `required_verify` against a real server whose leaf chains to the per-run
    // CA, which `app` trusts only through its OS trust store; then real SASL. The
    // other side's Received: is the proof, in the server's own words.
    let _logs = MATRIX.logs_on_failure();
    traps::MATRIX.reset();

    let replies = send("tls", "tlsauth", 1, &[]);
    assert!(replies.iter().all(|r| r.code == 250), "{replies:?}");
    assert_eq!(traps::MATRIX.wait_for_count(1), 1);
    let raw = traps::MATRIX.raw_messages().remove(0);
    let received = postfix_received(&raw, "postfix-tls-auth")
        .unwrap_or_else(|| panic!("no Received: from postfix-tls-auth:\n{raw}"));
    assert!(received.contains("with ESMTPSA"), "{received}");
    assert!(received.contains("(using TLSv1."), "{received}");
}

#[test]
#[ignore = "needs the matrix compose profile"]
fn a_strict_server_gets_honest_deferrals_and_never_a_loss() {
    // postfix-strict advertises no 8BITMIME and a 64 KiB SIZE, and its route does
    // not assume 8BITMIME (D-074). Neither refusal may be a 5xx (§14.1), neither
    // message may arrive, and a 7-bit message that fits must still go.
    let _logs = MATRIX.logs_on_failure();
    traps::MATRIX.reset();
    let capability =
        r#"simmer_downstream_errors_total{ramp="main",route="postfix-strict",class="capability"}"#;
    let capability_before = metric(capability);

    let eight_bit = send("strict", "strict8bit", 1, &["--charset", "latin1"]);
    assert!(
        eight_bit
            .iter()
            .all(|r| r.code == 451 && r.text.starts_with("4.3.5")),
        "an 8-bit body to a server without 8BITMIME: {eight_bit:?}"
    );
    let oversized = send("strict", "strictbig", 1, &["--size", "100k"]);
    assert!(
        oversized
            .iter()
            .all(|r| r.code == 451 && r.text.contains("SIZE")),
        "a message over the server's SIZE: {oversized:?}"
    );
    let fits = send("strict", "strict7bit", 1, &[]);
    assert!(fits.iter().all(|r| r.code == 250), "{fits:?}");

    assert_eq!(traps::MATRIX.wait_for_count(1), 1);
    let raw = traps::MATRIX.raw_messages().remove(0);
    assert!(
        header(&raw, "To").is_some_and(|to| to.starts_with("strict7bit-")),
        "the wrong message arrived:\n{raw}"
    );
    assert_eq!(metric(capability) - capability_before, 1.0);
}

#[test]
#[ignore = "needs the matrix compose profile"]
fn a_rate_limited_server_defers_the_excess_and_never_duplicates() {
    // Real 450s from a real server's rate limit, under concurrency: each is 451
    // to the client (D-008 keeps it temporary), and the trap holds exactly the
    // messages that were answered 250 — no loss, no duplicate.
    let _logs = MATRIX.logs_on_failure();
    await_rate_window();
    traps::MATRIX.reset();

    let replies = send("ratelimit", "burst", 40, &["--concurrency", "4"]);
    let accepted: Vec<&Reply> = replies.iter().filter(|r| r.code == 250).collect();
    let deferred: Vec<&Reply> = replies.iter().filter(|r| r.code != 250).collect();
    assert!(
        deferred
            .iter()
            .all(|r| r.code == 451 && r.text.contains("4.7.1")),
        "every refusal should be the rate limit's, as a 451: {deferred:?}"
    );
    assert!(
        !accepted.is_empty() && !deferred.is_empty(),
        "the limit should both admit and defer: {} accepted, {} deferred",
        accepted.len(),
        deferred.len()
    );

    assert_eq!(traps::MATRIX.wait_for_count(accepted.len()), accepted.len());
    let mut delivered: Vec<String> = traps::MATRIX
        .raw_messages()
        .iter()
        .filter_map(|raw| header(raw, "To"))
        .collect();
    delivered.sort();
    let mut want: Vec<String> = accepted.iter().map(|r| r.recipient.clone()).collect();
    want.sort();
    assert_eq!(delivered, want, "delivered should be exactly the 250s");
}

#[test]
#[ignore = "needs the matrix compose profile"]
fn a_pooled_connection_the_server_reaped_is_replaced_not_deferred() {
    // D-068's case against a real server: postfix-ratelimit closes an idle
    // connection after smtpd_timeout, well inside the route's idle_ttl, so the
    // pool holds a connection the far end has already closed. Checkout
    // validation or the one retry must catch it; the client must never see it.
    let _logs = MATRIX.logs_on_failure();
    await_rate_window(); // also leaves a freshly used connection in the pool
    let retries = r#"simmer_pool_retries_total{ramp="main",route="postfix-ratelimit"}"#;
    let before = pool_stats("postfix-ratelimit");
    let retries_before = metric(retries);

    std::thread::sleep(RATELIMIT_REAPS_AFTER + Duration::from_secs(5));
    let replies = send("ratelimit", "reaped", 1, &[]);
    assert!(
        replies.iter().all(|r| r.code == 250),
        "a reaped pooled connection reached the client: {replies:?}"
    );

    let after = pool_stats("postfix-ratelimit");
    let discarded = after["discarded"].as_u64().unwrap() - before["discarded"].as_u64().unwrap();
    let retried = metric(retries) - retries_before;
    assert!(
        discarded >= 1 || retried >= 1.0,
        "the reaped connection was never noticed — was it reaped at all? \
         before {before}, after {after}"
    );
}

#[test]
#[ignore = "needs the matrix compose profile"]
fn a_login_only_server_is_reached_with_the_mechanism_it_offers() {
    // postfix-login-only offers SASL LOGIN and nothing else. Finding F9: Simmer
    // only ever tries PLAIN.
    let _logs = MATRIX.logs_on_failure();
    traps::MATRIX.reset();

    let replies = send("login", "login", 1, &[]);
    // §14.1 whatever the outcome: a login failure is D-023's 451, never a 5xx.
    assert!(
        replies.iter().all(|r| r.code == 250 || r.code == 451),
        "{replies:?}"
    );
    let delivered = traps::MATRIX.wait_for_count(usize::from(replies[0].code == 250));
    assert_eq!(
        delivered,
        usize::from(replies[0].code == 250),
        "delivered should match the reply: {replies:?}"
    );

    judge(
        "t2/matrix/login-only",
        if replies.iter().all(|r| r.code == 250) {
            Ok(())
        } else {
            Err(format!("{replies:?}"))
        },
    );
}

// ---------------------------------------------------------------------------
// another submitting client
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the matrix compose profile"]
fn postfix_as_a_submitting_client_is_verified_authenticated_and_relayed() {
    // A second client implementation beside the loadgen: Postfix relaying to 587
    // at `secure`, verified through its OS trust store, with SASL. Its log says
    // the handshake verified; Simmer's Received: says the session was encrypted
    // and authenticated; the trap says it arrived once, under the route identity.
    let _logs = MATRIX.logs_on_failure();
    traps::MATRIX.reset();

    submit_via_postfix_client(
        "From: Jane <jane@client.matrix.test>\n\
         To: postfix-client@example.net\n\
         Subject: Sent by Postfix\n\
         \n\
         Submitted with sendmail, relayed by Postfix.\n",
    );
    assert_eq!(traps::MATRIX.wait_for_count(1), 1);
    let (return_path, raw) = traps::MATRIX.envelopes().remove(0);
    assert_eq!(return_path, "bounce@mailpit-direct.out.test");
    let ours = received_lines(&raw)
        .into_iter()
        .find(|l| l.contains("by simmer.acceptance"))
        .unwrap_or_else(|| panic!("no Received: from Simmer:\n{raw}"));
    assert!(ours.starts_with("Received: from postfix-client "), "{ours}");
    assert!(ours.contains("with ESMTPSA id"), "{ours}");

    // `secure` would have refused an unverified server rather than downgrade, so
    // the message arriving is itself the proof; the log line says so in words.
    let log =
        String::from_utf8_lossy(&MATRIX.run(&["logs", "--no-color", "postfix-client"]).stdout)
            .to_string();
    assert!(
        log.contains("Verified TLS connection established to app["),
        "postfix-client did not verify Simmer's certificate:\n{log}"
    );
    let queue = String::from_utf8_lossy(
        &MATRIX
            .run(&["exec", "-T", "postfix-client", "postqueue", "-p"])
            .stdout,
    )
    .to_string();
    assert!(queue.contains("Mail queue is empty"), "{queue}");
}

// ---------------------------------------------------------------------------
// driving the matrix
// ---------------------------------------------------------------------------

/// Hand `message` (LF line endings, as `sendmail` expects) to postfix-client
/// from `jane@client.matrix.test`, recipients taken from its headers.
fn submit_via_postfix_client(message: &str) {
    let mut child = MATRIX
        .compose()
        .args(["exec", "-T", "postfix-client"])
        .args(["sendmail", "-i", "-t", "-f", "jane@client.matrix.test"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("docker compose exec");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(message.as_bytes())
        .expect("write to sendmail");
    let status = child.wait().expect("sendmail");
    assert!(status.success(), "sendmail in postfix-client failed");
}

/// Send `count` messages from `jane@<domain>.matrix.test`, which picks the server.
fn send(domain: &str, tag: &str, count: usize, extra: &[&str]) -> Vec<Reply> {
    let from = format!("jane@{domain}.matrix.test");
    let from_header = format!("Jane <{from}>");
    let count = count.to_string();
    let mut args = vec![
        "--count",
        &count,
        "--tag",
        tag,
        "--from",
        &from,
        "--from-header",
        &from_header,
    ];
    args.extend_from_slice(extra);
    compose::loadgen::run(&MATRIX, &args)
}

/// Wait until postfix-ratelimit accepts again. It admits 20 messages per client
/// per minute, and an earlier test may have spent them.
fn await_rate_window() {
    for _ in 0..12 {
        if send("ratelimit", "window", 1, &[])
            .iter()
            .all(|r| r.code == 250)
        {
            return;
        }
        std::thread::sleep(Duration::from_secs(10));
    }
    panic!("postfix-ratelimit never accepted again within two minutes");
}

/// Every Received: header, unfolded.
fn received_lines(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in raw.replace("\r\n", "\n").lines() {
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = lines.last_mut() {
                last.push(' ');
                last.push_str(line.trim());
            }
        } else {
            lines.push(line.to_string());
        }
    }
    lines
        .into_iter()
        .filter(|l| l.to_ascii_lowercase().starts_with("received:"))
        .collect()
}

/// The Received: header a Postfix variant wrote, if the message passed through it.
fn postfix_received(raw: &str, server: &str) -> Option<String> {
    let by = format!("by {server} (Postfix)");
    received_lines(raw).into_iter().find(|l| l.contains(&by))
}

/// One series' value from `/metrics`, or 0 if it has not been written yet.
fn metric(series: &str) -> f64 {
    let body = traps::get(&format!("{ADMIN}/metrics"));
    body.lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
        .map(|v| v.trim().parse().expect("a metric value"))
        .unwrap_or(0.0)
}

/// A route's pool counters from `/ramps/main/routes`.
fn pool_stats(route: &str) -> serde_json::Value {
    let token = String::from_utf8_lossy(
        &MATRIX
            .run(&["exec", "-T", "app", "printenv", "SIMMER_ADMIN_TOKEN"])
            .stdout,
    )
    .trim()
    .to_string();
    let out = Command::new("curl")
        .args(["-sf", "-H", &format!("Authorization: Bearer {token}")])
        .arg(format!("{ADMIN}/ramps/main/routes"))
        .output()
        .expect("curl");
    assert!(out.status.success(), "GET /ramps/main/routes failed");
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("/ramps/main/routes JSON");
    let routes = v.get("routes").unwrap_or(&v).as_array().expect("routes");
    routes
        .iter()
        .find(|r| r["name"] == route)
        .unwrap_or_else(|| panic!("no route {route} in /ramps/main/routes"))["pool"]
        .clone()
}

// ---------------------------------------------------------------------------
// the one test that needs no Docker
// ---------------------------------------------------------------------------

#[test]
fn the_matrix_config_is_valid_and_routes_each_server_by_sender() {
    // Loaded through Simmer's own `config::load`, so everything §4.2 would refuse
    // at the container's startup is refused here first — in seconds, not after a
    // build and a stack that will not come up.
    let cfg = compose::configs::load(MATRIX_CONFIG);

    let routes: Vec<&str> = cfg
        .default_ramp()
        .routes
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(
        routes,
        [
            "mailpit-direct",
            "postfix-plain",
            "postfix-tls-auth",
            "postfix-login-only",
            "postfix-strict",
            "warming-flows",
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
        match simmer::routing::sender_match::match_sender(cfg.default_ramp(), &senders) {
            simmer::routing::sender_match::Match::Rule { rule, .. } => {
                assert_eq!(rule.chain, [route], "{sender}")
            }
            simmer::routing::sender_match::Match::Unmatched => {
                panic!("{sender} matched no rule")
            }
        }
    }
    // tests/e2e_flows.rs's sender: the one warming route, then the baseline.
    let flows = simmer::routing::sender_match::Senders::new(Some("jane@flows.matrix.test"), None);
    match simmer::routing::sender_match::match_sender(cfg.default_ramp(), &flows) {
        simmer::routing::sender_match::Match::Rule { rule, .. } => {
            assert_eq!(rule.chain, ["warming-flows", "mailpit-direct"])
        }
        simmer::routing::sender_match::Match::Unmatched => panic!("flows matched no rule"),
    }
    assert_eq!(
        cfg.default_ramp().default_chain.as_deref(),
        Some(&["mailpit-direct".to_string()][..])
    );

    // The TLS route verifies: that is the point of it.
    let tls = cfg.default_ramp().route("postfix-tls-auth").expect("route");
    assert_eq!(tls.downstream.tls, simmer::config::TlsMode::RequiredVerify);
    assert!(tls.downstream.auth.is_some());
}
