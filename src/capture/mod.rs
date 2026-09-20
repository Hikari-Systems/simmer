//! D-085 — the optional message capture sink.
//!
//! **This is a debugging mode, and it is not a spool.** §2.2 says Simmer does
//! not own messages, §8.1 says the `DATA` buffer is transient and not durable,
//! and neither changes here. What changes is that an operator may, deliberately
//! and temporarily, ask for a copy of what arrived so that it can be looked at
//! afterwards and replayed into a test instance (`capture::replay`).
//!
//! Four properties keep that framing true rather than merely asserted:
//!
//! 1. **Off unless configured.** Absent `capture:` means no directory, no task
//!    and no cost, exactly as `link_proxy` is absent.
//! 2. **Never in the delivery path.** Nothing ever reads a capture file back to
//!    decide what to deliver. The only reader is an offline subcommand run by
//!    hand. A write failure defaults to not changing the client's reply at all
//!    (`on_error: continue`).
//! 3. **It recovers nothing.** No crash recovery, no retry, no queue semantics,
//!    no `fsync`. A record is a diagnostic artefact, not a promise, and a
//!    message missing from the log was still delivered or still refused
//!    according to what the client was told.
//! 4. **It is bounded.** A retention sweeper deletes whole buckets by age, and
//!    startup warns loudly about what is being written.
//!
//! ## What it costs, said plainly
//!
//! §7.3 hashes recipient addresses precisely so that "the container does not
//! accumulate a plaintext record of every address mailed". A capture directory
//! is that record, plus the bodies. That is the trade an operator makes by
//! enabling this, and it is why the directory is `0700`, the files `0600`, the
//! retention short by default, and why none of it is reachable through the
//! §9 control plane. See `DECISIONS.md` D-085.

pub mod bucket;
pub mod client;
pub mod record;
pub mod replay;
pub mod sweeper;
mod writer;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

pub use record::{subject_from_headers, Ingress, Params, ParseError, Record};

use crate::config::{self, CaptureOnError};
use crate::metrics;

/// How long a session under `on_error: defer` waits for the writer to confirm a
/// line is on disk before giving up and answering `451`.
///
/// Not in the §4.1 schema, like `SHUTDOWN_GRACE`. It is a backstop against a
/// writer wedged on a hung filesystem, not a tuning knob: a disk that needs more
/// than five seconds to accept one line has already failed.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// At most one drop is logged in full per this interval, with a count of what it
/// stood for. A full queue would otherwise emit thousands of ERROR lines a
/// minute into the log the operator is reading.
const DROP_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// A handle on the capture sink.
///
/// Cheap to clone — an `Arc` and nothing else — so `Option<Capture>` on `Engine`
/// is one word when capture is off, which is the normal case.
///
/// **There is deliberately no read method.** No `read`, no `list`, no `iter`, no
/// `find`. The delivery path cannot consult the capture because there is no API
/// through which to do it, and that absence is the first of the four properties
/// in the module documentation above. The only reader in the crate is
/// [`replay`](crate::capture::replay), which is an SMTP client reachable only
/// from a subcommand.
#[derive(Clone)]
pub struct Capture(Arc<Inner>);

struct Inner {
    tx: mpsc::Sender<writer::Job>,
    queued: Queued,
    on_error: CaptureOnError,
    max_body_bytes: u64,
    drops: Mutex<DropLog>,
}

/// What became of an offered record under `on_error: continue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offered {
    /// Queued for the writer.
    Accepted,
    /// Not queued. A gap in the capture; the message is unaffected.
    Dropped(DropReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// `capture.queue_depth` records are already waiting.
    QueueFull,
    /// `capture.max_queue_bytes` would be exceeded.
    QueueBytes,
    /// The writer task is gone. Only reachable during shutdown.
    WriterGone,
}

impl DropReason {
    /// The `reason` label on `simmer_capture_dropped_total`. A closed set of
    /// `&'static str`, never anything the client influences (finding F7).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QueueFull => "queue_full",
            Self::QueueBytes => "queue_bytes",
            Self::WriterGone => "writer_gone",
        }
    }
}

