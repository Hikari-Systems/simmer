//! T3 — stress (docs/TESTING.md; the test programme's step 4).
//!
//! `app` at its real ceilings — 64 sessions, pools of 4 and 8, 10 database
//! connections — under 2 CPUs and 1 GiB, driven well past them, with the counting
//! `sink` as both routes' downstream. Every scenario ends with the same checks,
//! the programme's U1–U8, each judged against `test/known-findings.json`.
//! Throughput is reported, never gated: the bar is that the bounds and the replies
//! hold, not that it is fast.
//!
//! ```sh
//! C="docker compose -f docker-compose.yml -f test/compose/acceptance.yml \
//!    -f test/compose/stress.yml --profile acceptance --profile stress"
//! SIMMER_CONFIG=/config/simmer.stress.yaml $C build
//! SIMMER_CONFIG=/config/simmer.stress.yaml $C up -d --wait app sink
//! cargo test --test stress -- --ignored --test-threads=1 --nocapture
//! ```

mod compose;

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use compose::admin;
use compose::findings::judge;
use compose::loadgen::Reply;
use compose::reconcile::{self, Received, Sent};
use compose::stack::STRESS;

const STRESS_CONFIG: &str = "test/config/simmer.stress.yaml";

/// The sink's stats, published by `test/compose/stress.yml`.
const SINK_STATS: &str = "http://127.0.0.1:18081/";

// ---------------------------------------------------------------------------
// the one test that needs no Docker
// ---------------------------------------------------------------------------

#[test]
fn the_stress_config_is_valid_and_keeps_the_real_ceilings() {
    // The stress bar is "ten times the configured ceilings", so the ceilings are
    // the production ones or the tier measures a different system.
    let c = Ceilings::read();
    assert_eq!(
        (c.sessions, c.warming_pool, c.overflow_pool, c.db),
        (64, 4, 8, 10)
    );
}

// ---------------------------------------------------------------------------
// the harness's own proof
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the stress compose profile"]
fn s0_a_clean_run_passes_every_check() {
    // Well inside the ceilings' reach: 32 clients against a pool of 4. Nothing
    // here should fail, so a failure is the harness's or a real regression's.
    let _logs = STRESS.logs_on_failure();
    let s = Scenario::sending("S0", "", &["--count", "2000", "--concurrency", "32"]);
    let seen = run(&s);
    assert_eq!(seen.sent.len(), 2000, "the loadgen recorded every message");
    judge_all(&s, &seen);
}

#[test]
#[ignore = "needs the stress compose profile"]
fn the_accounting_check_catches_a_sink_that_loses_mail() {
    // Checking the checks: a sink that answers 250 and keeps nothing for one
    // message in fifty must fail U3, or a passing U3 proves nothing.
    let _logs = STRESS.logs_on_failure();
    let s = Scenario::sending(
        "selfcheck",
        "--lose-every 50",
        &["--count", "500", "--concurrency", "16"],
    );
    let seen = run(&s);
    let report = reconcile::reconcile(&seen.sent, &seen.received, Some(seen.ambiguous_delta));
    let lost = report
        .violations
        .iter()
        .filter(|v| v.contains("nothing arrived"))
        .count();
    assert_eq!(lost, 10, "{:#?}", report.violations);
}

// ---------------------------------------------------------------------------
// S1 — connection flood
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the stress compose profile"]
fn s1_a_connection_flood_is_bounded_and_every_client_is_answered() {
    // 640 clients at once across all three listeners, each reading the banner and
    // then holding its socket open. Past 64 the rest must get 421 4.3.2, that 421
    // count must equal connections_refused{max_sessions} (U8), and no client may
    // be dropped without a reply (U2) — an accepted-then-idle client is ended by
    // the command timeout with a 421, not a bare close.
    let _logs = STRESS.logs_on_failure();
    // Each flood gets its own tag, so the per-message ids do not collide across
    // the three loadgens' record files. --hold 25s outlasts the 15 s command
    // timeout, so an accepted-then-idle client is ended by the server, not by
    // itself. 465 speaks implicit TLS, verified through the OS trust store.
    let silent = |tag: &'static str, port: &str, n: &str, tls: &[&str]| {
        let mut a = vec![
            "--behaviour",
            "silent",
            "--tag",
            tag,
            "--port",
            port,
            "--count",
            n,
            "--concurrency",
            n,
            "--hold",
            "25s",
        ];
        a.extend_from_slice(tls);
        (tag, args(&a))
    };
    let mut s = Scenario::sending("S1", "", &[]);
    s.loadgens = vec![
        silent("flood25", "25", "400", &[]),
        silent("flood587", "587", "120", &[]),
        silent(
            "flood465",
            "465",
            "120",
            &["--mode", "implicit", "--ca", "os"],
        ),
    ];
    // The implicit port refuses over-cap with a bare close, which §5.1 permits and
    // U8 counts as a refusal.
    s.bare_close_ok = true;
    s.bare_close_is_refusal = true;
    let seen = run(&s);
    assert_eq!(
        seen.peaks.sessions, 64.0,
        "the flood should drive sessions to the cap"
    );
    assert!(
        seen.refused_max_sessions_delta > 400,
        "a 640-client flood should refuse most: {}",
        seen.refused_max_sessions_delta
    );
    judge_all(&s, &seen);
}

