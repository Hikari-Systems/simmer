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

use support::{config_for, xfail, FakeDownstream, GrantAllQuota, Script, Simmer};

const BODY: &str = "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n";

/// **F2** — `timeouts.session` cancels a relay that is already in flight.
///
/// The session ceiling is a `select!` around the whole session in
/// `src/smtp/mod.rs`, so when it fires during the downstream conversation the
/// relay future is simply dropped. Here the downstream has *stored* the message
/// and answers late. The client is told `421`, will retry, and the message is
/// delivered twice; no commit or release runs, so the warming route's ledger
/// never counts the delivery; and the reservation stays in the §10.4 registry
/// until the process exits.
///
/// The check asserts what should happen: every reservation resolved exactly
/// once, nothing left in the registry, and a stored message never answered with
/// a retryable code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f2_a_session_timeout_does_not_abandon_an_in_flight_relay() {
    let because = [
        "the downstream stored the message",
        "neither committed nor released",
        "left in the registry",
    ];
    xfail("F2", &because, async {
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

        // Long enough for the downstream to finish answering, and for a relay
        // that was allowed to complete to have committed.
        tokio::time::sleep(Duration::from_secs(4)).await;

        let stored = down.messages().len();
        assert!(
            !(stored == 1 && (400..500).contains(&verdict.code)),
            "the downstream stored the message but the client was told {} — it will \
             retry and the recipient gets it twice",
            verdict.code
        );
        assert_eq!(
            quota.committed().len() + quota.released().len(),
            1,
            "the reservation was neither committed nor released"
        );
        assert!(
            simmer.registry.is_empty(),
            "{} reservation(s) left in the registry",
            simmer.registry.len()
        );
    })
    .await;
}
