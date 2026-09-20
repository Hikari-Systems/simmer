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

/// How often the writer wakes with nothing to do: to flush a partial buffer so
/// `tail -f` is useful during the debugging session this exists for, to publish
/// the queue gauges, and to close a bucket whose window has passed.
const IDLE_TICK: Duration = Duration::from_secs(1);

/// The write buffer. `src/smtp/buffer.rs`'s spill buffer is 64 KiB for the same
/// reason (D-080): one `write` syscall per line put a blocking-pool job on the
/// latency path of every message.
const WRITE_BUFFER: usize = 256 * 1024;

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
                        // a buffer, before the client is told anything.
                        if let Some(ack) = job.ack {
                            let result = sync_open(&mut open).await;
                            if result.is_err() {
                                dropped += 1;
                            }
                            let _ = ack.send(result);
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
            }
        }
    }

    // The channel is closed, so nothing more can arrive. Everything buffered is
    // a message some client was told about.
    if let Some(o) = open.as_mut() {
        if let Err(e) = o.file.flush().await {
            tracing::error!(path = %o.path.display(), error = %e, "final capture flush failed");
            metrics::capture_dropped("shutdown");
        }
    }
    close(&mut open).await;
    tracing::info!(written, dropped, bytes, "capture writer stopped");
}

/// A write that did not happen, and enough to say why in one log line.
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
        records: 0,
        bytes: 0,
    })
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
    o.dirty = false;
    Ok(())
}

/// One idle pass: flush a partial buffer, and close a bucket whose window has
/// passed so its fd is released and a reader sees a finished file.
async fn idle(open: &mut Option<Open>) -> Result<(), String> {
    let Some(o) = open.as_mut() else {
        return Ok(());
    };
    if o.dirty {
        o.file.flush().await.map_err(|e| e.to_string())?;
        o.dirty = false;
    }
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
    if let Some(mut o) = open.take() {
        if let Err(e) = o.file.flush().await {
            tracing::error!(path = %o.path.display(), error = %e, "closing a capture file");
        }
    }
}