// ---------------------------------------------------------------------------
// S2 — pool saturation
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the stress compose profile"]
fn s2_a_slow_downstream_saturates_the_pool_without_overrunning_it() {
    // The sink answers every message 2 s slow. 64 senders pile onto a pool of 4:
    // the sink must never see more than 4 connections (U4), a client that waits
    // out the connect budget gets 451 4.4.5 and its message is never delivered
    // (U1, U3), and nothing is a permanent failure.
    let _logs = STRESS.logs_on_failure();
    let s = Scenario::sending(
        "S2",
        "--slow-ms 2000",
        &["--count", "120", "--concurrency", "64"],
    );
    let seen = run(&s);
    let deferred = seen.sent.iter().filter(|r| r.code == 451).count();
    assert!(
        deferred > 0,
        "a 2 s sink under 64 senders should defer some: {deferred}"
    );
    judge_all(&s, &seen);
}

#[test]
#[ignore = "needs the stress compose profile"]
fn s2b_a_downstream_that_reaps_idle_connections_never_duplicates() {
    // The sink closes idle connections after 1 s, so a pooled connection is dead
    // on reuse (D-068). Simmer's one retry must deliver each message exactly once
    // — no loss, no duplicate (U3) — under a stream that reuses connections hard.
    let _logs = STRESS.logs_on_failure();
    let s = Scenario::sending(
        "S2b",
        "--idle-close-secs 1",
        &[
            "--count",
            "200",
            "--concurrency",
            "8",
            "--per-session",
            "25",
        ],
    );
    let seen = run(&s);
    judge_all(&s, &seen);
}

// ---------------------------------------------------------------------------
// S5 — AUTH storm (F3a)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the stress compose profile"]
fn s5_an_auth_storm_stays_within_memory_and_keeps_answering() {
    // 200 clients over STARTTLS+AUTH, half with the wrong password, all paying
    // argon2's ~19 MiB cost. This is the scenario F3a was expected to OOM — 64
    // sessions × 19 MiB ≈ 1.2 GiB — and it did under glibc. It does not here, for
    // two measured reasons: argon2 is CPU-bound, so on 2 CPUs only a handful
    // verify at once rather than all 64 (peak ~530 MiB, not 1.2 GiB), and jemalloc
    // (D-078) returns each block promptly. So F3a does not reproduce at this core
    // count; the gate is that memory stays clear of the limit and /healthcheck
    // keeps answering while the CPU hashes. A host with far more cores could bring
    // F3a back, which the memory bound would then catch.
    let _logs = STRESS.logs_on_failure();
    let mut s = Scenario::sending(
        "S5",
        "",
        &[
            "--port",
            "587",
            "--starttls",
            "--ca",
            "os",
            "--auth",
            "plain",
            "--wrong-password-pct",
            "50",
            "--count",
            "200",
            "--concurrency",
            "64",
        ],
    );
    s.max_anon_mib = Some(900.0);
    s.max_healthcheck_ms = Some(1000.0);
    // The wrong-password half is answered 535, which is a permitted permanent
    // reply (U1 already exempts 535); nothing here should ERROR.
    let seen = run(&s);
    judge_all(&s, &seen);
}

// ---------------------------------------------------------------------------
// S8 — slowloris
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the stress compose profile"]
fn s8a_a_data_line_with_no_terminator_is_bounded(/* F1 */) {
    // One client reaches DATA and sends 300 MiB with no line ending. Finding F1:
    // the line is buffered whole because MAX_DATA_LINE is checked only after the
    // read, so anon climbs past the bound. A known finding until the length is
    // checked as the line is read.
    let _logs = STRESS.logs_on_failure();
    let mut s = Scenario::sending(
        "S8a",
        "",
        &[
            "--behaviour",
            "no-lf",
            "--count",
            "1",
            "--concurrency",
            "1",
            "--no-lf-bytes",
            "300m",
            "--hold",
            "60s",
        ],
    );
    s.max_anon_mib = Some(96.0);
    // F1's other face: the session is buffering the whole line, so it ends by
    // dropping the client rather than answering — tolerated here (the memory
    // bound is the gate), and how Simmer logs it is not U7's business.
    s.bare_close_ok = true;
    s.expected_errors = &["data line too long", "MAX_DATA_LINE"];
    let seen = run(&s);
    judge_all(&s, &seen);
}

#[test]
#[ignore = "needs the stress compose profile"]
fn s8b_noop_idlers_are_closed_at_the_session_timeout() {
    // Clients that authenticate and then only NOOP, forever. NOOP does not reset
    // the session timeout (it runs from connect), so each is ended at 120 s with a
    // 421 — never left open (a client still holding at its own 140 s deadline is a
    // code-0 record, which U2 fails).
    let _logs = STRESS.logs_on_failure();
    let mut s = Scenario::sending("S8b", "", &[]);
    s.loadgens = vec![(
        "idlers",
        args(&[
            "--behaviour",
            "noop-idle",
            "--count",
            "8",
            "--concurrency",
            "8",
            "--idle-every",
            "5s",
            "--hold",
            "140s",
        ]),
    )];
    let seen = run(&s);
    assert!(
        seen.sent.iter().all(|r| r.code == 421),
        "every idler should be ended with a 421: {:?}",
        seen.sent
            .iter()
            .map(|r| (r.code, r.text.as_str()))
            .collect::<Vec<_>>()
    );
    judge_all(&s, &seen);
}

