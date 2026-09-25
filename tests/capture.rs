//! D-085 — the debugging capture, through a real session.
//!
//! The claims worth pinning here are not "a file appeared". They are the four
//! properties that make this a debugging mode rather than a spool, and the two
//! that make a replay of it meaningful:
//!
//! - it is **off** unless configured, and off costs nothing;
//! - a record carries **no outcome and no derived state**, so nothing can grow
//!   into a queue's journal;
//! - `on_error: continue` **cannot stop mail**, and `on_error: defer` refuses
//!   **before** anything is relayed, so it cannot manufacture a duplicate;
//! - the recorded bytes are **exactly** what the client sent;
//! - a restart **appends** rather than truncating, and the sweeper never eats
//!   the file being written.

mod support;

use std::path::Path;

use simmer::capture::Record;
use support::{config_for, FakeDownstream, Script, Simmer};

/// `config_for` with a `capture:` block pointed at `dir`.
fn with_capture(down: &FakeDownstream, dir: &Path, extra: &str) -> String {
    config_for(
        down.addr,
        &format!(
            "capture:\n  directory: \"{}\"\n  max_queue_bytes: 1048576\n{extra}",
            dir.display()
        ),
    )
}

/// Every record in the directory, oldest first.
fn records(dir: &Path) -> Vec<Record> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    let mut out = Vec::new();
    for name in files {
        let text = std::fs::read(dir.join(&name)).expect("read");
        for line in text.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            out.push(Record::from_line(line).unwrap_or_else(|e| panic!("{name}: {e}")));
        }
    }
    out.sort_by_key(|r| r.at);
    out
}

fn bucket_files(dir: &Path) -> Vec<String> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".jsonl"))
        .collect();
    v.sort();
    v
}

