//! D-085/D-086 — a captured message, replayed, arrives as the original did.
//!
//! This is the test the feature exists to pass. Everything else about capture is
//! bookkeeping; the claim that matters is that a record is a faithful enough
//! description of a transaction to reproduce it.
//!
//! The comparison is made at the **downstream**, on the far side of a second
//! Simmer, rather than on the capture file itself:
//!
//! ```text
//!   client ──▶ simmer A ──▶ downstream A        (capture written here)
//!                  │
//!           capture file
//!                  │
//!           server replay ──▶ simmer B ──▶ downstream B
//! ```
//!
//! Asserting `downstream A == downstream B` is a stronger claim than asserting
//! anything about the file: it exercises the record, the reader, the replay
//! client's dot-stuffing and parameter handling, and the whole of B's ingress.
//! Only the `Received:` header differs, because each Simmer adds its own — D-002
//! excludes it from §12.3's byte-equivalence comparison for exactly this reason,
//! and `support::without_received` is the same carve-out.
//!
//! It also pins the rule that **replay adds nothing**: no marker header, no
//! stamp. `loadgen --stamp` is opt-in precisely because an extra header would
//! break this equality (`src/bin/loadgen.rs`).

mod support;

use std::collections::BTreeMap;

use simmer::capture::replay::{self, ReplayArgs, Summary};
use support::{config_for, without_received, FakeDownstream, Script, Simmer};

fn capture_config(down: &FakeDownstream, dir: &std::path::Path) -> String {
    config_for(
        down.addr,
        &format!(
            "capture:\n  directory: \"{}\"\n  max_queue_bytes: 1048576\n",
            dir.display()
        ),
    )
}

/// Build a replay plan covering everything in `dir`, aimed at `port`.
fn plan(dir: &std::path::Path, port: u16) -> ReplayArgs {
    let argv = [
        "replay",
        "--dir",
        dir.to_str().expect("utf-8 tempdir"),
        "--from",
        "2000-01-01T00:00:00Z",
        "--to",
        "2100-01-01T00:00:00Z",
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--confirm",
    ]
    .map(String::from);

    replay::args_from(argv.into_iter(), chrono::Utc::now())
        .expect("a replay invocation")
        .expect("valid arguments")
}

