//! The single writer task, and the bounded queue in front of it.
//!
//! ## Why a task and not a lock
//!
//! The obvious shape — `Mutex<BufWriter<File>>` shared by every session — puts
//! the write, the flush, the bucket rollover and the directory open *inline on
//! the message path, under a lock held across `await`s*. Every concurrent
//! session then serialises against one disk, and a disk that stalls stalls all
//! of them at once. That is the failure shape F16 found in another guise, and it
//! would make a debugging aid capable of taking the relay down.
//!
//! A task owns the file instead. Sessions hand it a serialised line and move on.
//!
//! ## Why the session serialises, and the writer does not
//!
//! [`Job::line`] arrives ready to write. base64 and SHA-256 over a megabyte of
//! body is real CPU, and doing it in the writer would make the writer the
//! bottleneck for every session at once. Doing it on the session's own task
//! spreads it across the runtime, and it is work that session was going to pay
//! for anyway.
//!
//! ## Why the queue is never awaited
//!
//! [`super::Capture::offer`] uses `try_send` and **never** `send().await`.
//! Awaiting a full capture queue is precisely how a slow disk becomes
//! backpressure on the relay — the thing this module exists not to do. A full
//! queue drops the record, counts it and logs it. Under `on_error: continue`
//! that is a gap in a debugging artefact and nothing else.
//!
//! `on_error: defer` is the one case that waits, and it waits *before* the
//! relay, so a failure defers a message nothing has accepted yet.
//!
//! ## The invariant the replay reader rests on
//!
//! **A record's `at` is always inside the window of the bucket its file names.**
//!
//! Rollover is therefore driven by the record's own timestamp, never by the
//! writer's clock. Two consequences, both handled below rather than designed
//! away: a record arriving out of order re-opens its own bucket
//! ([`metrics::capture_late_write`]), and a wall clock that steps backwards does
//! the same at scale ([`metrics::capture_clock_regression`]). The cost is that
//! lines within one file are not guaranteed to be in timestamp order, which is
//! why the reader sorts. The alternative — clamping `at` forward to keep a file
//! ordered — would put a lie in a file whose entire value is that its timestamps
//! are true.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

use super::bucket;
use crate::metrics;

/// How often the writer wakes with nothing to do: to publish the queue gauges,
/// to close a bucket whose window has passed, and as the backstop flush behind
/// the policy below.
const IDLE_TICK: Duration = Duration::from_secs(1);

/// Flush once this many lines are buffered.
///
/// The buffer below is 256 KiB, which at the 4 KiB end of a real message mix is
/// forty-odd records — so without this a quiet-ish stream can sit unwritten for
/// as long as it takes to fill, and `tail -f` on the file this mode exists to
/// produce shows nothing. Ten bounds it by records rather than by bytes.
const FLUSH_LINES: u64 = 10;

/// …or once the stream has been quiet this long with anything buffered.
///
/// The two rules are the two shapes traffic comes in. Under load the count is
/// what fires, so the flush cost is amortised over ten records; when the stream
/// trickles, the count may never be reached and this is what puts the record on
/// disk. Reset by each record, so it means *idle* and not "at most this stale" —
/// [`IDLE_TICK`] remains the backstop that bounds the trickling case, exactly as
/// it did before.
const IDLE_FLUSH: Duration = Duration::from_millis(500);

/// The write buffer. `src/smtp/buffer.rs`'s spill buffer is 64 KiB for the same
/// reason (D-080): one `write` syscall per line put a blocking-pool job on the
/// latency path of every message.
const WRITE_BUFFER: usize = 256 * 1024;

/// When the buffered lines should reach the file.
///
/// A pure decision, separated from the loop that acts on it so the policy can be
/// tested without a clock: a timing test of "did it flush within 500 ms" on a
/// loaded machine is a flake, and the thing worth pinning is the rule, not the
/// scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flush {
    /// [`FLUSH_LINES`] are buffered: write them through without waiting.
    Now,
    /// Something is buffered: write it through after this much quiet.
    WhenIdle(Duration),
    /// Nothing is buffered.
    Nothing,
}