#[test]
#[ignore = "needs the stress compose profile"]
fn s8c_data_tricklers_hit_the_data_timeout() {
    // Clients that reach DATA and then dribble a byte every 2 s without ever
    // completing a line. The data timeout ends them with a 421; none is left open.
    let _logs = STRESS.logs_on_failure();
    let mut s = Scenario::sending("S8c", "", &[]);
    s.loadgens = vec![(
        "tricklers",
        args(&[
            "--behaviour",
            "data-trickle",
            "--count",
            "8",
            "--concurrency",
            "8",
            "--trickle-every",
            "2s",
            "--hold",
            "120s",
        ]),
    )];
    let seen = run(&s);
    assert!(
        seen.sent.iter().all(|r| r.code == 421),
        "every trickler should be ended with a 421: {:?}",
        seen.sent
            .iter()
            .map(|r| (r.code, r.text.as_str()))
            .collect::<Vec<_>>()
    );
    judge_all(&s, &seen);
}

// ---------------------------------------------------------------------------
// S3 — the database blocked (F4)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the stress compose profile"]
fn s3_a_blocked_database_defers_without_hanging_the_client() {
    // One transaction holds `quota_usage` for 25 s while 64 senders arrive. §7.5's
    // answer with `fail_closed` is `451 4.3.0 quota service unavailable` —
    // temporary, never permanent — and the pool must not exceed its 10
    // connections. Finding F4: nothing sets `lock_timeout` or `statement_timeout`,
    // so a query that already holds a pool connection waits on the lock rather
    // than giving up, and those clients wait far past their own budget.
    let _logs = STRESS.logs_on_failure();
    // A duration, not a count: the lock takes about a second to establish through
    // `docker exec`, and a fixed count against a fast sink would be over before
    // the database was blocked at all. 30 s of load against a 25 s lock also shows
    // the recovery once it is released.
    let mut s = Scenario::sending("S3", "", &["--duration", "30s", "--concurrency", "64"]);
    s.during = Some(block_the_quota_table);
    s.max_reply_ms = Some(10_000.0); // the database connect_timeout (5 s), plus 5
    s.expected_errors = &["quota store unavailable"];
    let seen = run(&s);
    let deferred = seen
        .sent
        .iter()
        .filter(|r| r.code == 451 && r.text.contains("4.3.0"))
        .count();
    assert!(
        deferred > 0,
        "a blocked quota table should defer 451 4.3.0: {:?}",
        seen.sent.iter().map(|r| r.code).collect::<Vec<_>>()
    );
    judge_all(&s, &seen);
}

/// Hold `quota_usage` against all comers for longer than any client's budget.
///
/// A table lock rather than `SELECT … FOR UPDATE`: the row exists only once
/// something has reserved against it, and `run()` has just truncated the table, so
/// a row lock would need a warm-up message — whose delivery would then show up in
/// the accounting as something no loadgen sent.
fn block_the_quota_table() {
    let _ = STRESS
        .compose()
        .args([
            "exec",
            "-T",
            "simmer-db",
            "psql",
            "-U",
            "simmer",
            "-d",
            "simmer",
            "-qAt",
            "-c",
            "begin; lock table quota_usage in access exclusive mode; \
             select pg_sleep(25); commit;",
        ])
        .output();
}

// ---------------------------------------------------------------------------
// S4 — large messages (F3b)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the stress compose profile"]
fn s4_large_messages_stay_within_memory() {
    // 4 concurrent 20 MiB messages whose bodies the route rewrites, so §6.4 copies
    // the whole body rather than passing it through, and §8.1 spills each to
    // tmpfs. Nothing admits DATA by memory (F3b), so what this measures is what
    // the peak actually is; the gate is that it stays clear of the container's
    // 1 GiB and that every message is either delivered or cleanly deferred.
    //
    // Four, not eight: at 8 concurrent this container moves about 0.4 MB/s per
    // stream, so a 20 MiB body brushed the 60 s data timeout and most clients were
    // cut off mid-DATA with a broken pipe — which measured the timeout, not the
    // memory. That run did settle the memory question, though: anon peaked at
    // 23 MiB for 8 × 20 MiB of body, because the buffer is on tmpfs, not the heap.
    let _logs = STRESS.logs_on_failure();
    let mut s = Scenario::sending(
        "S4",
        "",
        &["--count", "8", "--concurrency", "4", "--size", "20m"],
    );
    s.max_anon_mib = Some(900.0);
    let seen = run(&s);
    // Measured against the bodies in flight at once, not the whole run — and the
    // bodies themselves are on tmpfs (§8.1), so what this ratio describes is the
    // engine's own working set beside them.
    let bodies_mib = 4.0 * 20.0;
    eprintln!(
        "  S4: peak anon {:.0} MiB for 4 concurrent 20 MiB bodies ({:.1}x, bodies on tmpfs)",
        seen.peaks.anon as f64 / 1_048_576.0,
        seen.peaks.anon as f64 / 1_048_576.0 / bodies_mib
    );
    judge_all(&s, &seen);
}

