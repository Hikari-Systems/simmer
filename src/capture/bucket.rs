//! The 10-minute bucket a captured record is filed under, and the filename that
//! names it.
//!
//! ```text
//! <directory>/2026-09-20T14.10.jsonl
//! ```
//!
//! `%Y-%m-%dT%H.%M` in **UTC**, the minute floored to a multiple of ten. Three
//! properties are load-bearing and each is asserted below:
//!
//! - **It sorts.** The minute is zero-padded (`.00`, not `.0`), so lexical order
//!   is time order under `ls`, `find`, a shell glob and a plain `Vec::sort`. The
//!   reader, the sweeper and anyone debugging by hand all lean on that.
//! - **It round-trips.** [`parse`] is the exact inverse of [`name`], so the
//!   reader can decide from the filename alone whether a file can hold a record
//!   in its range, without opening it.
//! - **It is portable.** `.` rather than `:` between hour and minute: a colon is
//!   legal on Linux and is not on Windows, and a capture directory gets copied
//!   off a host for analysis more often than it gets read in place.
//!
//! A filename that does not round-trip is **not ours** and is ignored rather
//! than repaired — a capture directory is an ordinary directory and may hold a
//! `README`, a `.gz` someone made, or a half-finished `scp`.

use chrono::{DateTime, NaiveDateTime, Utc};

/// The bucket width. Ten minutes, per the feature's whole premise; not
/// configurable, because it is baked into every filename already on disk and a
/// directory holding two widths could not be read back unambiguously.
pub const BUCKET_SECS: i64 = 600;

/// The format between the directory and the extension.
const FORMAT: &str = "%Y-%m-%dT%H.%M";

/// The extension, including the dot.
pub const EXT: &str = ".jsonl";

/// The start of the bucket containing `at`.
///
/// `div_euclid` rather than `/`: integer division truncates toward zero, which
/// for a pre-1970 instant would floor the wrong way and put a record in the
/// bucket after its own. Simmer will never see one, but a wrong answer that is
/// unreachable today is still a wrong answer to read later.
pub fn floor(at: DateTime<Utc>) -> DateTime<Utc> {
    let floored = at.timestamp().div_euclid(BUCKET_SECS) * BUCKET_SECS;
    DateTime::from_timestamp(floored, 0).unwrap_or(at)
}

/// The filename for the bucket containing `at`.
pub fn name(at: DateTime<Utc>) -> String {
    format!("{}{EXT}", floor(at).format(FORMAT))
}

/// The bucket start a filename names, or `None` if it is not one of ours.
///
/// Strict by construction: the candidate is re-rendered with [`name`] and
/// compared. That rejects a stray minute (`14.13`), a missing pad (`14.1`), a
/// trailing suffix (`....jsonl.gz`) and anything else that merely looks close,
/// without a second set of rules to keep in step with the first.
pub fn parse(file_name: &str) -> Option<DateTime<Utc>> {
    let stem = file_name.strip_suffix(EXT)?;
    let at = NaiveDateTime::parse_from_str(stem, FORMAT).ok()?.and_utc();
    (name(at) == file_name).then_some(at)
}

/// The half-open window `[start, start + 10m)` a bucket covers.
pub fn window(start: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    (start, start + chrono::Duration::seconds(BUCKET_SECS))
}

/// Whether a bucket beginning at `start` can hold a record in `[from, to)`.
///
/// Both ranges are half-open, so a bucket ending exactly at `from` holds
/// nothing wanted and a bucket beginning exactly at `to` likewise.
pub fn intersects(start: DateTime<Utc>, from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
    let (b_from, b_to) = window(start);
    b_from < to && from < b_to
}

