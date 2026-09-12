//! `SPEC.md` §12.3's acceptance tier — the design in `docs/ACCEPTANCE.md`, built.
//!
//! > compose stack with real Postgres; a warm-up walked across simulated day
//! > boundaries by manipulating `warmup.started`; verifying fall-through to
//! > overflow at exhaustion; verifying the cutover invariant by sending the same
//! > logical message under both arrangements of §1.1 and asserting
//! > byte-equivalent downstream output.
//!
//! Every tier below this one substitutes something: the unit tests substitute
//! the network, the integration tests substitute the downstream with an
//! in-process fake, the quota tests substitute the relay. This tier substitutes
//! nothing — real Postgres, real TCP, a real SMTP server on the other end, and
//! Simmer in the container it ships in.
//!
//! # Running it
//!
//! ```sh
//! docker compose --profile acceptance up -d --build
//! cargo test --test acceptance -- --ignored --test-threads=1
//! ```
//!
//! `--ignored` because it needs Docker and takes minutes; `--test-threads=1`
//! because the tests share one compose stack and one pair of traps, and two of
//! them restart `app` underneath everything else.

mod compose;

use std::process::Command;

use compose::mail::header;
use compose::stack::ACCEPTANCE;
use compose::traps::{Trap, OVERFLOW, WARMING};
use simmer::config::Config;

const TRAP_WARMING: Trap = WARMING;
const TRAP_OVERFLOW: Trap = OVERFLOW;
const ACCEPTANCE_CONFIG: &str = "simmer.acceptance.yaml";

// ---------------------------------------------------------------------------
// the ramp (§4.1, §4.2)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the acceptance compose profile"]
fn a_warming_route_carries_exactly_its_allowance_across_a_multi_day_ramp() {
    // The claim nothing else in the suite can make. §7.2's day index is elapsed
    // duration from `warmup.started`, so moving that instant backwards moves the
    // route forwards through its schedule — one container restart per simulated
    // day, because §2.2 rules out a hot reload.
    let schedule = warming_schedule();
    assert!(!schedule.is_empty(), "the acceptance schedule is empty");

    // Enough excess to prove the ceiling holds and that the excess goes
    // somewhere, without making the run long.
    const MARGIN: usize = 3;

    // Once, before the walk — not per day. Each simulated day is its own
    // `day_index` and therefore its own row, and clearing them mid-ramp would
    // erase the very accounting the test is checking.
    reset_quota();

    for (day, allowance) in schedule.iter().enumerate() {
        let allowance = *allowance as usize;
        restart_app_at_day(day);
        reset_traps();

        let replies = loadgen(&[
            "--count",
            &(allowance + MARGIN).to_string(),
            "--tag",
            &format!("day{day}"),
        ]);

        // §10.1: everything is accepted. The excess is *routed elsewhere*, not
        // refused — that is the difference between a ramp and a rate limit.
        for r in &replies {
            assert_eq!(r.code, 250, "day {day}: {r:?}");
        }

        let warming = wait_for_count(TRAP_WARMING, allowance);
        let overflow = wait_for_count(TRAP_OVERFLOW, MARGIN);

        // Not one more. This is the entire purpose of the component.
        assert_eq!(
            warming, allowance,
            "day {day}: warming route carried {warming}, allowance is {allowance}"
        );
        assert_eq!(
            overflow, MARGIN,
            "day {day}: overflow carried {overflow}, expected the {MARGIN} over allowance"
        );
    }
}

#[test]
#[ignore = "needs the acceptance compose profile"]
fn the_two_routes_are_physically_different_servers_under_different_identities() {
    // §4.2. Which container holds the message is the primary evidence: a header
    // Simmer wrote only proves it *said* which route it used.
    let allowance = warming_schedule()[0] as usize;
    restart_app_at_day(0);
    reset_quota();
    reset_traps();

    loadgen(&["--count", &(allowance + 2).to_string(), "--tag", "routing"]);

    // Assert the counts before looking at content. Without this the loops below
    // are vacuous on an empty trap — which is exactly how this test passed for a
    // while against a stack whose ramp was silently sitting on the wrong day.
    assert_eq!(wait_for_count(TRAP_WARMING, allowance), allowance);
    assert_eq!(wait_for_count(TRAP_OVERFLOW, 2), 2);

    for raw in raw_messages(TRAP_WARMING) {
        assert!(
            raw.contains("X-Simmer-Route: warming-newbrand\r\n"),
            "a message in the warming trap is not from the warming route:\n{raw}"
        );
        assert!(raw.contains("<sales@newbrand.com>"), "{raw}");
        assert!(
            !raw.contains("mail.established.com"),
            "routes crossed:\n{raw}"
        );
    }

    for raw in raw_messages(TRAP_OVERFLOW) {
        assert!(
            raw.contains("X-Simmer-Route: overflow-established\r\n"),
            "a message in the overflow trap is not from the overflow route:\n{raw}"
        );
        assert!(raw.contains("<news@mail.established.com>"), "{raw}");
        assert!(
            !raw.contains("sales@newbrand.com"),
            "routes crossed:\n{raw}"
        );
    }

    // §4.1's envelope evidence, from the trap rather than from Simmer's own log.
    for envelope in return_paths(TRAP_WARMING) {
        assert_eq!(envelope, "bounce@newbrand.com");
    }
    for envelope in return_paths(TRAP_OVERFLOW) {
        assert_eq!(envelope, "bounce@mail.established.com");
    }
}