// ---------------------------------------------------------------------------
// S9 — a relay cancelled mid-flight (F2)
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the stress compose profile"]
fn s9_a_relay_cancelled_by_the_session_timeout_is_accounted_for() {
    // The session timeout runs from connect, so a short one fires while a relay is
    // still in flight: the sink holds the dot for 30 s, the session is cut at 20.
    // Finding F2: the downstream has the message, the client is told 421, and the
    // reservation is left behind — loss and leak, both of which the accounting and
    // baseline checks see.
    let _logs = STRESS.logs_on_failure();
    let mut s = Scenario::sending(
        "S9",
        "--stall-at-dot-pct 100 --stall-secs 30",
        &["--count", "8", "--concurrency", "8"],
    );
    s.app_env = &[("SIMMER_SESSION_TIMEOUT", "20s")];
    s.expected_errors = &["session timeout", "reservation"];
    // The sink stalls *every* message at the dot, so a probe would be answered by
    // the injected fault rather than by Simmer's state. The rest of U5 — the
    // reservations, the rows, the pool — is what this scenario is about.
    s.probe = false;
    let seen = run(&s);
    judge_all(&s, &seen);
}

// ---------------------------------------------------------------------------
// a scenario, and what it observed
// ---------------------------------------------------------------------------

/// One scenario: how the sink misbehaves, what the loadgen sends, and which
/// failures are the scenario's point rather than a finding.
struct Scenario {
    /// `S0`, `S2`, … — the middle of each check's known-findings key,
    /// `stress/<name>/<check>`.
    name: &'static str,
    /// Passed to the sink as `SINK_ARGS` (`--slow-ms 2000`, `--lose-every 50`).
    sink_args: &'static str,
    /// One or more loadgens, run at once, each `(label, args after the shared
    /// ones)`. The label names its own JSONL, so several floods on different
    /// ports do not clobber one record. Every one carries `--stamp`.
    loadgens: Vec<(&'static str, Vec<String>)>,
    /// Substrings of the ERROR lines this scenario provokes on purpose (U7).
    expected_errors: &'static [&'static str],
    /// U2 tolerates a client that got no reply (code 0): the implicit port's
    /// pre-handshake refusal, or a client the scenario means to have dropped.
    bare_close_ok: bool,
    /// U8 counts a no-reply client as a `max_sessions` refusal — true only for a
    /// flood of the implicit port, where a bare close *is* the refusal.
    bare_close_is_refusal: bool,
    /// When set, a `memory` check that cgroup anon stayed under this many MiB —
    /// how S8a (F1) and S5 (F3a) are gated.
    max_anon_mib: Option<f64>,
    /// When set, a `healthcheck` check that `/healthcheck`'s p99 under load stayed
    /// under this many ms — S5's "still answering while argon2 runs".
    max_healthcheck_ms: Option<f64>,
    /// Environment `app` is re-created with for this scenario — S9's short
    /// `SIMMER_SESSION_TIMEOUT`. Empty means the tier's own values.
    app_env: &'static [(&'static str, &'static str)],
    /// Something that must happen *while* the load runs — S3's held database
    /// lock. Started after the quota reset, and joined when the load ends.
    during: Option<fn()>,
    /// When set, a `latency` check that no client waited longer than this for its
    /// reply — S3's "answered within its own budget even with the database blocked".
    max_reply_ms: Option<f64>,
    /// Whether U5 sends its probe message after the load. Off where the scenario's
    /// own sink faults would answer it (S9 stalls every message at the dot), since
    /// a probe that fails for the injected reason says nothing about recovery.
    probe: bool,
}

impl Scenario {
    /// A sending scenario: the loadgen, load args, and the defaults every check
    /// starts from.
    fn sending(name: &'static str, sink_args: &'static str, loadgen: &[&str]) -> Scenario {
        Scenario {
            name,
            sink_args,
            loadgens: vec![("loadgen", args(loadgen))],
            expected_errors: &[],
            bare_close_ok: false,
            bare_close_is_refusal: false,
            max_anon_mib: None,
            max_healthcheck_ms: None,
            app_env: &[],
            during: None,
            max_reply_ms: None,
            probe: true,
        }
    }
}

/// Everything a run saw, for the checks to judge.
struct Seen {
    sent: Vec<Sent>,
    received: Vec<Received>,
    summary: String,
    ambiguous_delta: u64,
    refused_max_sessions_delta: u64,
    peaks: Peaks,
    sink: serde_json::Value,
    quiesced: Result<(), String>,
    after: Baseline,
    probe: Vec<Reply>,
    container_before: String,
    container_after: String,
    log: String,
}