/// The two bounds on the queue, held together because they are checked and
/// released together.
///
/// `queue_depth` alone does not bound memory: a 25 MiB message serialises to a
/// ~34 MiB line, and a thousand queued is 34 GiB. The byte budget is what makes
/// the queue's cost predictable.
#[derive(Clone)]
pub(crate) struct Queued {
    bytes: Arc<AtomicU64>,
    depth: Arc<AtomicU64>,
    max_bytes: u64,
}

impl Queued {
    fn new(max_bytes: u64) -> Queued {
        Queued {
            bytes: Arc::new(AtomicU64::new(0)),
            depth: Arc::new(AtomicU64::new(0)),
            max_bytes,
        }
    }

    /// Reserve room for `n` bytes, or refuse.
    ///
    /// A compare-and-swap loop rather than `fetch_add` then compare: the latter
    /// can transiently exceed the budget under concurrency, and on a 34 MiB line
    /// "transiently" is the whole problem.
    fn reserve(&self, n: u64) -> bool {
        let mut current = self.bytes.load(Ordering::Relaxed);
        loop {
            if current.saturating_add(n) > self.max_bytes {
                return false;
            }
            match self.bytes.compare_exchange_weak(
                current,
                current + n,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.depth.fetch_add(1, Ordering::Relaxed);
                    return true;
                }
                Err(actual) => current = actual,
            }
        }
    }

    pub(crate) fn release(&self, n: u64) {
        self.bytes.fetch_sub(n, Ordering::AcqRel);
        self.depth.fetch_sub(1, Ordering::Relaxed);
    }

    pub(crate) fn publish(&self) {
        metrics::capture_queue(
            self.depth.load(Ordering::Relaxed) as usize,
            self.bytes.load(Ordering::Relaxed),
        );
    }
}

/// Rate-limiting state for the drop log.
struct DropLog {
    last: Option<Instant>,
    suppressed: u64,
}

impl Capture {
    /// Create the directory, open nothing, and start the writer.
    ///
    /// Returns the handle and the writer's join handle. A failure here is a
    /// **startup** failure — `main` propagates it and the process refuses to
    /// start, the `link_proxy::Listener::bind` precedent. A capture that is
    /// configured but silently writing nothing is the worst outcome available:
    /// the operator enabled it for a reason and would find out only when they
    /// went looking for the records.
    pub fn start(cfg: &config::Capture) -> anyhow::Result<(Capture, tokio::task::JoinHandle<()>)> {
        let dir = PathBuf::from(&cfg.directory);
        create_dir(&dir)?;

        let (tx, rx) = mpsc::channel(cfg.queue_depth);
        let queued = Queued::new(cfg.max_queue_bytes);
        let task = tokio::spawn(writer::run(dir.clone(), rx, queued.clone()));

        tracing::info!(
            dir = %dir.display(),
            bucket_seconds = bucket::BUCKET_SECS,
            retention_secs = cfg.retention.as_secs(),
            on_error = cfg.on_error.as_str(),
            max_body_bytes = cfg.max_body_bytes,
            queue_depth = cfg.queue_depth,
            max_queue_bytes = cfg.max_queue_bytes,
            "message capture started"
        );

        Ok((
            Capture(Arc::new(Inner {
                tx,
                queued,
                on_error: cfg.on_error,
                max_body_bytes: cfg.max_body_bytes,
                drops: Mutex::new(DropLog {
                    last: None,
                    suppressed: 0,
                }),
            })),
            task,
        ))
    }

    pub fn on_error(&self) -> CaptureOnError {
        self.0.on_error
    }

    pub fn max_body_bytes(&self) -> u64 {
        self.0.max_body_bytes
    }

