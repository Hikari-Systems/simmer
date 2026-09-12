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
    let s = Scenario {
        name: "S0",
        sink_args: "",
        loadgen: args(&["--count", "2000", "--concurrency", "32"]),
        expected_errors: &[],
        bare_close_ok: false,
    };
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
    let s = Scenario {
        name: "selfcheck",
        sink_args: "--lose-every 50",
        loadgen: args(&["--count", "500", "--concurrency", "16"]),
        expected_errors: &[],
        bare_close_ok: false,
    };
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
    /// Loadgen arguments after the ones every run carries (`--stamp`, `--jsonl`).
    loadgen: Vec<String>,
    /// Substrings of the ERROR lines this scenario provokes on purpose (U7).
    expected_errors: &'static [&'static str],
    /// Port 465 may close without a reply before its handshake (U2).
    bare_close_ok: bool,
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
    STRESS.reset_quota();
    fresh_sink(s.sink_args);
    let container_before = container_state();
    let since = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let before = scrape();
    // From here on `app` may be dead — an OOM kill is exactly what U6 exists to
    // report — so nothing below may assume it answers.

    let sampler = Sampler::start();
    let summary = loadgen(&s.loadgen);
    let quiesced = quiesce();
    let peaks = sampler.stop();

    let after_metrics = try_scrape().unwrap_or_default();
    let sent: Vec<Sent> = reconcile::read_jsonl(&results("loadgen.jsonl"));
    let received = settled_sink_records();
    let sink: serde_json::Value =
        serde_json::from_str(&compose::traps::get(SINK_STATS)).expect("sink stats JSON");
    let after = Baseline::read(&after_metrics);
    // U5's probe: a message after the load stops still gets through.
    let probe = compose::loadgen::run(&STRESS, &["--count", "1", "--tag", "probe"]);
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

    // U8 — every session refused at the ceiling was told so, and counted.
    let told_421 = seen
        .sent
        .iter()
        .filter(|r| r.code == 421 && r.stage == "banner")
        .count() as u64;
    out.push((
        "refusals",
        if told_421 == seen.refused_max_sessions_delta {
            Ok(())
        } else {
            Err(format!(
                "{told_421} clients got 421 at the banner, but \
                 connections_refused{{max_sessions}} rose by {}",
                seen.refused_max_sessions_delta
            ))
        },
    ));

    out
}

fn judge_all(s: &Scenario, seen: &Seen) {
    let results = checks(s, seen);
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

#[derive(Debug, Default, Clone, Copy)]
struct Peaks {
    sessions: f64,
    backends: usize,
    /// cgroup `memory.stat` anon: the process's own memory, without the tmpfs
    /// spill and page cache that `memory.current` would count.
    anon: u64,
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

/// Run the loadgen to completion and return its summary line.
fn loadgen(extra: &[String]) -> String {
    let out = STRESS
        .compose()
        .args(["run", "--rm", "--no-deps", "--no-TTY", "loadgen"])
        .args(["--host", "app", "--port", "25", "--stamp"])
        .args(["--jsonl", "/results/loadgen.jsonl"])
        .args(extra)
        .output()
        .expect("docker compose run loadgen");
    assert!(
        out.status.success(),
        "loadgen failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find(|l| l.starts_with("{\"summary\""))
        .unwrap_or_else(|| panic!("no summary in the loadgen's output:\n{stdout}"))
        .to_string()
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
    let out = Command::new("curl")
        .args(["-sf", &format!("{}/metrics", admin::BASE)])
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