fn run(s: &Scenario) -> Seen {
    recreate_app(s.app_env);
    STRESS.reset_quota();
    fresh_sink(s.sink_args);
    let container_before = container_state();
    let since = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let before = scrape();
    // From here on `app` may be dead — an OOM kill is exactly what U6 exists to
    // report — so nothing below may assume it answers.

    let sampler = Sampler::start();
    // Whatever must happen while the load runs. Started here, after the quota
    // reset, so a scenario that locks the quota table cannot deadlock the
    // truncate that precedes it.
    let during = s.during.map(thread::spawn);
    let summaries = loadgens(&s.loadgens);
    if let Some(handle) = during {
        let _ = handle.join();
    }
    let quiesced = quiesce();
    let peaks = sampler.stop();
    let summary = summaries.join("\n  ");

    let after_metrics = try_scrape().unwrap_or_default();
    let mut sent: Vec<Sent> = Vec::new();
    for (label, _) in &s.loadgens {
        sent.extend(reconcile::read_jsonl::<Sent>(&results(&format!(
            "{label}.jsonl"
        ))));
    }
    let received = settled_sink_records();
    let sink: serde_json::Value =
        serde_json::from_str(&compose::traps::get(SINK_STATS)).expect("sink stats JSON");
    let after = Baseline::read(&after_metrics);
    // U5's probe: a message after the load stops still gets through.
    let probe = if s.probe {
        compose::loadgen::run(&STRESS, &["--count", "1", "--tag", "probe"])
    } else {
        Vec::new()
    };
    let container_after = container_state();
    let log = String::from_utf8_lossy(
        &STRESS
            .run(&["logs", "--no-color", "--since", &since, "app"])
            .stdout,
    )
    .to_string();

    let delta = |series: &str| (value(&after_metrics, series) - value(&before, series)) as u64;
    let seen = Seen {
        ambiguous_delta: delta("simmer_ambiguous_delivery_total"),
        refused_max_sessions_delta: delta(
            r#"simmer_connections_refused_total{reason="max_sessions"}"#,
        ),
        sent,
        received,
        summary,
        peaks,
        sink,
        quiesced,
        after,
        probe,
        container_before,
        container_after,
        log,
    };
    eprintln!(
        "{}: {}\n  peaks: sink warming {} / overflow {}, sessions {}, db backends {}, \
         anon {:.1} MiB",
        s.name,
        seen.summary,
        sink_peak(&seen.sink, ":2525"),
        sink_peak(&seen.sink, ":2526"),
        seen.peaks.sessions,
        seen.peaks.backends,
        seen.peaks.anon as f64 / 1_048_576.0
    );
    if let Some(p99) = seen.peaks.healthcheck_p99_ms() {
        eprintln!("  /healthcheck p99 under load: {p99:.0} ms");
    }
    seen
}

// ---------------------------------------------------------------------------
// U1–U8
// ---------------------------------------------------------------------------

