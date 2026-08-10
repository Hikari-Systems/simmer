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

mod support;

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use simmer::config::Config;

const TRAP_WARMING: &str = "http://127.0.0.1:18025";
const TRAP_OVERFLOW: &str = "http://127.0.0.1:18026";
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

    // §6.4 is phase 5. Asserted as *not yet done* rather than left silent, so
    // this test starts failing the moment body rewriting lands and has to be
    // completed rather than forgotten.
    assert!(
        raw.contains("https://oldbrand.com/track"),
        "body rewriting appears to have landed — finish ACCEPTANCE.md §4.3's \
         last row and delete this assertion:\n{raw}"
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
    const EXCLUDED: [&str; 6] = [
        "reply-to",
        "received",
        "message-id",
        "list-unsubscribe",
        "to",
        "return-path",
    ];

    let mut out = Vec::new();
    let mut skipping = false;
    for line in raw.replace("\r\n", "\n").lines() {
        let continuation = line.starts_with(' ') || line.starts_with('\t');
        if continuation {
            if !skipping {
                out.push(line.to_string());
            }
            continue;
        }
        skipping = line
            .split_once(':')
            .map(|(name, _)| EXCLUDED.contains(&name.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        if !skipping {
            out.push(line.to_string());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// driving the stack
// ---------------------------------------------------------------------------

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

/// Move `warmup.started` back by `day` days and re-create the container.
///
/// The `- 1h` is not decoration: landing exactly on a day boundary makes the
/// test a race against its own clock.
fn restart_app_at_day(day: usize) {
    let started =
        chrono::Utc::now() - chrono::Duration::days(day as i64) - chrono::Duration::hours(1);

    *warmup_started().lock().expect("lock") =
        started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let status = compose()
        .args(["up", "-d", "--force-recreate", "--wait", "app"])
        .status()
        .expect("docker compose up");
    assert!(status.success(), "failed to restart app at day {day}");
}

/// The `warmup.started` the `app` container is currently running with.
///
/// Held here rather than passed at each call site because **every** compose
/// invocation has to carry it. Compose re-renders the whole file on every
/// command, and a command that renders `app` differently from the running
/// container will recreate it — so a `docker compose run loadgen` without this
/// variable silently resets the ramp to the compose default mid-test, and the
/// suite then measures the wrong day while looking like it worked.
fn warmup_started() -> &'static Mutex<String> {
    static STARTED: OnceLock<Mutex<String>> = OnceLock::new();
    STARTED.get_or_init(|| Mutex::new(String::new()))
}

fn compose() -> Command {
    let mut c = Command::new("docker");
    c.args(["compose", "--profile", "acceptance"]);
    c.env("SIMMER_CONFIG", "/app/simmer.acceptance.yaml");
    let started = warmup_started().lock().expect("lock").clone();
    if !started.is_empty() {
        c.env("SIMMER_WARMUP_STARTED", started);
    }
    c
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
    #[allow(dead_code)]
    text: String,
}

// ---------------------------------------------------------------------------
// the traps
// ---------------------------------------------------------------------------

/// Truncate the quota tables.
///
/// The exact analogue of `reset_traps`, and needed for the same reason. Quota
/// state lives in Postgres and outlives a container restart by design (§7.4), so
/// a test that re-uses a simulated day another test has already spent finds the
/// allowance gone and watches every message fall through to overflow — which
/// looks precisely like a routing bug. The suite owns this database.
fn reset_quota() {
    let out = Command::new("docker")
        .args(["compose", "exec", "-T", "simmer-db"])
        .args([
            "psql",
            "-U",
            "simmer",
            "-d",
            "simmer",
            "-q",
            "-c",
            "truncate quota_usage, quota_reservation, route_state;",
        ])
        .output()
        .expect("psql");
    assert!(
        out.status.success(),
        "failed to reset quota state: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn reset_traps() {
    for trap in [TRAP_WARMING, TRAP_OVERFLOW] {
        let out = Command::new("curl")
            .args(["-sf", "-X", "DELETE", &format!("{trap}/api/v1/messages")])
            .output()
            .expect("curl");
        assert!(out.status.success(), "failed to reset {trap}");
    }
    // `ACCEPTANCE.md` §6: reset between simulated days, or day 3's assertions
    // see day 2's mail.
    for trap in [TRAP_WARMING, TRAP_OVERFLOW] {
        assert_eq!(count(trap), 0, "{trap} did not reset");
    }
}

fn count(trap: &str) -> usize {
    let body = get(&format!("{trap}/api/v1/messages?limit=1"));
    let v: serde_json::Value = serde_json::from_str(&body).expect("trap JSON");
    v["total"].as_u64().expect("total") as usize
}

/// Poll until the count reaches `want` and then stops moving.
///
/// **Never sleep a fixed interval** — `ACCEPTANCE.md` §6 names this as the single
/// most likely source of flakes. Waiting for the count to be *stable* rather than
/// merely correct is what catches an off-by-one that arrives late: a run that
/// should deliver 5 and delivers 6 would otherwise pass by being read at the
/// right moment.
fn wait_for_count(trap: &str, want: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = count(trap);
    let mut stable_since = Instant::now();

    loop {
        std::thread::sleep(Duration::from_millis(200));
        let now = count(trap);
        if now != last {
            last = now;
            stable_since = Instant::now();
        } else if now == want && stable_since.elapsed() > Duration::from_secs(2) {
            return now;
        } else if stable_since.elapsed() > Duration::from_secs(10) {
            // Settled on the wrong number. Return it; the caller's assertion
            // says what was expected far better than a timeout message would.
            return now;
        }

        if Instant::now() > deadline {
            return now;
        }
    }
}

fn message_ids(trap: &str) -> Vec<String> {
    let body = get(&format!("{trap}/api/v1/messages?limit=500"));
    let v: serde_json::Value = serde_json::from_str(&body).expect("trap JSON");
    v["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| m["ID"].as_str().expect("ID").to_string())
        .collect()
}

/// Every message's raw source, exactly as the receiving server stored it.
fn raw_messages(trap: &str) -> Vec<String> {
    message_ids(trap)
        .iter()
        .map(|id| get(&format!("{trap}/api/v1/message/{id}/raw")))
        .collect()
}

/// The envelope sender each message arrived with, per the trap's own record —
/// not per a header Simmer wrote.
fn return_paths(trap: &str) -> Vec<String> {
    message_ids(trap)
        .iter()
        .map(|id| {
            let body = get(&format!("{trap}/api/v1/message/{id}"));
            let v: serde_json::Value = serde_json::from_str(&body).expect("trap JSON");
            v["ReturnPath"].as_str().unwrap_or_default().to_string()
        })
        .collect()
}

fn get(url: &str) -> String {
    let out = Command::new("curl")
        .args(["-sf", url])
        .output()
        .expect("curl");
    assert!(out.status.success(), "GET {url} failed");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The unfolded value of a header in a raw message.
fn header(raw: &str, name: &str) -> Option<String> {
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<(String, String)> = None;

    for line in raw.replace("\r\n", "\n").lines() {
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, v)) = current.as_mut() {
                v.push(' ');
                v.push_str(line.trim());
            }
            continue;
        }
        if let Some((k, v)) = current.take() {
            headers.entry(k).or_insert(v);
        }
        if let Some((k, v)) = line.split_once(':') {
            current = Some((k.to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    if let Some((k, v)) = current.take() {
        headers.entry(k).or_insert(v);
    }

    headers.get(&name.to_ascii_lowercase()).cloned()
}

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
    assert!(
        acceptance.server.single_recipient_only,
        "the loadgen sends one recipient per transaction (ACCEPTANCE.md §6)"
    );

    // And the identity must be the one the §4.3 assertions are written against.
    let identity = &warming.identity;
    assert_eq!(identity.envelope_from, "bounce@newbrand.com");
    assert_eq!(identity.unstable_headers, ["Reply-To"]);
    assert!(identity.set_headers.contains_key("List-Unsubscribe"));
}