// ---------------------------------------------------------------------------
// the rewrite (§4.3)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the acceptance compose profile"]
fn the_rewrite_is_what_a_real_mail_server_receives() {
    restart_app_at_day(0);
    reset_quota();
    reset_traps();
    loadgen(&["--count", "1", "--tag", "rewrite"]);
    assert_eq!(
        wait_for_count(TRAP_WARMING, 1),
        1,
        "the message never arrived"
    );

    let raw = raw_messages(TRAP_WARMING).remove(0);

    // §6.2 — the reputation-bearing rewrite, display name carried through.
    assert!(
        raw.contains("From: Jane Smith <sales@newbrand.com>\r\n"),
        "From: was not rewritten:\n{raw}"
    );
    // §6.2 — the Message-ID domain matches the outbound sending domain.
    let message_id = header(&raw, "Message-ID").expect("a Message-ID");
    assert!(
        message_id.ends_with("@newbrand.com>"),
        "Message-ID is not on the sending domain: {message_id}"
    );
    // §6.2, §6.6 — Reply-To preserves the original mailbox. Migration-only, and
    // declared in the route's unstable_headers.
    assert_eq!(
        header(&raw, "Reply-To").as_deref(),
        Some("jane@oldbrand.com"),
        "{raw}"
    );
    // §6.2 — weighted heavily by mailbox providers for bulk.
    assert!(header(&raw, "List-Unsubscribe").is_some(), "{raw}");
    assert_eq!(
        header(&raw, "List-Unsubscribe-Post").as_deref(),
        Some("List-Unsubscribe=One-Click"),
        "{raw}"
    );

    // remove_headers. `Return-Path` comes back — the *receiving* server writes
    // its own from the envelope — so the assertion is that the client's is gone,
    // which is visible in it now naming the rewritten sender.
    assert!(!raw.contains("bounces@oldbrand.com"), "{raw}");
    assert!(
        header(&raw, "X-Mailer").is_none(),
        "X-Mailer survived:\n{raw}"
    );

    // §6.5, unconditional and with no config switch. A regression here is
    // invisible in every other tier and expensive in production: a *failing*
    // signature is treated more harshly by filters than an absent one.
    for artefact in [
        "DKIM-Signature",
        "Authentication-Results",
        "ARC-Seal",
        "ARC-Message-Signature",
        "ARC-Authentication-Results",
    ] {
        assert!(
            header(&raw, artefact).is_none(),
            "{artefact} survived:\n{raw}"
        );
    }

    // §6.1 step 8 — Simmer names itself in the trace.
    assert!(
        raw.contains("by simmer.acceptance with ESMTPA id "),
        "no Received: header naming Simmer:\n{raw}"
    );

    // §6.4 — `ACCEPTANCE.md` §4.3's last row, and the only tier that reads a
    // rewritten body back off a real mail server rather than out of a buffer.
    assert!(
        raw.contains("https://newbrand.com/track"),
        "the body link was not rewritten:\n{raw}"
    );
    assert!(
        !raw.contains("oldbrand.com/track"),
        "the old link survived somewhere in the message:\n{raw}"
    );
    // The sentence around it is untouched — §6.4 replaces what the pattern
    // matched, and a decode/re-encode round trip must not disturb the rest.
    assert!(
        raw.contains("Your order has shipped."),
        "the rest of the body did not survive the round trip:\n{raw}"
    );
}