/// Wait until `want` records are on disk.
///
/// `offer` is deliberately fire-and-forget and the writer buffers, so the file
/// lags the client's reply by design — bounded by the writer's one-second idle
/// tick, which is what makes `tail -f` useful during a debugging session.
///
/// Polling rather than sleeping past that tick: a fixed sleep passes on an idle
/// machine and fails on a busy one, which is the test that wastes an afternoon
/// in CI. This one is correct at any speed and usually returns in a second.
///
/// `on_error: defer` needs none of this — it acknowledges only once the line is
/// on disk, and those tests assert exactly that by not calling it.
async fn settle_for(dir: &Path, want: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        // Tolerant of a half-flushed line, which `records` is deliberately not.
        let seen = readable_records(dir);
        if seen >= want {
            // One more tick, so a test asserting `want` would still catch a
            // writer that produced `want + 1`.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "waited 30s for {want} captured record(s); only {seen} arrived"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// How many well-formed records are on disk right now.
fn readable_records(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .map(|text| {
            text.split(|&b| b == b'\n')
                .filter(|l| !l.is_empty())
                .filter(|l| Record::from_line(l).is_ok())
                .count()
        })
        .sum()
}

/// For the two tests that assert nothing was written. Absence cannot be polled
/// for, so this waits well past the writer's tick; too short would risk a false
/// pass, never a false failure.
async fn settle_for_nothing() {
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
}

// ---------------------------------------------------------------------------
// Off by default
// ---------------------------------------------------------------------------

#[tokio::test]
async fn capture_is_off_unless_configured_and_writes_nothing() {
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    // A directory exists, but no `capture:` block names it.
    let simmer = Simmer::start(&config_for(down.addr, "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(
        c.deliver("a@oldbrand.com", "b@x.test", "Subject: hi\r\n\r\nbody\r\n")
            .await
            .code,
        250
    );
    settle_for_nothing().await;

    assert!(
        bucket_files(dir.path()).is_empty(),
        "nothing should be written"
    );
}

// ---------------------------------------------------------------------------
// What a record is
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_message_produces_one_record_in_one_bucket() {
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    c.deliver(
        "a@oldbrand.com",
        "bob@x.test",
        "Subject: hi\r\n\r\nbody\r\n",
    )
    .await;
    settle_for(dir.path(), 1).await;

    let files = bucket_files(dir.path());
    assert_eq!(files.len(), 1, "{files:?}");

    let recs = records(dir.path());
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert_eq!(r.v, 2);
    // D-099: the listener as configured, an ingress fact — not the ramp.
    assert_eq!(r.listener.as_deref(), Some("127.0.0.1:0"));
    assert_eq!(r.mail_from.as_deref(), Some("a@oldbrand.com"));
    assert_eq!(r.rcpt_to, vec!["bob@x.test".to_string()]);
    assert!(!r.id.is_empty(), "the correlation id is the join key");
    assert!(r.peer.starts_with("127.0.0.1:"), "{}", r.peer);
    assert_eq!(r.helo, "client.test");
    assert!(!r.tls);

    // The file its timestamp belongs to — the invariant a replay's file
    // selection rests on.
    let start = simmer::capture::bucket::parse(&files[0]).expect("a bucket name");
    let (from, to) = simmer::capture::bucket::window(start);
    assert!(r.at >= from && r.at < to, "{} not in {files:?}", r.at);
}

#[tokio::test]
async fn a_line_opens_with_the_timestamp_recipient_sender_and_subject() {
    // The order the file is meant to be read in. Asserted on the bytes on disk,
    // not on a parsed object: JSON key order means nothing to a parser, and this
    // is entirely for the person running `cut -c1-160` over a bucket.
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    c.deliver(
        "news@oldbrand.com",
        "bob@x.test",
        "Subject: quarterly update\r\n\r\nbody\r\n",
    )
    .await;
    settle_for(dir.path(), 1).await;

    let file = dir.path().join(&bucket_files(dir.path())[0]);
    let line = std::fs::read_to_string(&file).expect("read");
    let line = line.lines().next().expect("a line");

    assert!(line.starts_with("{\"at\":"), "{line}");
    let offsets: Vec<usize> = ["\"at\":", "\"rcpt_to\":", "\"mail_from\":", "\"subject\":"]
        .iter()
        .map(|k| {
            line.find(k)
                .unwrap_or_else(|| panic!("{k} missing from {line}"))
        })
        .collect();
    assert!(
        offsets.windows(2).all(|w| w[0] < w[1]),
        "expected at < rcpt_to < mail_from < subject, got {offsets:?} in {line}"
    );

    // The four are readable in a terminal's width, before any machinery.
    let preview: String = line.chars().take(160).collect();
    assert!(preview.contains("bob@x.test"), "{preview}");
    assert!(preview.contains("news@oldbrand.com"), "{preview}");
    assert!(preview.contains("quarterly update"), "{preview}");
    assert!(
        !preview.contains("body_b64"),
        "the body must not intrude on the preview: {preview}"
    );
}

#[tokio::test]
async fn a_message_with_no_subject_records_an_empty_string_not_null() {
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    c.deliver(
        "news@oldbrand.com",
        "b@x.test",
        "From: news@oldbrand.com\r\n\r\nno subject header at all\r\n",
    )
    .await;
    settle_for(dir.path(), 1).await;

    let file = dir.path().join(&bucket_files(dir.path())[0]);
    let line = std::fs::read_to_string(&file).expect("read");
    assert!(line.contains(r#""subject":"""#), "{line}");
    assert!(!line.contains("\"subject\":null"), "{line}");
    assert_eq!(records(dir.path())[0].subject, "");
}

#[tokio::test]
async fn an_encoded_subject_is_recorded_decoded() {
    // The file is for reading; `=?utf-8?B?…?=` in the fourth column is not.
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    c.deliver(
        "news@oldbrand.com",
        "b@x.test",
        "Subject: =?utf-8?B?Q2Fmw6kgY3LDqG1l?=\r\n\r\nbody\r\n",
    )
    .await;
    settle_for(dir.path(), 1).await;

    let recs = records(dir.path());
    assert_eq!(recs[0].subject, "Caf\u{e9} cr\u{e8}me");
    // And the body is untouched by that convenience — it still replays verbatim.
    assert_eq!(
        recs[0].body().unwrap().unwrap(),
        b"Subject: =?utf-8?B?Q2Fmw6kgY3LDqG1l?=\r\n\r\nbody\r\n".to_vec()
    );
}

#[tokio::test]
async fn the_recorded_body_is_exactly_what_the_client_sent() {
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "")).await;

    // The shapes that break a naive round-trip: a line beginning with a dot
    // (dot-stuffing), a line that is only a dot's worth of text, a long header,
    // and a trailing blank line.
    let body = "Subject: awkward\r\nX-Long: \
                aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n\
                \r\n.leading dot\r\n..double dot\r\nplain\r\n\r\n";

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(
        c.deliver("a@oldbrand.com", "b@x.test", body).await.code,
        250
    );
    settle_for(dir.path(), 1).await;

    let recs = records(dir.path());
    assert_eq!(recs.len(), 1);
    let captured = recs[0].body().expect("decodes").expect("a body");

    // The capture is the §8.1 buffer's contents, which is exactly what the
    // downstream leg was handed — so the two must agree byte for byte.
    let delivered = down.last().expect("a delivery").body;
    assert_eq!(
        String::from_utf8_lossy(&captured),
        String::from_utf8_lossy(body.as_bytes()),
        "the capture must be what the client sent"
    );
    assert_eq!(recs[0].size, captured.len() as u64);
    assert!(
        !delivered.is_empty(),
        "sanity: the downstream received something"
    );
}

#[tokio::test]
async fn an_eight_bit_body_survives_byte_for_byte() {
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    let r = c.command("MAIL FROM:<a@oldbrand.com> BODY=8BITMIME").await;
    assert_eq!(r.code, 250, "{r:?}");
    assert_eq!(c.command("RCPT TO:<b@x.test>").await.code, 250);
    assert_eq!(c.command("DATA").await.code, 354);
    // Latin-1 bytes that are not valid UTF-8.
    let raw: &[u8] = b"Subject: caf\xe9\r\n\r\nCaf\xe9 cr\xe8me\r\n.\r\n";
    c.send_raw(raw).await;
    assert_eq!(c.read_reply().await.code, 250);
    settle_for(dir.path(), 1).await;

    let recs = records(dir.path());
    assert_eq!(recs.len(), 1);
    assert_eq!(
        recs[0].body().unwrap().unwrap(),
        b"Subject: caf\xe9\r\n\r\nCaf\xe9 cr\xe8me\r\n".to_vec()
    );
    assert!(recs[0].params.body_8bitmime, "the ESMTP parameter is kept");
}

#[tokio::test]
async fn a_record_carries_no_outcome_and_no_derived_state() {
    // The property that keeps §2.2 true, asserted on a real captured line rather
    // than a synthesised one.
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    c.deliver("a@oldbrand.com", "b@x.test", "Subject: hi\r\n\r\nx\r\n")
        .await;
    settle_for(dir.path(), 1).await;

    let files = bucket_files(dir.path());
    let text = std::fs::read_to_string(dir.path().join(&files[0])).expect("read");
    for forbidden in [
        "\"reply\"",
        "\"code\"",
        "\"route\"",
        "\"domain_group\"",
        "\"attempts\"",
        "\"state\"",
        "\"next_retry",
        "\"delivered\"",
    ] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }
    // And nothing resembling the credential the session handled.
    assert!(!text.contains("password"));
}

#[tokio::test]
async fn a_body_over_the_cap_is_omitted_but_the_message_is_still_identified() {
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "  max_body_bytes: 64\n")).await;

    let big = format!("Subject: big\r\n\r\n{}\r\n", "x".repeat(500));
    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(
        c.deliver("a@oldbrand.com", "b@x.test", &big).await.code,
        250,
        "an omitted body must not affect delivery"
    );
    settle_for(dir.path(), 1).await;

    let recs = records(dir.path());
    assert_eq!(recs.len(), 1);
    assert!(recs[0].body_omitted);
    assert_eq!(recs[0].body().unwrap(), None);
    // Still named: size and digest are of the real body.
    assert_eq!(recs[0].size, big.len() as u64);
    assert_eq!(recs[0].sha256.len(), 64);
}