/// Every check, evaluated before any is judged, so a failing run reports all of
/// what it broke rather than the first.
fn checks(s: &Scenario, seen: &Seen) -> Vec<(&'static str, Result<(), String>)> {
    let c = Ceilings::read();
    let mut out = Vec::new();

    // U1 — no permanent failure except the three that are the client's fault or
    // D-008's to map: an oversize message, a wrong password.
    let permanent: Vec<&Sent> = seen
        .sent
        .iter()
        .filter(|r| r.code >= 500 && r.code != 552 && r.code != 535)
        .collect();
    out.push((
        "replies",
        failures(&permanent, |r| {
            format!("{} {} at {}: {}", r.id, r.code, r.stage, r.text)
        }),
    ));

    // U2 — a plaintext listener always answers before it closes.
    let bare: Vec<&Sent> = seen
        .sent
        .iter()
        .filter(|r| r.code == 0 && !s.bare_close_ok)
        .collect();
    out.push((
        "bare-close",
        failures(&bare, |r| {
            format!("{} closed without a reply at {}: {}", r.id, r.stage, r.text)
        }),
    ));

    // U3 — no loss, no duplicates, and every ambiguous delivery counted.
    let report = reconcile::reconcile(&seen.sent, &seen.received, Some(seen.ambiguous_delta));
    out.push((
        "accounting",
        if report.is_clean() {
            Ok(())
        } else {
            Err(format!(
                "{} violations, first {:?}",
                report.violations.len(),
                &report.violations[..report.violations.len().min(10)]
            ))
        },
    ));

    // U4 — the bounds, measured from outside.
    let mut bounds = Vec::new();
    for (port, max) in [(":2525", c.warming_pool), (":2526", c.overflow_pool)] {
        let peak = sink_peak(&seen.sink, port);
        if peak > max as u64 {
            bounds.push(format!(
                "the sink saw {peak} connections on {port}, pool max {max}"
            ));
        }
    }
    if seen.peaks.sessions > c.sessions as f64 {
        bounds.push(format!(
            "{} sessions active, max {}",
            seen.peaks.sessions, c.sessions
        ));
    }
    if seen.peaks.backends > c.db {
        bounds.push(format!(
            "{} database backends, max {}",
            seen.peaks.backends, c.db
        ));
    }
    out.push(("bounds", joined(bounds)));

    // U5 — back to nothing once the load stops, and still serving.
    let mut baseline = Vec::new();
    if let Err(e) = &seen.quiesced {
        baseline.push(e.clone());
    }
    let a = &seen.after;
    if a.reservations_in_flight != 0.0 {
        baseline.push(format!(
            "reservations_in_flight {}",
            a.reservations_in_flight
        ));
    }
    if a.reservation_rows != "0" {
        baseline.push(format!("{} quota_reservation rows", a.reservation_rows));
    }
    if a.reserved != "0" {
        baseline.push(format!("quota_usage.reserved sums to {}", a.reserved));
    }
    if !seen.probe.iter().all(|r| r.code == 250) {
        baseline.push(format!("the probe after the run got {:?}", seen.probe));
    }
    out.push(("baseline", joined(baseline)));

    // U6 — the same container, never OOM-killed.
    out.push((
        "container",
        if seen.container_before == seen.container_after
            && seen.container_after.contains("oom=false")
        {
            Ok(())
        } else {
            Err(format!(
                "before {}, after {}",
                seen.container_before, seen.container_after
            ))
        },
    ));

    // U7 — no panic, and no ERROR the scenario did not ask for.
    let unexpected: Vec<&str> = seen
        .log
        .lines()
        .filter(|l| {
            l.contains("panicked")
                || (l.contains("\"level\":\"ERROR\"")
                    && !s.expected_errors.iter().any(|e| l.contains(e)))
        })
        .collect();
    out.push(("log", failures(&unexpected, |l| l.to_string())));

    // U8 — every session refused at the ceiling was accounted for. A plaintext
    // listener answers the refusal 421 at the banner; the implicit port (465)
    // refuses with a bare TCP close before the handshake (src/smtp/mod.rs), which
    // the client records as a transport failure — so where the scenario permits
    // bare closes, those are refusals too. Both still increment the metric.
    let told_421 = seen
        .sent
        .iter()
        .filter(|r| r.code == 421 && r.stage == "banner")
        .count() as u64;
    let bare_refusals = if s.bare_close_is_refusal {
        seen.sent.iter().filter(|r| r.code == 0).count() as u64
    } else {
        0
    };
    let observed = told_421 + bare_refusals;
    out.push((
        "refusals",
        if observed == seen.refused_max_sessions_delta {
            Ok(())
        } else {
            Err(format!(
                "{told_421} clients got 421 at the banner and {bare_refusals} a bare close, \
                 {observed} in all, but connections_refused{{max_sessions}} rose by {}",
                seen.refused_max_sessions_delta
            ))
        },
    ));

    // Latency — only where a scenario names a bound. §14.1's rule has a corollary:
    // a client must be *answered*, and within its own budget, even when Simmer's
    // dependencies are misbehaving.
    if let Some(max) = s.max_reply_ms {
        let slow: Vec<&Sent> = seen.sent.iter().filter(|r| r.latency_ms > max).collect();
        out.push((
            "latency",
            failures(&slow, |r| {
                format!(
                    "{} waited {:.0} ms to be told {} {}",
                    r.id, r.latency_ms, r.code, r.text
                )
            }),
        ));
    }

    // Memory — only where a scenario names a bound. cgroup anon, so tmpfs spill
    // and page cache are not counted (F1's line buffer and F3a's argon2 blocks
    // are anonymous). Gated against known findings like the rest.
    if let Some(max) = s.max_anon_mib {
        let peak = seen.peaks.anon as f64 / 1_048_576.0;
        out.push((
            "memory",
            if peak <= max {
                Ok(())
            } else {
                Err(format!(
                    "cgroup anon peaked at {peak:.0} MiB, over the {max:.0} MiB bound"
                ))
            },
        ));
    }

    // The control plane stayed responsive while the CPU was busy hashing.
    if let Some(max) = s.max_healthcheck_ms {
        out.push((
            "healthcheck",
            match seen.peaks.healthcheck_p99_ms() {
                None => Err("no /healthcheck sample was taken".to_string()),
                Some(p99) if p99 <= max => Ok(()),
                Some(p99) => Err(format!(
                    "/healthcheck p99 was {p99:.0} ms, over the {max:.0} ms bound"
                )),
            },
        ));
    }

    out
}

fn judge_all(s: &Scenario, seen: &Seen) {
    let mut results = checks(s, seen);

    // When the container was OOM-killed, one root cause fails half the checks —
    // accounting, baseline, the probe — none of which can speak to anything with
    // the process dead. Judge only the checks that are about the kill itself
    // (`container`, and `memory` if the scenario set a bound); the rest are noted
    // and dropped. A scenario that expects the OOM lists `container` (and
    // `memory`) in the known findings, so the finding still lands.
    let oomed = seen.container_after.contains("oom=true");
    if oomed {
        eprintln!("  (container was OOM-killed; judging only container/memory)");
        results.retain(|(check, _)| *check == "container" || *check == "memory");
    }

    for (check, result) in &results {
        eprintln!(
            "  {check:<11} {}",
            match result {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("FAIL {e}"),
            }
        );
    }
    for (check, result) in results {
        judge(&format!("stress/{}/{check}", s.name), result);
    }
}

fn failures<T>(bad: &[T], describe: impl Fn(&T) -> String) -> Result<(), String> {
    if bad.is_empty() {
        return Ok(());
    }
    let first: Vec<String> = bad.iter().take(5).map(describe).collect();
    Err(format!("{} of them, first {first:?}", bad.len()))
}

fn joined(problems: Vec<String>) -> Result<(), String> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

// ---------------------------------------------------------------------------
// the configured ceilings
// ---------------------------------------------------------------------------

/// Read from the config under test, never repeated here.
struct Ceilings {
    sessions: usize,
    warming_pool: usize,
    overflow_pool: usize,
    db: usize,
}

impl Ceilings {
    fn read() -> Ceilings {
        let cfg = compose::configs::load(STRESS_CONFIG);
        let pool = |name: &str| {
            count(
                cfg.route(name)
                    .unwrap_or_else(|| panic!("no route {name}"))
                    .downstream
                    .pool
                    .max_connections,
            )
        };
        Ceilings {
            sessions: count(cfg.server.max_concurrent_sessions),
            warming_pool: pool("warming-newbrand"),
            overflow_pool: pool("overflow-established"),
            db: count(cfg.database.max_connections),
        }
    }
}

