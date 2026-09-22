//! D-092 — jemalloc's own counters, written to a file for the soak tier.
//!
//! The soak's memory gate judges cgroup `anon`, which cannot tell memory the
//! program still holds from memory the allocator kept after it was freed.
//! `SOAK.md` §12's hour failed that gate on a series that stepped up once after a
//! spike and then held — the allocator's shape, not a leak's — and nothing in the
//! samples could say which. jemalloc can: `allocated` is the bytes the program
//! holds, `resident` and `retained` are what the allocator has kept.
//!
//! Two switches, both off in anything published:
//!
//! - the `alloc-stats` cargo feature, which builds jemalloc with its `stats`
//!   option and links `tikv-jemalloc-ctl`. Without it there is nothing to read;
//!   the shipped binary is the allocator D-078 measured, unchanged.
//! - `SIMMER_ALLOC_STATS_FILE`, naming the file to write. Unset, no task starts.
//!
//! It is a **file**, not a `/metrics` series, on purpose. The soak scrapes `app`
//! and never `app2`, so that the exporter's own cost shows up as a difference
//! between them (V2). The harness already samples both through `docker exec`, so
//! a file each instance writes on the same interval keeps both instances paying
//! the same cost and V2's asymmetry intact.
//!
//! The rendering and the parser live here unconditionally, so the soak harness
//! reads exactly what the server writes, in a build without the feature.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::smtp::Shutdown;

/// The environment variable naming the file. Unset or empty means off.
pub const ENV: &str = "SIMMER_ALLOC_STATS_FILE";

/// Once per sampling tick of the soak's (10 s), twice over: a sample is never
/// more than 5 s stale, and an epoch advance every 5 s is nothing.
pub const INTERVAL: Duration = Duration::from_secs(5);

/// One reading of jemalloc's global counters, in bytes. The names are jemalloc's
/// `stats.*` names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Bytes the program holds. A leak grows this; allocator retention does not.
    pub allocated: u64,
    /// Bytes in active pages: `allocated` plus the slack within them.
    pub active: u64,
    /// Bytes physically resident in pages the allocator maps — what `anon` sees.
    pub resident: u64,
    /// Bytes unmapped from use but kept as virtual memory for reuse.
    pub retained: u64,
    /// Bytes in active extents mapped by the allocator.
    pub mapped: u64,
    /// The allocator's own bookkeeping.
    pub metadata: u64,
}

const KEYS: [&str; 6] = [
    "je_allocated",
    "je_active",
    "je_resident",
    "je_retained",
    "je_mapped",
    "je_metadata",
];

impl Snapshot {
    fn values(&self) -> [u64; 6] {
        [
            self.allocated,
            self.active,
            self.resident,
            self.retained,
            self.mapped,
            self.metadata,
        ]
    }

    /// One `key value` line per counter. The keys are prefixed so they cannot
    /// collide with the other lines the soak's sampling command prints —
    /// `memory.stat` has a `file` of its own.
    pub fn render(&self) -> String {
        KEYS.iter()
            .zip(self.values())
            .map(|(k, v)| format!("{k} {v}\n"))
            .collect()
    }

    /// Fold one line of [`render`](Self::render)'s output in. Returns whether the
    /// line was one of ours, so a caller scanning mixed output can skip the rest.
    pub fn absorb(&mut self, line: &str) -> bool {
        let mut f = line.split_whitespace();
        let (Some(key), Some(value), None) = (f.next(), f.next(), f.next()) else {
            return false;
        };
        let Ok(value) = value.parse() else {
            return false;
        };
        let slot = match key {
            "je_allocated" => &mut self.allocated,
            "je_active" => &mut self.active,
            "je_resident" => &mut self.resident,
            "je_retained" => &mut self.retained,
            "je_mapped" => &mut self.mapped,
            "je_metadata" => &mut self.metadata,
            _ => return false,
        };
        *slot = value;
        true
    }
}

/// Whether this binary can read the counters at all.
pub const fn compiled_in() -> bool {
    cfg!(feature = "alloc-stats")
}

