//! Is a resource series growing? The soak tier's leak criterion.
//!
//! Resident memory is noisy — allocator arenas, bursts, fragmentation — so the
//! raw series is never tested directly:
//!
//! 1. [`floors`] reduces it to the minimum in each window. A leak raises the
//!    floor; a burst does not.
//! 2. [`theil_sen`] fits a slope that a few outliers cannot drag, unlike least
//!    squares.
//! 3. [`verdict`] fails only when the slope is over its threshold **and** the
//!    last quarter's floors sit above the first quarter's by at least half of what
//!    a leak at exactly the threshold would have produced over the run. Neither a
//!    steep fit to a flat series nor a one-off step is enough on its own.
//!
//! The second gate is **derived from the slope threshold and the run's length**,
//! never a fixed amount. A fixed 4 MiB looked reasonable and was wrong: a leak at
//! 2.2 MiB/h — the 64 B-per-message calibration target at 10 msg/s — moves the
//! quartile medians by only about 1.4 MiB in a one-hour run, so a fixed gate let
//! exactly the leak the soak exists to catch pass. The self-test that pins this
//! plants that leak, not a convenient larger one.
//!
//! Pure functions over `(seconds, value)` samples; tested in
//! `tests/harness_selftest.rs` against flat, noisy and planted-leak series.

/// The minimum value in each consecutive `window`-second window, stamped with the
/// window's start. Windows with no samples are skipped.
pub fn floors(samples: &[(f64, f64)], window: f64) -> Vec<(f64, f64)> {
    assert!(window > 0.0, "window must be positive");
    let Some(start) = samples.first().map(|s| s.0) else {
        return Vec::new();
    };
    let mut out: Vec<(f64, f64)> = Vec::new();
    for &(t, v) in samples {
        let slot = ((t - start) / window).floor();
        let slot_start = start + slot * window;
        match out.last_mut() {
            Some(last) if last.0 == slot_start => last.1 = last.1.min(v),
            _ => out.push((slot_start, v)),
        }
    }
    out
}

/// The Theil–Sen slope: the median of every pairwise slope, in value per second.
/// `None` for fewer than two points.
pub fn theil_sen(points: &[(f64, f64)]) -> Option<f64> {
    let mut slopes = Vec::with_capacity(points.len() * points.len() / 2);
    for (i, a) in points.iter().enumerate() {
        for b in &points[i + 1..] {
            if b.0 != a.0 {
                slopes.push((b.1 - a.1) / (b.0 - a.0));
            }
        }
    }
    median(&mut slopes)
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    })
}

/// Thresholds for one series.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Fail above this growth, in value per hour. The quartile gate follows from
    /// it; see the module comment.
    pub slope_per_hour: f64,
}

/// Where the first and last quarters' medians sit, as a fraction of the span:
/// the medians of the first and last quarters are 3/4 of the span apart.
const QUARTILE_SEPARATION: f64 = 0.75;

/// How much of a threshold-rate leak's quartile step the gate demands. Half:
/// enough to reject floors that merely wobble, loose enough that a leak just over
/// the threshold still clears it.
const STEP_FRACTION: f64 = 0.5;

#[derive(Debug, Clone)]
pub struct Verdict {
    pub slope_per_hour: f64,
    pub quartile_step: f64,
    pub leaking: bool,
    /// Too few post-warm-up floors to judge: the checks report but cannot fail.
    pub inconclusive: bool,
}

/// Judge `samples` after discarding the first `warmup` seconds.
///
/// `floor_window` is the floor window in seconds (the soak uses 300), and fewer
/// than eight post-warm-up floors is inconclusive: a slope through a handful of
/// points is a guess, and a gate built on a guess is a flake.
pub fn verdict(samples: &[(f64, f64)], warmup: f64, floor_window: f64, limits: Limits) -> Verdict {
    let start = samples.first().map(|s| s.0).unwrap_or(0.0);
    let steady: Vec<(f64, f64)> = samples
        .iter()
        .copied()
        .filter(|(t, _)| *t - start >= warmup)
        .collect();
    let fl = floors(&steady, floor_window);

    if fl.len() < 8 {
        return Verdict {
            slope_per_hour: theil_sen(&fl).unwrap_or(0.0) * 3600.0,
            quartile_step: 0.0,
            leaking: false,
            inconclusive: true,
        };
    }

    let slope_per_hour = theil_sen(&fl).expect("eight points") * 3600.0;
    let q = fl.len() / 4;
    let mut first: Vec<f64> = fl[..q].iter().map(|p| p.1).collect();
    let mut last: Vec<f64> = fl[fl.len() - q..].iter().map(|p| p.1).collect();
    let quartile_step = median(&mut last).expect("q >= 2") - median(&mut first).expect("q >= 2");

    let span_hours = (fl.last().expect("floors").0 - fl[0].0 + floor_window) / 3600.0;
    let step_gate = STEP_FRACTION * limits.slope_per_hour * QUARTILE_SEPARATION * span_hours;

    Verdict {
        slope_per_hour,
        quartile_step,
        leaking: slope_per_hour > limits.slope_per_hour && quartile_step > step_gate,
        inconclusive: false,
    }
}