/// Every bucket filename whose window intersects `[from, to)`, in time order.
///
/// The reader does **not** use this — it lists the directory and filters with
/// [`parse`] and [`intersects`], which copes with gaps, foreign files and a
/// range of any length. This exists for the sweeper's tests and for anyone who
/// wants to name the files a range *should* have produced.
///
/// Empty when `to <= from`.
pub fn covering(from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<String> {
    if to <= from {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut at = floor(from);
    let last = floor(to - chrono::Duration::nanoseconds(1));
    while at <= last {
        out.push(name(at));
        at += chrono::Duration::seconds(BUCKET_SECS);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn the_minute_floors_to_a_multiple_of_ten() {
        for (input, want) in [
            ("2026-09-20T14:13:02.418Z", "2026-09-20T14:10:00Z"),
            ("2026-09-20T14:10:00.000Z", "2026-09-20T14:10:00Z"),
            ("2026-09-20T14:19:59.999Z", "2026-09-20T14:10:00Z"),
            ("2026-09-20T14:00:00.000Z", "2026-09-20T14:00:00Z"),
            ("2026-09-20T14:59:59.999Z", "2026-09-20T14:50:00Z"),
        ] {
            assert_eq!(floor(at(input)), at(want), "{input}");
        }
    }

    #[test]
    fn the_minute_is_zero_padded_so_the_names_sort_in_time_order() {
        let mut names: Vec<String> = (0..6)
            .map(|i| name(at("2026-09-20T14:00:00Z") + chrono::Duration::minutes(i * 10)))
            .collect();
        let sorted = {
            let mut c = names.clone();
            c.sort();
            c
        };
        assert_eq!(names, sorted, "lexical order must be time order");
        assert_eq!(names.remove(0), "2026-09-20T14.00.jsonl");
        assert_eq!(names.pop().unwrap(), "2026-09-20T14.50.jsonl");
    }

    #[test]
    fn an_hour_and_a_day_boundary_sort_after_what_precedes_them() {
        // The two places a naive format would break: 23.50 -> 00.00 of the next
        // day, and 14.50 -> 15.00.
        assert!(name(at("2026-09-20T15:00:00Z")) > name(at("2026-09-20T14:50:00Z")));
        assert!(name(at("2026-09-21T00:00:00Z")) > name(at("2026-09-20T23:50:00Z")));
    }

    #[test]
    fn name_and_parse_are_inverses() {
        for s in [
            "2026-09-20T14:13:02.418Z",
            "2026-01-01T00:00:00Z",
            "2026-12-31T23:59:59.999Z",
            "2026-02-28T12:34:56Z",
        ] {
            let bucket = floor(at(s));
            assert_eq!(parse(&name(at(s))), Some(bucket), "{s}");
        }
    }

    #[test]
    fn a_filename_that_is_not_ours_is_ignored_rather_than_repaired() {
        for candidate in [
            "README",
            "2026-09-20T14.13.jsonl",    // not a bucket start
            "2026-09-20T14.1.jsonl",     // unpadded
            "2026-09-20T14.10.jsonl.gz", // someone compressed it
            "2026-09-20T14.10.json",     // wrong extension
            "2026-09-20T14:10.jsonl",    // colon, not our separator
            "2026-13-20T14.10.jsonl",    // month 13
            ".jsonl",
            "",
        ] {
            assert_eq!(parse(candidate), None, "{candidate}");
        }
    }

    #[test]
    fn a_bucket_intersects_only_the_ranges_that_could_hold_its_records() {
        let b = at("2026-09-20T14:10:00Z");
        // Half-open on both sides: touching at an endpoint is not an overlap.
        assert!(!intersects(
            b,
            at("2026-09-20T14:00:00Z"),
            at("2026-09-20T14:10:00Z")
        ));
        assert!(!intersects(
            b,
            at("2026-09-20T14:20:00Z"),
            at("2026-09-20T14:30:00Z")
        ));
        // Wholly inside, straddling either edge, and wholly containing.
        assert!(intersects(
            b,
            at("2026-09-20T14:12:00Z"),
            at("2026-09-20T14:13:00Z")
        ));
        assert!(intersects(
            b,
            at("2026-09-20T14:05:00Z"),
            at("2026-09-20T14:11:00Z")
        ));
        assert!(intersects(
            b,
            at("2026-09-20T14:19:00Z"),
            at("2026-09-20T14:25:00Z")
        ));
        assert!(intersects(
            b,
            at("2026-09-20T00:00:00Z"),
            at("2026-09-21T00:00:00Z")
        ));
    }

    #[test]
    fn covering_spans_the_range_and_nothing_either_side() {
        // Wholly inside one bucket.
        assert_eq!(
            covering(at("2026-09-20T14:11:00Z"), at("2026-09-20T14:12:00Z")),
            vec!["2026-09-20T14.10.jsonl"]
        );
        // A range ending exactly on a boundary does not pull in the next bucket.
        assert_eq!(
            covering(at("2026-09-20T14:00:00Z"), at("2026-09-20T14:20:00Z")),
            vec!["2026-09-20T14.00.jsonl", "2026-09-20T14.10.jsonl"]
        );
        // Across an hour.
        assert_eq!(
            covering(at("2026-09-20T14:45:00Z"), at("2026-09-20T15:05:00Z")),
            vec![
                "2026-09-20T14.40.jsonl",
                "2026-09-20T14.50.jsonl",
                "2026-09-20T15.00.jsonl"
            ]
        );
    }

    #[test]
    fn an_empty_or_inverted_range_covers_nothing() {
        let t = at("2026-09-20T14:10:00Z");
        assert!(covering(t, t).is_empty());
        assert!(covering(t, t - chrono::Duration::hours(1)).is_empty());
    }

    #[test]
    fn every_name_covering_produces_parses_back_into_the_range() {
        let from = at("2026-09-20T23:45:00Z");
        let to = at("2026-09-21T00:15:00Z");
        let names = covering(from, to);
        assert_eq!(names.len(), 4, "{names:?}");
        for n in &names {
            let start =
                parse(n).unwrap_or_else(|| panic!("covering produced {n}, which parse rejects"));
            assert!(intersects(start, from, to), "{n}");
        }
    }

    #[test]
    fn a_pre_epoch_instant_floors_backwards_not_toward_zero() {
        // Unreachable in service; a wrong answer here would still be wrong when
        // read back. 1969-12-31T23:55:00Z belongs to the 23.50 bucket.
        let t = at("1969-12-31T23:55:00Z");
        assert_eq!(floor(t), at("1969-12-31T23:50:00Z"));
        assert_eq!(name(t), "1969-12-31T23.50.jsonl");
    }
}