/// Wait until `want` records are on disk.
///
/// Polling rather than a fixed sleep past the writer's one-second idle tick: a
/// sleep passes on an idle machine and fails on a busy one, which is the test
/// that wastes an afternoon in CI.
async fn settle_for(dir: &std::path::Path, want: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let seen = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .filter_map(|e| std::fs::read(e.path()).ok())
            .map(|t| {
                t.split(|&b| b == b'\n')
                    .filter(|l| !l.is_empty())
                    .filter(|l| simmer::capture::Record::from_line(l).is_ok())
                    .count()
            })
            .sum::<usize>();
        if seen >= want {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "waited 30s for {want} captured record(s); only {seen} arrived"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Messages chosen to break a careless implementation: dot-stuffing, an 8-bit
/// body, a line that is exactly a dot's worth, long headers, an empty body, and
/// a body whose last line has no trailing blank.
fn awkward() -> Vec<(&'static str, &'static str)> {
    vec![
        ("plain", "Subject: plain\r\n\r\nhello\r\n"),
        (
            "leading-dot",
            "Subject: dots\r\n\r\n.leading\r\n..doubled\r\n...triple\r\n",
        ),
        (
            "dot-only-line",
            "Subject: dot only\r\n\r\nbefore\r\n.\r\nafter\r\n",
        ),
        ("empty-body", "Subject: empty\r\n\r\n"),
        (
            "no-trailing-blank",
            "Subject: no trailing blank\r\n\r\nlast line has no blank after it\r\n",
        ),
        (
            "long-header",
            "Subject: long\r\nX-Filler: \
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\
             \r\nbody\r\n",
        ),
        (
            "mime",
            "Subject: mime\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
             --b\r\nContent-Type: text/plain\r\n\r\nplain part\r\n\
             --b\r\nContent-Type: text/html\r\n\r\n<p>html part</p>\r\n--b--\r\n",
        ),
    ]
}

#[tokio::test]
async fn a_replayed_message_reaches_the_downstream_exactly_as_the_original_did() {
    let dir = tempfile::tempdir().expect("tempdir");

    // -- the original run ---------------------------------------------------
    let down_a = FakeDownstream::start(Script::default()).await;
    let simmer_a = Simmer::start(&capture_config(&down_a, dir.path())).await;

    for (name, body) in awkward() {
        let mut c = simmer_a.connect().await;
        c.hello().await;
        let r = c
            .deliver("a@oldbrand.com", &format!("{name}@x.test"), body)
            .await;
        assert_eq!(r.code, 250, "{name}: {r:?}");
    }
    settle_for(dir.path(), awkward().len()).await;

    let sent_a = down_a.messages();
    assert_eq!(sent_a.len(), awkward().len());

    // -- the replay ---------------------------------------------------------
    let down_b = FakeDownstream::start(Script::default()).await;
    let simmer_b = Simmer::start(&config_for(down_b.addr, "")).await;

    let args = plan(dir.path(), simmer_b.addr.port());
    let (records, summary) = replay::read_range(&args).expect("read the capture");
    assert_eq!(
        records.len(),
        awkward().len(),
        "every captured message should be selected"
    );

    let summary = replay::run(&args, &records, &BTreeMap::new(), summary).await;
    assert_eq!(summary.sent, awkward().len());
    assert_eq!(
        summary.accepted,
        awkward().len(),
        "every replayed message should be accepted: {summary:?}"
    );
    assert_eq!(summary.exit_code(), 0);

    // -- the comparison -----------------------------------------------------
    let sent_b = down_b.messages();
    assert_eq!(sent_b.len(), sent_a.len());

    // And the fixtures really are different from each other, so "every pair
    // matched" is a claim about fidelity rather than about uniformity.
    let distinct: std::collections::BTreeSet<_> =
        sent_a.iter().map(|m| without_received(&m.body)).collect();
    assert_eq!(distinct.len(), sent_a.len(), "the fixtures must differ");

    // Both sides are ordered by the order they were sent, and the replay reader
    // sorts by (at, id), which is the order they were captured in.
    for (original, replayed) in sent_a.iter().zip(sent_b.iter()) {
        // Not a vacuous comparison: each message is distinct and non-trivial, so
        // an implementation that delivered nothing, or the same thing every
        // time, fails here rather than sailing through the equality below.
        assert!(
            original.body.len() > 20,
            "the fixture must be a real message: {:?}",
            String::from_utf8_lossy(&original.body)
        );
        assert_eq!(
            original.mail_from, replayed.mail_from,
            "the envelope sender must survive"
        );
        assert_eq!(original.recipients, replayed.recipients);
        assert_eq!(
            without_received(&original.body),
            without_received(&replayed.body),
            "the message must arrive byte for byte as it did the first time \
             (recipients {:?})",
            original.recipients
        );
    }
}

#[tokio::test]
async fn an_eight_bit_message_replays_with_its_parameters_and_its_bytes() {
    // BODY=8BITMIME is not cosmetic: without it the target's downstream may
    // refuse the message, so the record has to carry it and the replay has to
    // re-present it.
    let dir = tempfile::tempdir().expect("tempdir");
    let down_a = FakeDownstream::start(Script::default()).await;
    let simmer_a = Simmer::start(&capture_config(&down_a, dir.path())).await;

    let raw: &[u8] = b"Subject: caf\xe9\r\n\r\nCaf\xe9 cr\xe8me, na\xefve\r\n.\r\n";
    let mut c = simmer_a.connect().await;
    c.hello().await;
    assert_eq!(
        c.command("MAIL FROM:<a@oldbrand.com> BODY=8BITMIME")
            .await
            .code,
        250
    );
    assert_eq!(c.command("RCPT TO:<b@x.test>").await.code, 250);
    assert_eq!(c.command("DATA").await.code, 354);
    c.send_raw(raw).await;
    assert_eq!(c.read_reply().await.code, 250);
    settle_for(dir.path(), 1).await;

    let down_b = FakeDownstream::start(Script::default()).await;
    let simmer_b = Simmer::start(&config_for(down_b.addr, "")).await;
    let args = plan(dir.path(), simmer_b.addr.port());
    let (records, summary) = replay::read_range(&args).expect("read");
    assert!(records[0].params.body_8bitmime, "the parameter is recorded");

    let summary = replay::run(&args, &records, &BTreeMap::new(), summary).await;
    assert_eq!(summary.accepted, 1, "{summary:?}");

    let a = down_a.last().expect("a");
    let b = down_b.last().expect("b");
    assert_eq!(without_received(&a.body), without_received(&b.body));
    assert!(
        b.mail_from_params.contains("8BITMIME"),
        "the parameter must reach the second downstream too: {:?}",
        b.mail_from_params
    );
}

#[tokio::test]
async fn the_replay_adds_nothing_to_the_message() {
    // §1.1's byte-equality is the property a replay is worth running for, and an
    // extra header would break it. Asserted directly, not only by the equality
    // above, so the reason survives a refactor of that test.
    let dir = tempfile::tempdir().expect("tempdir");
    let down_a = FakeDownstream::start(Script::default()).await;
    let simmer_a = Simmer::start(&capture_config(&down_a, dir.path())).await;

    let mut c = simmer_a.connect().await;
    c.hello().await;
    c.deliver("a@oldbrand.com", "b@x.test", "Subject: hi\r\n\r\nbody\r\n")
        .await;
    settle_for(dir.path(), 1).await;

    let down_b = FakeDownstream::start(Script::default()).await;
    let simmer_b = Simmer::start(&config_for(down_b.addr, "")).await;
    let args = plan(dir.path(), simmer_b.addr.port());
    let (records, summary) = replay::read_range(&args).expect("read");
    replay::run(&args, &records, &BTreeMap::new(), summary).await;

    let body = String::from_utf8_lossy(&down_b.last().expect("b").body).to_lowercase();
    for marker in ["x-simmer-replay", "x-replay", "x-test-id", "x-simmer"] {
        assert!(!body.contains(marker), "{marker} in the replayed message");
    }
    // Exactly one Received: — simmer B's. Two would mean the replay forwarded
    // the header A added, which would break §6.6's stability on a second pass.
    assert_eq!(
        body.matches("received:").count(),
        1,
        "the replayed message should carry one Received:, added by simmer B"
    );
}

#[tokio::test]
async fn a_range_selects_only_the_records_inside_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let down = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&capture_config(&down, dir.path())).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    c.deliver("a@oldbrand.com", "b@x.test", "Subject: hi\r\n\r\nx\r\n")
        .await;
    settle_for(dir.path(), 1).await;

    // A window that ended before the message arrived.
    let argv = [
        "replay",
        "--dir",
        dir.path().to_str().unwrap(),
        "--from",
        "2000-01-01T00:00:00Z",
        "--to",
        "2000-01-02T00:00:00Z",
        "--host",
        "127.0.0.1",
    ]
    .map(String::from);
    let args = replay::args_from(argv.into_iter(), chrono::Utc::now())
        .unwrap()
        .unwrap();
    let (records, _) = replay::read_range(&args).expect("read");
    assert!(
        records.is_empty(),
        "a record outside [from, to) must not be selected"
    );

    // And the window that does contain it.
    let wide = plan(dir.path(), simmer.addr.port());
    assert_eq!(replay::read_range(&wide).expect("read").0.len(), 1);
}