// ---------------------------------------------------------------------------
// What is deliberately not captured
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_message_refused_before_the_final_dot_is_not_captured() {
    // Oversize: refused at the dot, never accepted, and there is no complete
    // message to record. Pins a known and documented gap.
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("MAIL FROM:<a@oldbrand.com>").await.code, 250);
    assert_eq!(c.command("RCPT TO:<b@x.test>").await.code, 250);
    assert_eq!(c.command("DATA").await.code, 354);
    // Over `config_for`'s max_message_bytes of 100000, so it is refused at the
    // dot and there is no complete message to record.
    c.send_raw(format!("Subject: x\r\n\r\n{}\r\n.\r\n", "y".repeat(120_000)).as_bytes())
        .await;
    assert_eq!(c.read_reply().await.code, 552);
    settle_for_nothing().await;

    assert!(records(dir.path()).is_empty());
}

// ---------------------------------------------------------------------------
// on_error
// ---------------------------------------------------------------------------

/// A directory that exists at validation time and is made unwritable afterwards,
/// so the failure happens where it matters — at the write, not at startup.
fn break_writes(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).expect("chmod");
}

fn unbreak(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[tokio::test]
async fn on_error_continue_cannot_stop_mail() {
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "")).await;
    break_writes(dir.path());

    let mut c = simmer.connect().await;
    c.hello().await;
    let reply = c
        .deliver("a@oldbrand.com", "b@x.test", "Subject: hi\r\n\r\nx\r\n")
        .await;
    unbreak(dir.path());

    assert_eq!(reply.code, 250, "a broken capture must not defer mail");
    assert_eq!(down.messages().len(), 1, "and it must still be relayed");
}

#[tokio::test]
async fn on_error_defer_refuses_before_anything_is_relayed() {
    // The load-bearing clause is the second assertion. A 451 raised *after* the
    // downstream accepted would make the client's retry a duplicate delivery —
    // §10.2's hazard, which is why the capture happens before the relay.
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "  on_error: defer\n")).await;
    break_writes(dir.path());

    let mut c = simmer.connect().await;
    c.hello().await;
    let reply = c
        .deliver("a@oldbrand.com", "b@x.test", "Subject: hi\r\n\r\nx\r\n")
        .await;
    unbreak(dir.path());

    assert_eq!(reply.code, 451, "{reply:?}");
    assert!(
        reply.text().contains("capture"),
        "the operator must be able to tell this from a quota 451: {reply:?}"
    );
    assert_eq!(
        down.messages().len(),
        0,
        "nothing may be relayed when the capture deferred the message"
    );
}