    /// Queue a record without waiting for it. Never fails a message.
    ///
    /// `try_send`, never `send().await`: awaiting a full capture queue is how a
    /// slow disk becomes backpressure on the relay, which is the one thing this
    /// module must not do.
    pub fn offer(&self, record: Record) -> Offered {
        let (job, len) = match self.job(record, None) {
            Ok(v) => v,
            Err(reason) => return self.dropped(reason),
        };
        match self.0.tx.try_send(job) {
            Ok(()) => Offered::Accepted,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.0.queued.release(len);
                self.dropped(DropReason::QueueFull)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.0.queued.release(len);
                self.dropped(DropReason::WriterGone)
            }
        }
    }

    /// Queue a record and wait until it is on disk.
    ///
    /// Only for `on_error: defer`, and only ever called **before** the relay: a
    /// failure here defers a message nothing has accepted yet. Called after the
    /// downstream conversation it would manufacture the duplicate delivery that
    /// §10.2 and D-068 exist to prevent.
    pub async fn offer_durable(&self, record: Record) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        let (job, len) = self
            .job(record, Some(tx))
            .map_err(|r| format!("capture queue is full ({})", r.as_str()))?;

        if let Err(e) = self.0.tx.try_send(job) {
            self.0.queued.release(len);
            return Err(match e {
                mpsc::error::TrySendError::Full(_) => "capture queue is full".to_string(),
                mpsc::error::TrySendError::Closed(_) => "capture writer is gone".to_string(),
            });
        }

        match tokio::time::timeout(ACK_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("capture writer dropped the acknowledgement".to_string()),
            Err(_) => Err(format!(
                "capture writer did not acknowledge within {}s",
                ACK_TIMEOUT.as_secs()
            )),
        }
    }

    /// Serialise, and reserve the queue budget for the result.
    fn job(
        &self,
        record: Record,
        ack: Option<oneshot::Sender<Result<(), String>>>,
    ) -> Result<(writer::Job, u64), DropReason> {
        if record.body_omitted {
            metrics::capture_body_omitted();
        }
        let at = record.at;
        let line = match record.to_line() {
            Ok(l) => l,
            Err(e) => {
                // Unreachable: every field is a scalar, a String or a Vec<String>.
                tracing::error!(error = %e, "serialising a capture record");
                return Err(DropReason::QueueBytes);
            }
        };
        let len = line.len() as u64;
        if !self.0.queued.reserve(len) {
            return Err(DropReason::QueueBytes);
        }
        Ok((writer::Job { line, at, ack }, len))
    }

    /// Count a drop, and log it at most once per [`DROP_LOG_INTERVAL`].
    fn dropped(&self, reason: DropReason) -> Offered {
        metrics::capture_dropped(reason.as_str());

        let mut log = self.0.drops.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let due = log
            .last
            .is_none_or(|t| now.duration_since(t) >= DROP_LOG_INTERVAL);
        if due {
            let suppressed = std::mem::take(&mut log.suppressed);
            log.last = Some(now);
            drop(log);
            tracing::error!(
                reason = reason.as_str(),
                suppressed,
                "a message was not captured. The message itself is unaffected under                  capture.on_error: continue — this is a gap in the capture, not in the mail"
            );
        } else {
            log.suppressed += 1;
        }
        Offered::Dropped(reason)
    }
}

/// Create the capture directory `0700` if it is not there, and tighten it if it
/// is.
///
/// `0700` rather than the umask's answer: the directory holds every recipient
/// address and every body that passed through, and "as sensitive as a mailbox"
/// is the standard it should be held to on a host where other things run.
fn create_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| {
                anyhow::anyhow!("creating the capture directory {}: {e}", dir.display())
            })?;
    } else if !dir.is_dir() {
        anyhow::bail!(
            "the capture directory {} exists and is not a directory",
            dir.display()
        );
    } else {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }

    // The container runs as uid 1000 against whatever volume was mounted, so a
    // bare EACCES from the first message is a plausible outcome. Finding out now
    // and saying which uid is what makes it a two-minute fix.
    let probe = dir.join(".simmer-capture-probe");
    std::fs::File::create(&probe).map_err(|e| {
        anyhow::anyhow!(
            "the capture directory {} is not writable by uid {}: {e}",
            dir.display(),
            process_uid()
        )
    })?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// The process's uid, for the error message above.
///
/// `/proc/self` is owned by the process's effective uid, which saves a `libc`
/// dependency for one number — and D-060's rule is that reaching for a crate to
/// get at one helper is a decision, not a Cargo.toml line.
fn process_uid() -> u32 {
    std::fs::metadata("/proc/self")
        .map(|m| {
            use std::os::unix::fs::MetadataExt as _;
            m.uid()
        })
        .unwrap_or(0)
}