fn count<T: TryInto<usize>>(n: T) -> usize
where
    T::Error: std::fmt::Debug,
{
    n.try_into().expect("a count")
}

// ---------------------------------------------------------------------------
// sampling while the load runs
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Peaks {
    sessions: f64,
    backends: usize,
    /// cgroup `memory.stat` anon: the process's own memory, without the tmpfs
    /// spill and page cache that `memory.current` would count.
    anon: u64,
    /// One `GET /healthcheck` latency per sample, in ms — the control plane's
    /// responsiveness while the message path is under load (S5).
    healthcheck_ms: Vec<f64>,
}

impl Peaks {
    fn healthcheck_p99_ms(&self) -> Option<f64> {
        if self.healthcheck_ms.is_empty() {
            return None;
        }
        let mut v = self.healthcheck_ms.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        Some(v[(v.len() as f64 * 0.99) as usize % v.len()])
    }
}

struct Sampler {
    stop: Arc<AtomicBool>,
    handle: thread::JoinHandle<Peaks>,
}

impl Sampler {
    fn start() -> Sampler {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut peaks = Peaks::default();
            let mut tick = 0u64;
            while !flag.load(Ordering::Relaxed) {
                if let Some(n) = backends() {
                    peaks.backends = peaks.backends.max(n);
                }
                if let Some(a) = anon() {
                    peaks.anon = peaks.anon.max(a);
                }
                // A scrape costs a pool-stats read and two queries; every fifth
                // second keeps the harness from becoming part of the load.
                if tick.is_multiple_of(5) {
                    if let Some(body) = try_scrape() {
                        peaks.sessions = peaks.sessions.max(value(&body, "simmer_sessions_active"));
                    }
                }
                // /healthcheck is cheap and touches no argon2, so its latency is
                // whether ordinary requests are starved while the CPU hashes.
                if let Some(ms) = healthcheck_ms() {
                    peaks.healthcheck_ms.push(ms);
                }
                tick += 1;
                thread::sleep(Duration::from_secs(1));
            }
            peaks
        });
        Sampler { stop, handle }
    }

    fn stop(self) -> Peaks {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().expect("the sampler thread")
    }
}

/// One `GET /healthcheck` round trip in ms, or `None` if it did not answer
/// (which the container check catches — this must not make the sampler panic).
fn healthcheck_ms() -> Option<f64> {
    let start = Instant::now();
    let ok = Command::new("curl")
        .args(["-sf", "-o", "/dev/null", "--max-time", "5"])
        .arg(format!("{}/healthcheck", admin::BASE))
        .status()
        .ok()?
        .success();
    ok.then(|| start.elapsed().as_secs_f64() * 1000.0)
}