pub(super) fn flush_decision(dirty: bool, since_flush: u64) -> Flush {
    match (dirty, since_flush) {
        (false, _) => Flush::Nothing,
        (true, n) if n >= FLUSH_LINES => Flush::Now,
        (true, _) => Flush::WhenIdle(IDLE_FLUSH),
    }
}

/// The idle-flush arm of the loop's `select!`.
///
/// `None` is "nothing is buffered", and must park forever rather than fire: a
/// deadline that resolves immediately would spin the loop.
async fn when(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// One record on its way to the file.
pub(super) struct Job {
    /// The serialised line, newline included. Serialised by the session.
    pub line: Vec<u8>,
    /// The record's own timestamp. This, and not the writer's clock, chooses the
    /// bucket.
    pub at: DateTime<Utc>,
    /// Present only under `on_error: defer`: the writer answers once the line is
    /// on disk, or says why it is not.
    pub ack: Option<oneshot::Sender<Result<(), String>>>,
}

/// The bucket file currently open.
struct Open {
    start: DateTime<Utc>,
    path: PathBuf,
    file: tokio::io::BufWriter<tokio::fs::File>,
    /// Whether anything has been written since the last flush.
    dirty: bool,
    /// Lines written since the last flush, for [`FLUSH_LINES`].
    since_flush: u64,
    /// Bytes written into the buffer and not yet pushed to the file. What a flush
    /// adds to `simmer_capture_disk_bytes` (F17).
    unflushed_bytes: u64,
    records: u64,
    bytes: u64,
}

/// Run until the channel closes.
///
/// It listens to **no shutdown token**. The channel closes when the last
/// [`super::Capture`] handle is dropped, and "no handle exists" is exactly "no
/// session can offer another record" — a condition the type system already
/// tracks, and a sharper one than any token. `main` drops its handles after the
/// session grace period and then waits here, so a message accepted in the last
/// second of shutdown is still written.
pub(super) async fn run(dir: PathBuf, mut rx: mpsc::Receiver<Job>, queued: super::Queued) {
    let mut open: Option<Open> = None;
    let mut newest: Option<DateTime<Utc>> = None;
    let mut written: u64 = 0;
    let mut dropped: u64 = 0;
    let mut bytes: u64 = 0;

    let mut ticker = tokio::time::interval(IDLE_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // When the idle rule would next put a partial buffer on disk. `None` while
    // nothing is buffered.
    let mut flush_at: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            job = rx.recv() => {
                let Some(job) = job else { break };
                queued.release(job.line.len() as u64);
                let size = job.line.len() as u64;
                match write_one(&dir, &mut open, &mut newest, &job).await {
                    Ok(()) => {
                        written += 1;
                        bytes += size;
                        metrics::capture_written(size);
                        // Under `defer` the line must be on the platter, not in
                        // a buffer, before the client is told anything. That is
                        // a flush and an fsync, so it satisfies the policy below
                        // outright.
                        if let Some(ack) = job.ack {
                            let result = sync_open(&mut open).await;
                            if result.is_err() {
                                dropped += 1;
                            }
                            let _ = ack.send(result);
                            flush_at = None;
                        } else {
                            let (dirty, since) = open
                                .as_ref()
                                .map_or((false, 0), |o| (o.dirty, o.since_flush));
                            match flush_decision(dirty, since) {
                                Flush::Now => {
                                    if let Err(e) = flush_and_account(&mut open).await {
                                        metrics::capture_dropped("write_error");
                                        tracing::error!(error = %e, "flushing the capture buffer failed");
                                    }
                                    flush_at = None;
                                }
                                Flush::WhenIdle(after) => {
                                    // Reset on every record, so the deadline is
                                    // measured from the last one.
                                    flush_at = Some(tokio::time::Instant::now() + after);
                                }
                                Flush::Nothing => flush_at = None,
                            }
                        }
                    }
                    Err(e) => {
                        dropped += 1;
                        metrics::capture_dropped(e.reason);
                        tracing::error!(
                            path = %e.path.display(),
                            reason = e.reason,
                            error = %e.error,
                            "capture write failed"
                        );
                        if let Some(ack) = job.ack {
                            let _ = ack.send(Err(e.error));
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                queued.publish();
                if let Err(e) = idle(&mut open).await {
                    metrics::capture_dropped("write_error");
                    tracing::error!(error = %e, "flushing the capture buffer failed");
                }
                flush_at = None;
            }
            _ = when(flush_at) => {
                if let Err(e) = flush_and_account(&mut open).await {
                    metrics::capture_dropped("write_error");
                    tracing::error!(error = %e, "flushing the capture buffer failed");
                }
                flush_at = None;
            }
        }
    }

    // The channel is closed, so nothing more can arrive. Everything buffered is
    // a message some client was told about.
    if let Err(e) = flush_and_account(&mut open).await {
        tracing::error!(error = %e, "final capture flush failed");
        metrics::capture_dropped("shutdown");
    }
    close(&mut open).await;
    tracing::info!(written, dropped, bytes, "capture writer stopped");
}

/// A write that did not happen, and enough to say why in one log line.
#[derive(Debug)]
struct WriteError {
    reason: &'static str,
    path: PathBuf,
    error: String,
}

/// Put one line in the file its timestamp belongs to, opening or rolling over
/// first if it is not the file already open.
async fn write_one(
    dir: &Path,
    open: &mut Option<Open>,
    newest: &mut Option<DateTime<Utc>>,
    job: &Job,
) -> Result<(), WriteError> {
    let start = bucket::floor(job.at);

    // A record more than one bucket behind the newest seen means the wall clock
    // went backwards — NTP stepping, or a container resumed from a snapshot. It
    // is written to its own bucket regardless, which is what keeps the invariant
    // absolute; the operator gets one line saying why a replay of that range may
    // need a wider window.
    match *newest {
        Some(n) if job.at < n - chrono::Duration::seconds(bucket::BUCKET_SECS) => {
            metrics::capture_clock_regression();
            tracing::warn!(
                record_at = %job.at.to_rfc3339(),
                newest_seen = %n.to_rfc3339(),
                bucket = %bucket::name(job.at),
                "a captured record is more than one bucket behind the newest seen; this \
                 instance's clock stepped backwards. The record is filed under its own \
                 timestamp; a replay covering it may need a wider --pad-buckets"
            );
        }
        Some(n) if job.at > n => *newest = Some(job.at),
        None => *newest = Some(job.at),
        Some(_) => {}
    }

    let rolled = match open.as_ref() {
        Some(o) if o.start == start => false,
        Some(_) => {
            roll(open).await;
            true
        }
        None => true,
    };

    if rolled {
        // A bucket whose window has already passed is one we are re-opening for
        // a record that arrived late. Worth counting: it is the visible symptom
        // of out-of-order arrival, and it is benign.
        let reopened = open_bucket(dir, start).await?;
        if start + chrono::Duration::seconds(bucket::BUCKET_SECS) <= Utc::now() {
            metrics::capture_late_write();
        }
        *open = Some(reopened);
    }

    let o = open.as_mut().expect("just opened");
    o.file.write_all(&job.line).await.map_err(|e| WriteError {
        reason: "write_error",
        path: o.path.clone(),
        error: e.to_string(),
    })?;
    o.dirty = true;
    o.since_flush += 1;
    o.unflushed_bytes += job.line.len() as u64;
    o.records += 1;
    o.bytes += job.line.len() as u64;
    Ok(())
}

/// Open — or re-open — the file for `start`, appending.
///
/// `append` is what makes a restart inside the same ten minutes continue the
/// file rather than truncate it, and it is why the name carries no pid or
/// sequence number: the name is a pure function of the bucket, so the same
/// bucket always resolves to the same file.
async fn open_bucket(dir: &Path, start: DateTime<Utc>) -> Result<Open, WriteError> {
    let path = dir.join(bucket::name(start));
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        // umask can only clear bits, so this is a ceiling rather than a
        // guarantee. The directory is 0700, which is the real containment.
        .mode(0o600)
        .open(&path)
        .await
        .map_err(|e| WriteError {
            reason: "open_error",
            path: path.clone(),
            error: e.to_string(),
        })?;

    Ok(Open {
        start,
        path,
        file: tokio::io::BufWriter::with_capacity(WRITE_BUFFER, file),
        dirty: false,
        since_flush: 0,
        unflushed_bytes: 0,
        records: 0,
        bytes: 0,
    })
}

/// Write the buffered lines through to the file, and reset the policy's counters.
///
/// A flush and not an fsync: the point is that the bytes leave this process, so a
/// `tail -f`, a `replay`, or anything else reading the directory sees them.
/// Durability against a machine that loses power is `on_error: defer`'s business
/// (`sync_open`), and this is a debugging mode, not a spool.
async fn flush_open(open: &mut Option<Open>) -> Result<u64, String> {
    let Some(o) = open.as_mut() else {
        return Ok(0);
    };
    if !o.dirty {
        // Nothing buffered. Returning 0 rather than the last count is what stops
        // the gauge double-counting a flush that had nothing to do — the idle
        // deadline and the backstop tick can both land on the same clean buffer.
        return Ok(0);
    }
    o.file.flush().await.map_err(|e| e.to_string())?;
    let pushed = o.unflushed_bytes;
    o.dirty = false;
    o.since_flush = 0;
    o.unflushed_bytes = 0;
    Ok(pushed)
}

/// [`flush_open`], reporting what reached the file to the gauge (F17).
async fn flush_and_account(open: &mut Option<Open>) -> Result<(), String> {
    let pushed = flush_open(open).await?;
    if pushed > 0 {
        metrics::capture_disk_grew(pushed);
    }
    Ok(())
}

/// Flush and fsync whatever is open, for a `defer` acknowledgement.
async fn sync_open(open: &mut Option<Open>) -> Result<(), String> {
    let Some(o) = open.as_mut() else {
        return Err("no capture file is open".to_string());
    };
    o.file.flush().await.map_err(|e| e.to_string())?;
    // `sync_data`, not `sync_all`: the file's length and contents are what
    // matter, and the directory entry already exists by the time anything is
    // acknowledged.
    o.file
        .get_ref()
        .sync_data()
        .await
        .map_err(|e| e.to_string())?;
    let pushed = o.unflushed_bytes;
    o.dirty = false;
    o.since_flush = 0;
    o.unflushed_bytes = 0;
    if pushed > 0 {
        metrics::capture_disk_grew(pushed);
    }
    Ok(())
}

/// One idle pass: the backstop flush, and close a bucket whose window has passed
/// so its fd is released and a reader sees a finished file.
async fn idle(open: &mut Option<Open>) -> Result<(), String> {
    flush_and_account(open).await?;
    let Some(o) = open.as_ref() else {
        return Ok(());
    };
    if o.start + chrono::Duration::seconds(bucket::BUCKET_SECS) <= Utc::now() {
        roll(open).await;
    }
    Ok(())
}

/// Finish the open bucket and say so. One line per ten minutes, which is how an
/// operator sees the capture is alive without turning on DEBUG.
async fn roll(open: &mut Option<Open>) {
    if let Some(o) = open.as_ref() {
        let (from, to) = bucket::window(o.start);
        tracing::info!(
            file = %o.path.display(),
            from = %from.to_rfc3339(),
            to = %to.to_rfc3339(),
            records = o.records,
            bytes = o.bytes,
            "capture bucket closed"
        );
    }
    close(open).await;
}

async fn close(open: &mut Option<Open>) {
    // Through the accounting, so a rollover's last partial buffer reaches the
    // gauge like any other flush.
    let path = open.as_ref().map(|o| o.path.clone());
    if let Err(e) = flush_and_account(open).await {
        match path {
            Some(p) => tracing::error!(path = %p.display(), error = %e, "closing a capture file"),
            None => tracing::error!(error = %e, "closing a capture file"),
        }
    }
    open.take();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_buffer_is_not_flushed() {
        // Nothing buffered must park the idle arm forever rather than resolve:
        // a deadline that fires immediately would spin the writer's loop.
        assert_eq!(flush_decision(false, 0), Flush::Nothing);
        // Counter without the dirty flag is not a reason to write: the two are
        // reset together, so this pairing should not arise, and if it ever does
        // the flag is the one that says whether there are bytes.
        assert_eq!(flush_decision(false, FLUSH_LINES + 5), Flush::Nothing);
    }

    #[test]
    fn ten_buffered_lines_flush_without_waiting() {
        assert_eq!(flush_decision(true, FLUSH_LINES), Flush::Now);
        assert_eq!(flush_decision(true, FLUSH_LINES + 1), Flush::Now);
    }

    #[test]
    fn fewer_than_ten_wait_for_the_stream_to_go_quiet() {
        // The trickle case: the count will not be reached, so the record reaches
        // the file half a second after the last one and not when the 256 KiB
        // buffer eventually fills.
        for n in 1..FLUSH_LINES {
            assert_eq!(
                flush_decision(true, n),
                Flush::WhenIdle(IDLE_FLUSH),
                "{n} buffered"
            );
        }
        assert_eq!(IDLE_FLUSH, Duration::from_millis(500));
    }

    #[tokio::test]
    async fn a_flush_reports_the_bytes_it_pushed_exactly_once() {
        // F17's fix. The gauge is incremented by what each flush pushes, so the
        // one way to get it wrong is to count the same bytes twice: the idle
        // deadline and the backstop tick can both land on the same buffer, and a
        // rollover flushes again on the way out.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut open = None;
        let mut newest = None;
        let job = |line: &str| Job {
            line: line.as_bytes().to_vec(),
            at: Utc::now(),
            ack: None,
        };

        write_one(dir.path(), &mut open, &mut newest, &job("{\"a\":1}\n"))
            .await
            .expect("write");
        assert_eq!(open.as_ref().expect("open").unflushed_bytes, 8);

        assert_eq!(flush_open(&mut open).await.expect("flush"), 8);
        assert_eq!(open.as_ref().expect("open").unflushed_bytes, 0);

        // The second flush has nothing to push and must say so, or every idle
        // tick would add the last flush's bytes again.
        assert_eq!(flush_open(&mut open).await.expect("flush"), 0);
        assert_eq!(flush_open(&mut open).await.expect("flush"), 0);

        // And it resumes counting from zero, not from the running total.
        write_one(dir.path(), &mut open, &mut newest, &job("{\"bb\":2}\n"))
            .await
            .expect("write");
        assert_eq!(flush_open(&mut open).await.expect("flush"), 9);
    }

    #[tokio::test]
    async fn the_bytes_flushed_are_the_bytes_on_disk() {
        // The gauge's claim is "bytes on disk", so what a flush reports has to be
        // what the file actually grew by — otherwise the increments and the
        // sweeper's recount would disagree by construction and every sweep would
        // show a jump.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut open = None;
        let mut newest = None;
        let mut reported = 0u64;
        for n in 0..25 {
            let line = format!("{{\"n\":{n}}}\n");
            write_one(
                dir.path(),
                &mut open,
                &mut newest,
                &Job {
                    line: line.into_bytes(),
                    at: Utc::now(),
                    ack: None,
                },
            )
            .await
            .expect("write");
            reported += flush_open(&mut open).await.expect("flush");
        }
        let path = open.as_ref().expect("open").path.clone();
        let on_disk = std::fs::metadata(&path).expect("stat").len();
        assert_eq!(reported, on_disk, "reported {reported}, file is {on_disk}");
    }

    #[test]
    fn the_idle_deadline_is_bounded_by_the_backstop_tick() {
        // IDLE_FLUSH is what normally fires; IDLE_TICK is what bounds the case
        // IDLE_FLUSH cannot, a stream arriving just often enough to keep
        // resetting the deadline without ever reaching FLUSH_LINES. It is only a
        // backstop if it is the longer of the two.
        assert!(
            IDLE_FLUSH < IDLE_TICK,
            "the idle flush must fire before the backstop tick"
        );
    }
}