#[tokio::test]
async fn a_body_omitted_record_is_skipped_rather_than_sent_as_something_else() {
    // Synthesising a body would send bytes that never arrived, which destroys
    // the only property a replay has.
    let dir = tempfile::tempdir().expect("tempdir");
    let down_a = FakeDownstream::start(Script::default()).await;
    let simmer_a = Simmer::start(&config_for(
        down_a.addr,
        &format!(
            "capture:\n  directory: \"{}\"\n  max_body_bytes: 32\n  max_queue_bytes: 1048576\n",
            dir.path().display()
        ),
    ))
    .await;

    let mut c = simmer_a.connect().await;
    c.hello().await;
    c.deliver(
        "a@oldbrand.com",
        "b@x.test",
        &format!("Subject: big\r\n\r\n{}\r\n", "x".repeat(200)),
    )
    .await;
    settle_for(dir.path(), 1).await;

    let down_b = FakeDownstream::start(Script::default()).await;
    let simmer_b = Simmer::start(&config_for(down_b.addr, "")).await;
    let args = plan(dir.path(), simmer_b.addr.port());
    let (records, summary) = replay::read_range(&args).expect("read");
    assert_eq!(records.len(), 1);
    assert!(records[0].body_omitted);

    let summary = replay::run(&args, &records, &BTreeMap::new(), summary).await;
    assert_eq!(summary.skipped_no_body, 1);
    assert_eq!(summary.sent, 0, "nothing should be sent");
    assert!(down_b.messages().is_empty());
}

