//! **F1** — a DATA line with no LF must not be buffered without limit. Fixed by
//! D-082.
//!
//! `Session::read_data_inner` read each line with `read_until(b'\n')` and no
//! `take()`, so `MAX_DATA_LINE` was checked only after the whole line was in
//! memory. A single client on an allowed address could grow one session's
//! buffer until `timeouts.data` fired. `max_message_bytes` did not help: it stops
//! bytes being *kept*, not being read into the line. Each read now takes at most
//! one byte past the cap, and the rest of an over-long line is discarded as it
//! arrives.
//!
//! **Its own test binary on purpose.** The evidence is the process's peak
//! resident memory (`VmHWM`), and every other test in a shared binary would add
//! to it. The client streams its payload in 64 KiB chunks from one reused buffer,
//! so the client side contributes nothing either.

mod support;

use std::time::Duration;

use support::{config_for, FakeDownstream, Script, Simmer};

/// The process's peak resident set, in bytes.
fn peak_rss() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("procfs");
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .expect("VmHWM");
    kb * 1024
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f1_a_data_line_without_lf_is_not_buffered_whole() {
    const PAYLOAD: usize = 64 * 1024 * 1024;
    // Generous: a fixed reader needs a line buffer of MAX_DATA_LINE (64 KiB)
    // plus the session's ordinary working set, nowhere near this.
    const ALLOWED_GROWTH: u64 = 16 * 1024 * 1024;

    let down = FakeDownstream::start(Script::default()).await;
    let cfg = config_for(down.addr, "").replace(
        "timeouts: { command: 5s, data: 5s, session: 60s }",
        "timeouts: { command: 5s, data: 30s, session: 60s }",
    );
    let simmer = Simmer::start(&cfg).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<bob@example.net>").await.code, 250);
    assert_eq!(c.command("DATA").await.code, 354);

    let before = peak_rss();
    let chunk = vec![b'a'; 64 * 1024];
    for _ in 0..PAYLOAD / chunk.len() {
        c.send_raw(&chunk).await;
    }
    // End the monster line and the message. Either way the verdict is 552:
    // the line alone is over max_message_bytes.
    c.send_raw(b"\r\n.\r\n").await;
    let verdict = c.read_reply().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let grew = peak_rss().saturating_sub(before);

    assert_eq!(verdict.code, 552, "{verdict:?}");
    assert!(
        grew < ALLOWED_GROWTH,
        "peak RSS grew by {} MiB while one client sent a {} MiB line with no LF",
        grew / (1024 * 1024),
        PAYLOAD / (1024 * 1024)
    );
}