// ---------------------------------------------------------------------------
// the cutover invariant (§4.4) — the suite's centrepiece
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the acceptance compose profile"]
fn both_arrangements_of_the_cutover_invariant_produce_the_same_output() {
    // §1.1's whole thesis, and the only test that states it against real mail
    // servers. Send the same logical message twice — once as an application that
    // has not been reconfigured yet, once as one that has — and require the
    // recipient to see the same thing.
    restart_app_at_day(0);
    reset_quota();
    reset_traps();

    // Arrangement A: the app still sends the old identity.
    loadgen(&[
        "--count",
        "1",
        "--tag",
        "arrangement-a",
        "--from-header",
        "Jane Smith <jane@oldbrand.com>",
    ]);
    assert_eq!(
        wait_for_count(TRAP_WARMING, 1),
        1,
        "arrangement A never arrived"
    );
    let a = raw_messages(TRAP_WARMING).remove(0);

    reset_traps();

    // Arrangement B: the app has been cut over and sends the target identity.
    // The envelope sender still matches the same §5.4 rule, because otherwise
    // the message would take a different chain and the comparison would be
    // about routing rather than about rewriting.
    loadgen(&[
        "--count",
        "1",
        "--tag",
        "arrangement-b",
        "--from-header",
        "Jane Smith <sales@newbrand.com>",
    ]);
    assert_eq!(
        wait_for_count(TRAP_WARMING, 1),
        1,
        "arrangement B never arrived"
    );
    let b = raw_messages(TRAP_WARMING).remove(0);

    let a = comparable(&a);
    let b = comparable(&b);
    assert_eq!(a, b, "the two arrangements of §1.1 produced different mail");
}

// ---------------------------------------------------------------------------
// inbound TLS (D-070)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the acceptance compose profile"]
fn submission_on_587_is_verified_tls_and_refuses_plaintext_auth() {
    // The inbound half of the "no real-certificate TLS test" gap. The loadgen
    // verifies the served certificate against the CA `tls-init` minted for this
    // stack, by name, so a pass means the configured certificate was the one
    // presented — through the image's own certificate loading, in the container
    // it ships in, as UID 1000 against a volume-mounted key.
    restart_app_at_day(0);
    reset_quota();
    reset_traps();

    let replies = loadgen(&[
        "--count",
        "1",
        "--tag",
        "tls",
        "--port",
        "587",
        "--starttls",
    ]);
    assert!(
        replies.iter().all(|r| r.code == 250),
        "submission over STARTTLS failed: {replies:?}"
    );
    assert_eq!(
        wait_for_count(TRAP_WARMING, 1),
        1,
        "the message never arrived"
    );
    let raw = raw_messages(TRAP_WARMING).remove(0);
    // RFC 3848, as the real mail server received it: encrypted and
    // authenticated. The top Received: is Simmer's; the trap prepends its own
    // above it, so look for Simmer's by its `by`.
    let ours = raw
        .lines()
        .find(|l| l.starts_with("Received: from loadgen.acceptance"))
        .unwrap_or_else(|| panic!("no Received: from Simmer:\n{raw}"));
    assert!(
        ours.contains("by simmer.acceptance with ESMTPSA id"),
        "{ours}"
    );

    // And 587's defaults hold: starttls_required means AUTH before the
    // handshake is 530, so a plaintext submission never gets as far as sending
    // its password.
    let refused = loadgen(&["--count", "1", "--tag", "plain587", "--port", "587"]);
    assert!(
        refused
            .iter()
            .all(|r| r.code == 0 && r.text.contains("530")),
        "plaintext AUTH on 587 was not refused with 530: {refused:?}"
    );
}

/// Strip everything D-002 excludes from §12.3's comparison, plus what the
/// receiving server added.
///
/// - **`Reply-To`** — named in the route's `unstable_headers`. It is *defined*
///   as differing between exactly these two arrangements; that is what declaring
///   it means (§6.6).
/// - **`Received`** — Simmer's own (§6.1 step 8) and Mailpit's.
/// - **`Message-ID`** — rendered from `{{uuid}}`, one of §6.6's volatile
///   variables.
/// - **`List-Unsubscribe`** — also carries a `{{uuid}}`.
/// - **`To`** and **`Return-Path`** — the harness sends to a per-run recipient so
///   the two runs are distinguishable, and the receiving server writes its own
///   `Return-Path`. Neither is Simmer's output.
fn comparable(raw: &str) -> Vec<String> {
    compose::mail::without_headers(
        raw,
        &[
            "reply-to",
            "received",
            "message-id",
            "list-unsubscribe",
            "to",
            "return-path",
        ],
    )
}

// ---------------------------------------------------------------------------
// driving the stack
// ---------------------------------------------------------------------------

// The helpers below are the shared compose harness (`tests/compose/`), named as
// this suite always named them so its tests read unchanged.

fn compose() -> Command {
    ACCEPTANCE.compose()
}

fn restart_app_at_day(day: usize) {
    ACCEPTANCE.restart_app_at_day(day);
}

fn reset_quota() {
    ACCEPTANCE.reset_quota();
}

fn reset_traps() {
    TRAP_WARMING.reset();
    TRAP_OVERFLOW.reset();
}

fn wait_for_count(trap: Trap, want: usize) -> usize {
    trap.wait_for_count(want)
}

fn raw_messages(trap: Trap) -> Vec<String> {
    trap.raw_messages()
}

fn return_paths(trap: Trap) -> Vec<String> {
    trap.return_paths()
}