/// Simmer's connections to Postgres: the TCP ones. `psql` through `exec` comes
/// in over the local socket, so the harness never counts itself.
fn backends() -> Option<usize> {
    let out = STRESS
        .compose()
        .args([
            "exec",
            "-T",
            "simmer-db",
            "psql",
            "-U",
            "simmer",
            "-d",
            "simmer",
        ])
        .args([
            "-qAt",
            "-c",
            "select count(*) from pg_stat_activity where datname = 'simmer' \
             and client_addr is not null",
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn anon() -> Option<u64> {
    let out = STRESS
        .compose()
        .args(["exec", "-T", "app", "cat", "/sys/fs/cgroup/memory.stat"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("anon ")?.trim().parse().ok())
}

// ---------------------------------------------------------------------------
// driving the stack
// ---------------------------------------------------------------------------

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// Start every scenario from a freshly created `app` carrying exactly that
/// scenario's environment.
///
/// Always a re-create rather than a health check, for three reasons: a scenario
/// that changes `app`'s environment (S9's short session timeout) would otherwise
/// leave it in place for the next one, which is D-042's trap in a new guise; a
/// scenario may have left `app` OOM-killed; and a fresh process gives the memory
/// checks a clean baseline. `SIMMER_SESSION_TIMEOUT` always has a value from
/// `test/compose/stress.yml`, so the rendering is identical unless asked otherwise.
fn recreate_app(env: &[(&str, &str)]) {
    let mut cmd = STRESS.compose();
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd
        .args(["up", "-d", "--no-deps", "--force-recreate", "--wait", "app"])
        .output()
        .expect("docker compose up app");
    assert!(
        out.status.success(),
        "app would not come back up: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Clear the results and re-create the sink with this scenario's faults, so its
/// JSONL starts empty and its peaks start at zero.
fn fresh_sink(sink_args: &str) {
    STRESS.run(&[
        "run",
        "--rm",
        "--no-deps",
        "--no-TTY",
        "--entrypoint",
        "sh",
        "loadgen",
        "-c",
        "rm -f /results/*.jsonl",
    ]);
    let out = STRESS
        .compose()
        .env("SINK_ARGS", sink_args)
        .args([
            "up",
            "-d",
            "--no-deps",
            "--force-recreate",
            "--wait",
            "sink",
        ])
        .output()
        .expect("docker compose up sink");
    assert!(
        out.status.success(),
        "re-creating the sink failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while !Command::new("curl")
        .args(["-sf", SINK_STATS])
        .output()
        .is_ok_and(|o| o.status.success())
    {
        assert!(
            Instant::now() < deadline,
            "the sink never answered its stats"
        );
        thread::sleep(Duration::from_millis(200));
    }
}

/// Run every loadgen at once, each writing `/results/<label>.jsonl`, and return
/// their summary lines in the same order. They share the results volume, so a
/// misbehaving flood on 587 and a sending stream on 25 accumulate side by side.
fn loadgens(specs: &[(&'static str, Vec<String>)]) -> Vec<String> {
    let handles: Vec<_> = specs
        .iter()
        .map(|(label, extra)| {
            let label = (*label).to_string();
            let extra = extra.clone();
            thread::spawn(move || one_loadgen(&label, &extra))
        })
        .collect();
    handles
        .into_iter()
        .map(|h| h.join().expect("a loadgen thread"))
        .collect()
}

/// One loadgen to completion. A misbehaving `--behaviour` run still prints its
/// summary and JSONL, so this is the same for senders and floods; a client whose
/// own socket setup fails is a harness fault and panics.
fn one_loadgen(label: &str, extra: &[String]) -> String {
    let out = STRESS
        .compose()
        .args(["run", "--rm", "--no-deps", "--no-TTY", "loadgen"])
        .args(["--host", "app", "--port", "25", "--stamp"])
        .args(["--jsonl", &format!("/results/{label}.jsonl")])
        .args(extra)
        .output()
        .expect("docker compose run loadgen");
    assert!(
        out.status.success(),
        "loadgen {label} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let summary = stdout
        .lines()
        .find(|l| l.starts_with("{\"summary\""))
        .unwrap_or_else(|| panic!("no summary in loadgen {label}'s output:\n{stdout}"));
    format!("{label}: {summary}")
}

fn results(file: &str) -> String {
    let out = STRESS.run(&[
        "run",
        "--rm",
        "--no-deps",
        "--no-TTY",
        "--entrypoint",
        "cat",
        "loadgen",
        &format!("/results/{file}"),
    ]);
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The sink's records once its writer has caught up: read until two reads a
/// second apart agree.
fn settled_sink_records() -> Vec<Received> {
    let mut last = usize::MAX;
    for _ in 0..30 {
        let text = results("sink.jsonl");
        let n = text.lines().filter(|l| !l.trim().is_empty()).count();
        if n == last {
            return reconcile::read_jsonl(&text);
        }
        last = n;
        thread::sleep(Duration::from_secs(1));
    }
    panic!("the sink's record never settled");
}

/// Wait for every session and pooled connection to be released.
fn quiesce() -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let Some(metrics) = try_scrape() else {
            return Err("app stopped answering /metrics (see the container check)".into());
        };
        let sessions = value(&metrics, "simmer_sessions_active");
        let active = pool_active();
        if sessions == 0.0 && active == 0 {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!(
                "a minute after the load: {sessions} sessions and {active} pooled \
                 connections still active"
            ));
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn pool_active() -> u64 {
    let v = admin::get(&STRESS, "/routes");
    let routes = v.get("routes").unwrap_or(&v).as_array().expect("routes");
    routes
        .iter()
        .map(|r| r["pool"]["active"].as_u64().unwrap_or(0))
        .sum()
}

/// What U5 reads once the load has stopped.
struct Baseline {
    reservations_in_flight: f64,
    reservation_rows: String,
    reserved: String,
}

impl Baseline {
    fn read(metrics: &str) -> Baseline {
        Baseline {
            reservations_in_flight: value(metrics, "simmer_reservations_in_flight"),
            reservation_rows: STRESS.psql("select count(*) from quota_reservation"),
            reserved: STRESS.psql("select coalesce(sum(reserved), 0) from quota_usage"),
        }
    }
}

/// `StartedAt`, restarts and the OOM flag: U6 compares before and after.
fn container_state() -> String {
    let id = String::from_utf8_lossy(&STRESS.run(&["ps", "-q", "app"]).stdout)
        .trim()
        .to_string();
    let out = Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{.State.StartedAt}} restarts={{.RestartCount}} oom={{.State.OOMKilled}}",
            &id,
        ])
        .output()
        .expect("docker inspect");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn scrape() -> String {
    try_scrape().expect("GET /metrics")
}

fn try_scrape() -> Option<String> {
    // Bounded: a scrape runs two database queries, and S3 deliberately blocks the
    // database — an untimed curl would hang the sampler for the whole scenario.
    let out = Command::new("curl")
        .args([
            "-sf",
            "--max-time",
            "10",
            &format!("{}/metrics", admin::BASE),
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// One series from a scrape, 0 when it has not been written yet.
fn value(body: &str, series: &str) -> f64 {
    body.lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
        .map(|v| v.trim().parse().expect("a metric value"))
        .unwrap_or(0.0)
}

/// The sink's peak concurrent connections on the listener ending in `port`.
fn sink_peak(stats: &serde_json::Value, port: &str) -> u64 {
    stats["listeners"]
        .as_object()
        .and_then(|l| l.iter().find(|(k, _)| k.ends_with(port)))
        .and_then(|(_, g)| g["peak"].as_u64())
        .unwrap_or(0)
}
