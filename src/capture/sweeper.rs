//! Retention: bucket files whose window ended longer ago than
//! `capture.retention` are deleted.
//!
//! `src/frequency/sweeper.rs`'s shape, on the same shutdown token, for the same
//! reason it exists there: §7.3 requires a sweeper over `recipient_event`
//! because an append-only store with no eviction is a disk incident waiting for
//! a quiet week. A capture directory is the same store with bigger rows.
//!
//! Two differences from that one, and both follow from what is being swept.
//!
//! **It deletes whole files, never parts of them.** A bucket is the unit: its
//! name says when it ended, so deciding is a string parse rather than a read.
//! Nothing is ever rewritten in place, which means a reader holding a file open
//! keeps reading it even as the sweeper unlinks it.
//!
//! **It only ever deletes files it can prove are ours.** The candidate must
//! round-trip through [`bucket::parse`], so a `README`, a `.gz` someone made or
//! a half-finished `scp` in the same directory is left alone. The alternative —
//! a glob — would eventually delete somebody's notes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;

use super::bucket;
use crate::metrics;
use crate::smtp::Shutdown;

/// How often to sweep. Hourly, like §7.3's: nothing waits on a deletion, and a
/// bucket outliving its retention by up to an hour costs only the disk it sits
/// on. The first pass runs immediately, so a restart after a retention change
/// applies it at once rather than an hour later.
const INTERVAL: Duration = Duration::from_secs(3_600);

/// Run until `shutdown` fires.
///
/// Not started at all when `capture:` is absent — nothing writes bucket files
/// then, so there is nothing to evict and no reason to wake hourly to discover
/// that. The `frequency::sweeper` precedent.
pub async fn run(dir: PathBuf, retention: Duration, shutdown: Shutdown) {
    let mut ticker = tokio::time::interval(INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::info!(
        dir = %dir.display(),
        retention_secs = retention.as_secs(),
        interval_secs = INTERVAL.as_secs(),
        "capture sweeper started"
    );

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => sweep_once(&dir, retention).await,
        }
    }

    tracing::info!("capture sweeper stopped");
}

/// One pass. Split out so a test can drive it without waiting an hour — the
/// `frequency::sweeper::sweep_once` precedent.
pub async fn sweep_once(dir: &Path, retention: Duration) {
    let cutoff = match chrono::Duration::from_std(retention) {
        Ok(d) => Utc::now() - d,
        Err(_) => {
            // Only reachable past ~292 million years of retention. Sweeping
            // nothing is the safe answer: a cutoff in the future would delete
            // the file being written.
            tracing::error!(
                retention_secs = retention.as_secs(),
                "capture retention is too large to compute a cutoff; skipping the sweep"
            );
            return;
        }
    };

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(dir = %dir.display(), error = %e, "reading the capture directory");
            return;
        }
    };

    let mut deleted = 0u64;
    let mut freed = 0u64;
    let mut remaining = 0u64;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Not ours unless the name round-trips. A glob would eventually delete
        // something an operator put here.
        let Some(start) = bucket::parse(name) else {
            continue;
        };
        let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);

        // The bucket's *end*, not its start: a file named 14.10 holds records up
        // to 14:20, and deleting it at 14:10 + retention would take up to ten
        // minutes of history early.
        let (_, ends) = bucket::window(start);
        if ends <= cutoff {
            match tokio::fs::remove_file(entry.path()).await {
                Ok(()) => {
                    deleted += 1;
                    freed += size;
                }
                Err(e) => {
                    tracing::error!(file = %entry.path().display(), error = %e, "deleting a capture file");
                    remaining += size;
                }
            }
        } else {
            remaining += size;
        }
    }

    // A gauge recomputed here rather than tracked by the writer: this is the
    // only place that has looked at the whole directory, and a number derived
    // from what is actually on disk cannot drift from it (D-056's reasoning).
    metrics::capture_disk_bytes(remaining);

    if deleted > 0 {
        metrics::capture_files_swept(deleted);
        tracing::info!(
            deleted,
            freed_bytes = freed,
            remaining_bytes = remaining,
            cutoff = %cutoff.to_rfc3339(),
            "evicted capture files past their retention"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a file named for `minutes_ago`, rounded to its bucket.
    fn plant(dir: &Path, minutes_ago: i64) -> PathBuf {
        let at = Utc::now() - chrono::Duration::minutes(minutes_ago);
        let path = dir.join(bucket::name(at));
        std::fs::write(&path, b"{\"v\":1}\n").expect("plant");
        path
    }

    #[tokio::test]
    async fn deletes_only_the_buckets_whose_window_ended_before_the_cutoff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = plant(dir.path(), 180);
        let recent = plant(dir.path(), 5);

        sweep_once(dir.path(), Duration::from_secs(3600)).await;

        assert!(!old.exists(), "a three-hour-old bucket should be gone");
        assert!(recent.exists(), "the bucket being written must survive");
    }

    #[tokio::test]
    async fn the_bucket_currently_being_written_is_never_deleted() {
        // The boundary case the `retention >= one bucket` validation exists for:
        // even at the shortest legal retention, now's bucket has not ended.
        let dir = tempfile::tempdir().expect("tempdir");
        let current = plant(dir.path(), 0);
        sweep_once(dir.path(), Duration::from_secs(600)).await;
        assert!(current.exists());
    }

    #[tokio::test]
    async fn a_file_that_is_not_ours_is_left_alone_however_old_it_looks() {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in [
            "README",
            "2026-09-20T14.13.jsonl",
            "1999-01-01T00.00.jsonl.gz",
            "notes.txt",
        ] {
            std::fs::write(dir.path().join(name), b"not ours").expect("write");
        }
        sweep_once(dir.path(), Duration::from_secs(600)).await;
        for name in [
            "README",
            "2026-09-20T14.13.jsonl",
            "1999-01-01T00.00.jsonl.gz",
            "notes.txt",
        ] {
            assert!(dir.path().join(name).exists(), "{name} was deleted");
        }
    }

    #[tokio::test]
    async fn a_missing_directory_is_reported_rather_than_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("never-created");
        sweep_once(&gone, Duration::from_secs(600)).await;
    }

    #[tokio::test]
    async fn an_empty_directory_sweeps_nothing_and_says_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        sweep_once(dir.path(), Duration::from_secs(600)).await;
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