/// Read the counters now. `stats.*` are cached until the epoch advances, so it
/// advances first — without that every read would repeat the first.
#[cfg(feature = "alloc-stats")]
pub fn read() -> Result<Snapshot, String> {
    use tikv_jemalloc_ctl::{epoch, stats};
    let e = |err: tikv_jemalloc_ctl::Error| err.to_string();
    epoch::advance().map_err(e)?;
    Ok(Snapshot {
        allocated: stats::allocated::read().map_err(e)? as u64,
        active: stats::active::read().map_err(e)? as u64,
        resident: stats::resident::read().map_err(e)? as u64,
        retained: stats::retained::read().map_err(e)? as u64,
        mapped: stats::mapped::read().map_err(e)? as u64,
        metadata: stats::metadata::read().map_err(e)? as u64,
    })
}

#[cfg(not(feature = "alloc-stats"))]
pub fn read() -> Result<Snapshot, String> {
    Err("built without the alloc-stats feature".to_string())
}

/// The configured file, if any. Empty counts as unset, because compose renders
/// an unset `${VAR:-}` as an empty string rather than leaving it out.
pub fn configured() -> Option<PathBuf> {
    std::env::var_os(ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Write the counters to `path` every [`INTERVAL`] until shutdown.
///
/// Through a temporary file and a rename, so a reader never sees half a file. A
/// failed read or write is logged once and the loop carries on: a debugging aid
/// must not be able to take the process down.
pub async fn run(path: PathBuf, shutdown: Shutdown) {
    let mut ticker = tokio::time::interval(INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut warned = false;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {
                if let Err(e) = write_once(&path) {
                    if !warned {
                        tracing::warn!(path = %path.display(), error = %e, "could not write allocator stats (D-092)");
                        warned = true;
                    }
                }
            }
        }
    }
}

/// Plain `std::fs`, on the task, deliberately. `tokio::fs` hands each call to
/// the blocking pool, whose threads linger ~10 s after use, so a write every
/// 5 s kept one alive and sometimes started a second. The first soak with this
/// on failed its threads-at-rest check (4 before, 5 after) on the instrument,
/// not on Simmer. ~150 bytes to a tmpfs is microseconds; blocking a worker for
/// that is the cheaper cost, and it starts no thread at all.
fn write_once(path: &Path) -> Result<(), String> {
    let text = read()?.render();
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rendering_parses_back_to_itself() {
        let s = Snapshot {
            allocated: 1,
            active: 22,
            resident: 333,
            retained: 4_444,
            mapped: 55_555,
            metadata: 666_666,
        };
        let mut back = Snapshot::default();
        for line in s.render().lines() {
            assert!(back.absorb(line), "{line}");
        }
        assert_eq!(back, s);
    }

    #[test]
    fn other_lines_are_not_ours() {
        let mut s = Snapshot::default();
        for line in [
            "file 123",
            "anon 5",
            "VmRSS: 10 kB",
            "je_allocated",
            "je_allocated x",
            "01",
        ] {
            assert!(!s.absorb(line), "{line}");
        }
        assert_eq!(s, Snapshot::default());
    }

    /// The server sets jemalloc in `main.rs`, which a library test does not
    /// link. Without this the test would allocate through the system allocator
    /// and watch jemalloc's counters not move.
    #[cfg(feature = "alloc-stats")]
    #[global_allocator]
    static TEST_ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

    #[cfg(feature = "alloc-stats")]
    #[test]
    fn the_counters_are_readable_and_move() {
        let before = read().expect("readable").allocated;
        let hold = std::hint::black_box(vec![1u8; 8 << 20]);
        let during = read().expect("readable").allocated;
        drop(hold);
        // Not exactly 8 MiB: other test threads allocate and free meanwhile.
        assert!(during >= before + (7 << 20), "{before} -> {during}");
    }

    #[cfg(not(feature = "alloc-stats"))]
    #[test]
    fn without_the_feature_there_is_nothing_to_read() {
        assert!(!compiled_in());
        assert!(read().is_err());
    }
}