#[tokio::test]
async fn on_error_defer_relays_normally_when_the_capture_works() {
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "  on_error: defer\n")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(
        c.deliver("a@oldbrand.com", "b@x.test", "Subject: hi\r\n\r\nx\r\n")
            .await
            .code,
        250
    );

    // No settle(): `defer` acknowledges only once the line is on disk, so the
    // record must already be there when the client has its 250.
    assert_eq!(records(dir.path()).len(), 1);
    assert_eq!(down.messages().len(), 1);
}

// ---------------------------------------------------------------------------
// The file on disk
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_directory_is_0700_and_the_files_are_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    c.deliver("a@oldbrand.com", "b@x.test", "Subject: hi\r\n\r\nx\r\n")
        .await;
    settle_for(dir.path(), 1).await;

    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(dir.path()), 0o700, "the directory");
    let file = dir.path().join(&bucket_files(dir.path())[0]);
    assert_eq!(mode(&file), 0o600, "the bucket file");
}

#[tokio::test]
async fn a_restart_appends_to_the_open_bucket_rather_than_truncating_it() {
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let yaml = with_capture(&down, dir.path(), "");

    {
        let simmer = Simmer::start(&yaml).await;
        let mut c = simmer.connect().await;
        c.hello().await;
        c.deliver("a@oldbrand.com", "first@x.test", "Subject: 1\r\n\r\nx\r\n")
            .await;
        settle_for(dir.path(), 1).await;
    } // the first Simmer is dropped, and with it its Capture handle

    {
        let simmer = Simmer::start(&yaml).await;
        let mut c = simmer.connect().await;
        c.hello().await;
        c.deliver("a@oldbrand.com", "second@x.test", "Subject: 2\r\n\r\nx\r\n")
            .await;
        settle_for(dir.path(), 2).await;
    }

    // One bucket, two records: the filename is a pure function of the bucket and
    // the file is opened O_APPEND, so a restart inside ten minutes continues it.
    assert_eq!(bucket_files(dir.path()).len(), 1);
    let recs = records(dir.path());
    assert_eq!(recs.len(), 2, "the first record must survive the restart");
    assert_eq!(recs[0].rcpt_to, vec!["first@x.test".to_string()]);
    assert_eq!(recs[1].rcpt_to, vec!["second@x.test".to_string()]);
}

#[tokio::test]
async fn every_accepted_message_under_load_is_captured() {
    // The capture must not lose records just because sessions are concurrent —
    // and, since `offer` is fire-and-forget, this is the test that would catch a
    // queue bound set carelessly.
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "")).await;

    let mut tasks = Vec::new();
    for i in 0..16 {
        let addr = simmer.addr;
        tasks.push(tokio::spawn(async move {
            let mut c = support::Client::connect(addr).await;
            c.hello().await;
            c.deliver(
                "a@oldbrand.com",
                &format!("r{i}@x.test"),
                "Subject: load\r\n\r\nx\r\n",
            )
            .await
            .code
        }));
    }
    let mut accepted = 0;
    for t in tasks {
        if t.await.expect("join") == 250 {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 16);
    settle_for(dir.path(), 16).await;

    let recs = records(dir.path());
    assert_eq!(recs.len(), 16, "one record per accepted message");
    let mut seen: Vec<String> = recs.iter().flat_map(|r| r.rcpt_to.clone()).collect();
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 16, "every recipient appears exactly once");
}

#[tokio::test]
async fn the_sweeper_deletes_an_old_bucket_and_leaves_the_open_one() {
    let down = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let simmer = Simmer::start(&with_capture(&down, dir.path(), "")).await;

    let mut c = simmer.connect().await;
    c.hello().await;
    c.deliver("a@oldbrand.com", "b@x.test", "Subject: hi\r\n\r\nx\r\n")
        .await;
    settle_for(dir.path(), 1).await;

    // A bucket from three hours ago, as a restart after an outage would leave.
    let old = simmer::capture::bucket::name(chrono::Utc::now() - chrono::Duration::hours(3));
    std::fs::write(dir.path().join(&old), b"{\"v\":1}\n").expect("plant");
    assert_eq!(bucket_files(dir.path()).len(), 2);

    simmer::capture::sweeper::sweep_once(dir.path(), std::time::Duration::from_secs(3600)).await;

    let left = bucket_files(dir.path());
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(!left.contains(&old), "the old bucket should be gone");
}