#[tokio::test]
async fn a_truncated_capture_file_is_replayable_up_to_the_truncation() {
    // How a crash leaves a file. Everything before the cut is still perfectly
    // good, and a replay that refused the lot would be useless exactly when it
    // was most wanted.
    let dir = tempfile::tempdir().expect("tempdir");
    let down_a = FakeDownstream::start(Script::default()).await;
    let simmer_a = Simmer::start(&capture_config(&down_a, dir.path())).await;

    for i in 0..3 {
        let mut c = simmer_a.connect().await;
        c.hello().await;
        c.deliver(
            "a@oldbrand.com",
            &format!("r{i}@x.test"),
            "Subject: hi\r\n\r\nx\r\n",
        )
        .await;
    }
    settle_for(dir.path(), 3).await;

    // Cut the last line in half.
    let file = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .expect("a bucket file");
    let text = std::fs::read(&file).unwrap();
    let cut = text.len() - 40;
    std::fs::write(&file, &text[..cut]).unwrap();

    let args = plan(dir.path(), 1);
    let (records, summary) = replay::read_range(&args).expect("read");
    assert_eq!(records.len(), 2, "the two intact records survive");
    assert_eq!(summary.malformed_lines, 1, "and the cut one is counted");
}

#[tokio::test]
async fn a_summary_with_a_refusal_exits_nonzero() {
    // So `server replay ... && echo ok` means something in a test script.
    let dir = tempfile::tempdir().expect("tempdir");
    let down_a = FakeDownstream::start(Script::default()).await;
    let simmer_a = Simmer::start(&capture_config(&down_a, dir.path())).await;

    let mut c = simmer_a.connect().await;
    c.hello().await;
    c.deliver("a@oldbrand.com", "b@x.test", "Subject: hi\r\n\r\nx\r\n")
        .await;
    settle_for(dir.path(), 1).await;

    // Nothing is listening on the target port, so the replay fails in transport.
    let args = plan(dir.path(), 1);
    let (records, summary) = replay::read_range(&args).expect("read");
    let summary = replay::run(&args, &records, &BTreeMap::new(), summary).await;

    assert_eq!(summary.sent, 1);
    assert_eq!(summary.accepted, 0);
    assert_eq!(summary.transport_errors, 1);
    assert_eq!(summary.exit_code(), 1);
    assert_eq!(
        Summary::default().exit_code(),
        0,
        "sent nothing, failed nothing"
    );
}
