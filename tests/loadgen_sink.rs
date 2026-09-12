//! The load tiers' two instruments, proven against each other — no Docker.
//!
//! Every stress and soak verdict rests on three pieces agreeing: the loadgen's
//! record of what each client was told, the sink's record of what each downstream
//! stored, and `tests/compose/reconcile.rs` joining the two. Here the real
//! `loadgen` and `sink` binaries run around an in-process Simmer, in ordinary
//! `cargo test`, and the join is shown to be exact on a clean run and to catch a
//! planted loss. If these disagree here, no load-tier result means anything.

mod compose;
mod support;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use compose::reconcile::{read_jsonl, reconcile, Received, Report, Sent};
use support::{config_for, Simmer};

/// A running `sink`, killed when dropped.
struct Sink {
    child: Child,
    addr: SocketAddr,
    records: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Sink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn sink(extra: &[&str]) -> Sink {
    let dir = tempfile::tempdir().expect("tempdir");
    let records = dir.path().join("sink.jsonl");
    let mut child = Command::new(env!("CARGO_BIN_EXE_sink"))
        .args(["--listen", "127.0.0.1:0", "--jsonl"])
        .arg(&records)
        .args(extra)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn sink");
    // "sink listening <requested> <bound>"
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("sink banner");
    let addr = line
        .split_whitespace()
        .nth(3)
        .and_then(|a| a.parse().ok())
        .unwrap_or_else(|| panic!("unexpected sink banner: {line:?}"));
    Sink {
        child,
        addr,
        records,
        _dir: dir,
    }
}

/// Run the loadgen to completion against `port`, writing its records to `out`.
async fn loadgen(port: u16, out: &Path, extra: &[&str]) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loadgen"));
    cmd.args([
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--stamp",
        "--jsonl",
    ])
    .arg(out)
    .args(extra);
    let output = tokio::task::spawn_blocking(move || cmd.output().expect("run loadgen"))
        .await
        .expect("loadgen task");
    assert!(
        output.status.success(),
        "loadgen failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The sink's records once they stop arriving: at least `min`, and unchanged for
/// half a second. Never a fixed sleep (`ACCEPTANCE.md` §6).
fn settled(path: &Path, min: usize) -> Vec<Received> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = usize::MAX;
    let mut stable_since = Instant::now();
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let n = text.lines().filter(|l| !l.trim().is_empty()).count();
        if n != last {
            last = n;
            stable_since = Instant::now();
        } else if n >= min && stable_since.elapsed() > Duration::from_millis(500) {
            return read_jsonl(&text);
        }
        assert!(
            Instant::now() < deadline,
            "the sink recorded {n} messages; waited for at least {min}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A Simmer in front of the sink, with room for the loadgen's concurrency.
async fn simmer_before(sink: SocketAddr) -> Simmer {
    let cfg = config_for(sink, "")
        .replace(
            "pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }",
            "pool: { max_connections: 4, idle_ttl: 60s, max_messages_per_connection: 100 }",
        )
        // Headroom over the loadgen's 16 workers: a worker reconnecting before
        // its previous session has released its permit must not be refused.
        .replace("max_concurrent_sessions: 16", "max_concurrent_sessions: 32");
    Simmer::start(&cfg).await
}

async fn run(extra_sink: &[&str], extra_loadgen: &[&str], expect_at_least: usize) -> Report {
    let sink = sink(extra_sink);
    let simmer = simmer_before(sink.addr).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let sent_path = dir.path().join("sent.jsonl");
    loadgen(simmer.addr.port(), &sent_path, extra_loadgen).await;

    let sent: Vec<Sent> = read_jsonl(&std::fs::read_to_string(&sent_path).expect("sent records"));
    let received = settled(&sink.records, expect_at_least);
    reconcile(&sent, &received, None)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_concurrent_messages_reconcile_exactly() {
    let report = run(&[], &["--count", "200", "--concurrency", "16"], 200).await;
    assert!(report.is_clean(), "{:#?}", report.violations);
    assert_eq!(report.accepted, 200, "{report:?}");
    assert_eq!(report.stored, 200, "{report:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persistent_pipelined_sessions_reconcile_exactly() {
    // Five messages per session with RSET between, commands pipelined — the
    // shapes an application with a connection pool actually sends.
    let report = run(
        &[],
        &[
            "--count",
            "100",
            "--concurrency",
            "8",
            "--per-session",
            "5",
            "--pipelining",
        ],
        100,
    )
    .await;
    assert!(report.is_clean(), "{:#?}", report.violations);
    assert_eq!(report.accepted, 100, "{report:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sink_that_loses_mail_is_caught_end_to_end() {
    // Checking the check: every 20th message is answered 250 and not recorded.
    // A reconciler that passes this run would pass a relay that loses mail.
    let report = run(
        &["--lose-every", "20"],
        &["--count", "200", "--concurrency", "16"],
        190,
    )
    .await;
    let lost = report
        .violations
        .iter()
        .filter(|v| v.contains("nothing arrived"))
        .count();
    assert_eq!(lost, 10, "{:#?}", report.violations);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_downstream_that_hangs_up_after_the_dot_is_ambiguous_not_lost() {
    // §10.2: the sink stores one message in ten and hangs up before answering.
    // Simmer cannot know, so it must answer 451 — never 250 — and never retry
    // past the dot (D-068). The reconciler must accept each of those as
    // ambiguous, and find no duplicate among them.
    let report = run(
        &["--drop-after-dot-pct", "10", "--seed", "7"],
        &["--count", "200", "--concurrency", "16"],
        200,
    )
    .await;
    assert!(report.is_clean(), "{:#?}", report.violations);
    assert!(report.ambiguous > 0, "the fault never fired: {report:?}");
    assert_eq!(
        report.deferred, report.ambiguous,
        "every ambiguous delivery is a 451, and nothing else was deferred: {report:?}"
    );
    assert_eq!(report.accepted + report.deferred, 200, "{report:?}");
}