/// The warming route's schedule, read from the same file Simmer reads.
///
/// This is why the expected numbers exist once. A schedule written down in the
/// test as well as in the config drifts, and the day it does is the day the
/// acceptance suite starts lying.
fn warming_schedule() -> Vec<i64> {
    let cfg = load_acceptance_config();
    let route = cfg
        .routes
        .iter()
        .find(|r| r.name == "warming-newbrand")
        .expect("the acceptance config has a warming-newbrand route");
    route
        .warmup
        .as_ref()
        .expect("a warming route has a warmup block")
        .schedule
        .default
        .clone()
}

fn load_acceptance_config() -> Config {
    // §4.2 reads `server.tls`'s files, which in the stack live in the
    // `acceptance-tls` volume. A certificate minted here stands in for them, so
    // the drift guard needs no Docker. Leaked deliberately: the `Config` it
    // validates does not outlive the process, and neither should the files.
    if std::env::var("SIMMER_TLS_DIR").is_err() {
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

    // The interpolated values do not matter for reading the schedule, but they
    // have to resolve or §4 refuses to load the file at all.
    for (k, v) in [
        ("SIMMER_WARMUP_STARTED", "2026-08-01T00:00:00Z"),
        ("DATABASE_URL", "postgres://simmer:simmer@127.0.0.1:5433/simmer"),
        ("SIMMER_ADMIN_TOKEN", "acceptance"),
        (
            "SIMMER_CFAPP_HASH",
            "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ),
    ] {
        if std::env::var(k).is_err() {
            std::env::set_var(k, v);
        }
    }
    simmer::config::load(ACCEPTANCE_CONFIG).unwrap_or_else(|e| {
        panic!("{ACCEPTANCE_CONFIG} is invalid:\n{e}");
    })
}

/// Run the loadgen inside the compose network and parse its JSON.
fn loadgen(extra: &[&str]) -> Vec<LoadgenReply> {
    let mut cmd = compose();
    // `--no-deps`: the loadgen declares `depends_on: app`, and resolving that
    // dependency is enough to make compose reconcile `app` against a freshly
    // rendered config. `restart_app_at_day` has already waited for it to be
    // healthy; nothing here needs compose to check again.
    cmd.args(["run", "--rm", "--no-deps", "--no-TTY", "loadgen"])
        .args(["--host", "app", "--port", "25"])
        .args(extra);

    let out = cmd.output().expect("docker compose run loadgen");
    assert!(
        out.status.success(),
        "loadgen failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout
        .lines()
        .find(|l| l.starts_with('['))
        .unwrap_or_else(|| panic!("no JSON in loadgen output:\n{stdout}"));
    serde_json::from_str(json).unwrap_or_else(|e| panic!("loadgen JSON: {e}\n{json}"))
}

#[derive(Debug, serde::Deserialize)]
struct LoadgenReply {
    #[allow(dead_code)]
    recipient: String,
    code: u16,
    text: String,
}

// ---------------------------------------------------------------------------
// the traps
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// the one test that needs no Docker
// ---------------------------------------------------------------------------

#[test]
fn the_acceptance_config_and_the_shipped_config_stay_in_step() {
    // `ACCEPTANCE.md` §8 question 2: a second config file can drift from the
    // first. This is the containment — it runs in the ordinary `cargo test`, not
    // behind --ignored, so a change to one file that is not mirrored in the other
    // fails immediately rather than the next time somebody runs Docker.
    let acceptance = load_acceptance_config();

    let shipped_names = ["warming-newbrand", "overflow-established"];
    let acceptance_names: Vec<&str> = acceptance.routes.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        acceptance_names, shipped_names,
        "the acceptance config's routes no longer match the shipped config's"
    );

    // The three ways the acceptance config is *allowed* to differ, asserted so
    // that a fourth has to be deliberate.
    let warming = &acceptance.routes[0];
    assert_eq!(warming.downstream.host, "trap-warming");
    assert!(
        warming
            .warmup
            .as_ref()
            .expect("warmup")
            .schedule
            .default
            .len()
            <= 4,
        "the acceptance schedule is one container restart per entry; keep it short"
    );
    assert_eq!(
        acceptance.server.max_recipients, 1,
        "the loadgen sends one recipient per transaction (ACCEPTANCE.md §6, D-047)"
    );

    // And the identity must be the one the §4.3 assertions are written against.
    let identity = &warming.identity;
    assert_eq!(identity.envelope_from, "bounce@newbrand.com");
    assert_eq!(identity.unstable_headers, ["Reply-To"]);
    assert!(identity.set_headers.contains_key("List-Unsubscribe"));
    // §4.3's last row needs a rule to assert against, and the loadgen's body
    // carries the link it matches.
    assert_eq!(identity.body_rewrites.len(), 1, "§6.4 needs a rule here");
    assert_eq!(
        identity.body_rewrites[0].replacement,
        "https://newbrand.com/"
    );
}
