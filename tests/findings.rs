//! Known defects, pinned as failing checks until they are fixed.
//!
//! Each test wraps a check of the *correct* behaviour in `support::xfail`, so it
//! passes while the defect is present and fails with `XPASS` the moment a fix
//! lands — at which point the fixing commit removes the wrapper and the check
//! becomes an ordinary test. The ids are the test programme's findings table;
//! `DECISIONS.md` will carry each fix.

mod support;

use std::sync::Arc;
use std::time::Duration;

use support::{config_for, FakeDownstream, GrantAllQuota, Script, Simmer};

const BODY: &str = "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n";

/// **F2** — `timeouts.session` must not cancel a relay that is already in flight.
/// Fixed by D-081.
///
/// The session ceiling was a `select!` around the whole session in
/// `src/smtp/mod.rs`, so when it fired during the downstream conversation the
/// relay future was simply dropped. The downstream had *stored* the message; the
/// client was told `421`, would retry, and the message was delivered twice; no
/// commit or release ran, so the warming route's ledger never counted the
/// delivery; and the reservation stayed in the §10.4 registry until the process
/// exited.
///
/// The deadline is now enforced where Simmer waits on the client, never inside a
/// relay. Here the session's second has long passed when the downstream answers,
/// so the relay finishes, the client is told what happened, and the next wait on
/// the client is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f2_a_session_timeout_does_not_abandon_an_in_flight_relay() {
    let down = FakeDownstream::start(Script::with(|s| {
        s.final_dot_delay = Some(Duration::from_secs(3));
    }))
    .await;
    let cfg = config_for(down.addr, "")
        .replace("session: 60s", "session: 1s")
        .replace(
            "timeouts: { connect: 2s, command: 2s, data: 2s }",
            "timeouts: { connect: 2s, command: 2s, data: 5s }",
        );
    let quota = Arc::new(GrantAllQuota::new());
    let simmer = Simmer::start_with_quota(&cfg, quota.clone()).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let verdict = c
        .deliver("jane@oldbrand.com", "bob@example.net", BODY)
        .await;

    assert_eq!(
        down.messages().len(),
        1,
        "the downstream stored the message"
    );
    assert_eq!(
        verdict.code, 250,
        "the downstream stored the message, so the client must be told so — a 4xx \
         makes it retry and the recipient gets it twice: {verdict:?}"
    );

    // The deadline passed during the relay, so the next wait on the client is
    // refused at once, without a command to prompt it.
    let next = c.read_reply().await;
    assert_eq!(next.code, 421, "{next:?}");
    assert!(next.contains("session timeout"), "{next:?}");
    assert!(c.is_closed().await, "421 closes the connection");

    assert_eq!(quota.committed().len(), 1, "committed exactly once");
    assert!(quota.released().is_empty(), "and never released");
    assert!(
        simmer.registry.is_empty(),
        "{} reservation(s) left in the registry",
        simmer.registry.len()
    );
}
