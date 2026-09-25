//! §3.2 step 3c′ (D-091, D-097) — the partial ramp.
//!
//! Only part of the messages that reach a warming route in the walk are offered
//! to it. The rest skip to the next link, exactly as §7.3 steers, so the cap is
//! reached later in the day instead of in its first hours.
//!
//! The share is either **listed** per day (`share: [0.1, 0.25]`, D-091) or
//! **computed** per domain group from the day's cap and the clock
//! (`share: {mode: auto}`, D-097). Past the end of a list, every message is
//! offered and the walk is what it always was; `auto` has no end and applies for
//! as long as the route is warming and not graduated.
//!
//! Which messages are offered is a **keyed hash**, not a dice roll: the §7.3
//! salt over the normalised recipient, the ramp, the route and the day index.
//! That is what
//! lets two instances agree without sharing any state, and what lets §9.4's dry
//! run report the real result rather than a probability. The route is in the
//! hash so two partial routes in one chain decide independently; the ramp is in
//! it so two ramps' same-named routes do too (D-099); the day index is in it so
//! a recipient's route can change from one day to the next.
//!
//! Nothing about the recipient leaves this module — not in a log line, not in a
//! metric. §7.3 hashes so that the container keeps no record of who was mailed.

use chrono::{DateTime, Utc};

use crate::config::{AutoShare, FrequencyMode, Route};
use crate::frequency::{self, Keyer};
use crate::quota::store::{RouteState, Usage};
use crate::quota::{self, Allowance};

/// The share a **list** puts in force for this route today, or `None` when every
/// message is offered: no `share` list, past its end, a share of 1, or the route
/// graduated — §9.3's graduate jumps to the end of the ramp, and this is part of
/// the ramp.
///
/// `auto` is `None` here too. [`share_for_group`] is what resolves either kind.
pub fn share_today(route: &Route, day_index: i64, state: RouteState) -> Option<f64> {
    if state.graduated {
        return None;
    }
    route.warmup.as_ref()?.schedule.share_for(day_index)
}

/// Does deciding this route's share need the day's quota row read first?
///
/// True only for an `auto` route that is warming and not graduated, so the
/// listed case and every route without a ramp pay nothing for D-097.
pub fn needs_usage(route: &Route, state: RouteState) -> bool {
    !state.graduated
        && route
            .warmup
            .as_ref()
            .is_some_and(|w| w.schedule.share.auto().is_some())
}

/// The share in force for one `(route, domain_group)` right now, or `None` when
/// every message is offered.
///
/// **The one entry point.** The walk, §9.4's dry run and §9.2's `/routes` all
/// call this, which is what stops the control plane from describing a decision
/// the walk did not make.
///
/// `usage` is the day's row, or `None` where no row exists yet — nothing has
/// been sent on this route and group today, so the cap is untouched. `scheduled`
/// is what §7.2 says the allowance is, used only when the row does not carry one.
pub fn share_for_group(
    route: &Route,
    day_index: i64,
    state: RouteState,
    now: DateTime<Utc>,
    scheduled: Allowance,
    usage: Option<&Usage>,
) -> Option<f64> {
    if state.graduated {
        return None;
    }
    let warmup = route.warmup.as_ref()?;
    let Some(auto) = warmup.schedule.share.auto() else {
        return warmup.schedule.share_for(day_index);
    };
    if day_index < 0 {
        // §7.2: a `started` in the future makes the route ineligible anyway, and
        // the walk has already skipped it. Nothing to ramp.
        return None;
    }

    // The allowance the reservation will actually be judged against: the row's
    // once a row exists (D-026), §9.3's override where it is set, and only
    // otherwise what the schedule says. Reading it any other way would let the
    // ramp pace against a ceiling that is not the one being enforced.
    let effective = match usage {
        Some(u) => Usage {
            allowance: u.allowance.or(scheduled.as_column()),
            ..*u
        }
        .effective_allowance(),
        None => scheduled.as_column(),
    };
    // No ceiling is an overflow route, which §4.2 refuses to let carry an auto
    // ramp; there is nothing to pace against either way.
    let allowance = effective?;

    let used = usage.map_or(0, |u| u.committed + u.reserved);
    let share = auto_share(
        auto,
        allowance,
        used,
        quota::day::fraction_elapsed(route, now),
    );
    Some(share).filter(|s| *s < 1.0)
}

/// D-097's controller, as arithmetic: no clock, no configuration lookup, no row.
///
/// ```text
/// c  = (allowance - used) / allowance      the cap still to fill, 1 -> 0
/// t' = (fill_by - elapsed) / fill_by       the fill window still to run, 1 -> 0
///
/// remaining <= tail.below * allowance  ->  tail.ceiling   (the release)
/// t' == 0                              ->  ceiling        (past the deadline)
/// otherwise      clamp(floor, ceiling, (c / t') ^ gain)
/// ```
///
/// Ahead of pace the ratio falls and the share collapses; behind it, or as the
/// window closes, the ratio rises and the share opens to the ceiling. The window
/// being shorter than the day is what overstates the share throughout: the cap is
/// met early rather than exactly, because simmer cannot delay a message and so
/// cannot rely on later traffic existing to fill it.
///
/// Returns a value in `[floor, ceiling]`, or `tail.ceiling` when released. Never
/// `NaN`: every degenerate input is answered explicitly.
pub fn auto_share(p: &AutoShare, allowance: i64, used: i64, elapsed: f64) -> f64 {
    // A cap of nothing cannot be paced against, and a route with one has no
    // headroom to offer anyway.
    if allowance <= 0 {
        return p.ceiling;
    }
    // `used` can exceed the allowance: D-090 reserves a pinned reply past the
    // cap. Remaining is floored at zero, as §7.4's headroom is.
    let remaining = (allowance - used).max(0);

    // The tail release, first: it is the arm that makes a ramp finish, so it
    // must not be reachable only when the controller happens to agree.
    if p.tail.below > 0.0 && (remaining as f64) <= p.tail.below * allowance as f64 {
        return p.tail.ceiling;
    }

    let c = remaining as f64 / allowance as f64;
    let window_left = p.fill_by - elapsed.clamp(0.0, 1.0);
    if window_left <= 0.0 {
        return p.ceiling;
    }
    let t = window_left / p.fill_by;

    (c / t).powf(p.gain).clamp(p.floor, p.ceiling)
}

/// Is this message offered to `route` today, at `share`?
pub fn offered(
    keyer: &Keyer,
    ramp: &str,
    route: &str,
    recipient: &str,
    day_index: i64,
    share: f64,
    dot_insensitive: &[String],
) -> bool {
    if share >= 1.0 {
        return true;
    }
    let normalised = frequency::normalise(recipient, FrequencyMode::ToAddress, dot_insensitive);
    let key = keyer.key(&format!("{normalised}\0{ramp}\0{route}\0{day_index}"));
    let mut head = [0u8; 8];
    head.copy_from_slice(&key.as_bytes()[..8]);
    // A uniform draw from [0, 1) with 53 bits of precision, which is all an f64
    // share can distinguish anyway.
    let draw = (u64::from_be_bytes(head) >> 11) as f64 / (1u64 << 53) as f64;
    draw < share
}

#[cfg(test)]
mod auto_tests {
    use super::*;
    use crate::config::Tail;

    /// The defaults, so a test that cares about one parameter says which.
    fn params() -> AutoShare {
        AutoShare::default()
    }

    /// Half a day gone, of a fill window that is the whole day: exactly on pace
    /// when the cap is half full.
    const MIDDAY: f64 = 0.5;

    #[test]
    fn the_share_falls_as_the_cap_fills() {
        let p = AutoShare {
            fill_by: 1.0,
            ceiling: 1.0,
            floor: 0.0001,
            tail: Tail {
                below: 0.0,
                ..Tail::default()
            },
            ..params()
        };
        let at = |used| auto_share(&p, 200, used, MIDDAY);

        // Never rising as the cap fills, at one instant: this is the property
        // the whole feature is named for. Not *strictly* falling, because both
        // clamps are plateaus — behind pace it sits at the ceiling, and far
        // ahead of it at the floor.
        let series: Vec<f64> = (0..=200).step_by(5).map(at).collect();
        for pair in series.windows(2) {
            assert!(
                pair[0] >= pair[1],
                "share must never rise as the cap fills, got {series:?}"
            );
        }

        // On pace is the ceiling: half the cap gone, half the window left. Being
        // behind pace cannot throttle the route.
        assert_eq!(at(100), 1.0);
        assert_eq!(at(0), 1.0);

        // And past it, it really does fall rather than merely not rise.
        assert!(at(120) > at(140), "{} vs {}", at(120), at(140));
        assert!(at(140) > at(160));
    }

    #[test]
    fn the_share_rises_as_the_fill_window_closes() {
        let p = AutoShare {
            fill_by: 1.0,
            tail: Tail {
                below: 0.0,
                ..Tail::default()
            },
            ..params()
        };
        let at = |elapsed| auto_share(&p, 200, 150, elapsed);

        // Three quarters of the cap gone. As the window closes the share must
        // never fall, and must actually open: otherwise the ramp ends the day
        // stalled at the floor with the cap unmet, which is the failure D-097
        // exists to prevent.
        let series: Vec<f64> = (0..=20).map(|i| at(f64::from(i) / 20.0)).collect();
        for pair in series.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "share must never fall as the window closes, got {series:?}"
            );
        }
        assert_eq!(at(0.1), p.floor, "early and well ahead of pace: throttled");
        assert!(at(0.7) > at(0.5), "{} vs {}", at(0.7), at(0.5));
        assert!(at(0.5) > at(0.3), "{} vs {}", at(0.5), at(0.3));
    }

    #[test]
    fn past_the_fill_window_the_share_is_the_ceiling() {
        let p = AutoShare {
            fill_by: 0.6,
            ceiling: 0.5,
            ..params()
        };
        // Still half the cap to go, but the window has closed.
        assert_eq!(auto_share(&p, 200, 100, 0.6), 0.5);
        assert_eq!(auto_share(&p, 200, 100, 0.9), 0.5);
        assert_eq!(auto_share(&p, 200, 100, 1.0), 0.5);
    }

    #[test]
    fn the_fill_window_overstates_the_share() {
        // The same row and the same instant, paced against the day and against
        // a window ending at 60% of it. The shorter window must offer more:
        // that is what "hit the cap early rather than exactly" buys.
        let whole_day = AutoShare {
            fill_by: 1.0,
            ..params()
        };
        let compressed = AutoShare {
            fill_by: 0.6,
            ..params()
        };
        let (allowance, used, elapsed) = (200, 120, 0.3);
        assert!(
            auto_share(&compressed, allowance, used, elapsed)
                > auto_share(&whole_day, allowance, used, elapsed)
        );
    }

    #[test]
    fn the_tail_releases_to_its_own_ceiling() {
        let p = AutoShare {
            ceiling: 0.5,
            tail: Tail {
                below: 0.1,
                ceiling: 1.0,
            },
            ..params()
        };
        // 21 of 200 left is above a tenth: still throttled, and under the
        // route's ceiling rather than the tail's.
        assert!(auto_share(&p, 200, 179, 0.1) <= 0.5);
        // 20 left is the threshold itself, and everything below it is released.
        assert_eq!(auto_share(&p, 200, 180, 0.1), 1.0);
        assert_eq!(auto_share(&p, 200, 199, 0.1), 1.0);

        // The release has its own ceiling so a route held well under the
        // route's ceiling all day can still close out — or be held at the tail
        // too, if that is what the operator wants.
        let held = AutoShare {
            tail: Tail {
                below: 0.1,
                ceiling: 0.25,
            },
            ..p
        };
        assert_eq!(auto_share(&held, 200, 199, 0.1), 0.25);
    }

    #[test]
    fn a_tail_of_zero_disables_the_release() {
        let p = AutoShare {
            floor: 0.05,
            tail: Tail {
                below: 0.0,
                ceiling: 1.0,
            },
            ..params()
        };
        // One message left in the cap, early in the day: without a release this
        // is the floor, which is exactly the asymptote the release exists to
        // cut short.
        assert_eq!(auto_share(&p, 200, 199, 0.05), 0.05);
    }

    #[test]
    fn the_share_is_clamped_to_the_floor_and_the_ceiling() {
        let p = AutoShare {
            floor: 0.1,
            ceiling: 0.4,
            gain: 8.0,
            tail: Tail {
                below: 0.0,
                ..Tail::default()
            },
            ..params()
        };
        for used in 0..=200 {
            for elapsed in [0.0, 0.25, 0.5, 0.75, 1.0] {
                let share = auto_share(&p, 200, used, elapsed);
                assert!(
                    (0.1..=0.4).contains(&share),
                    "used {used} at {elapsed} gave {share}"
                );
            }
        }
    }

    #[test]
    fn degenerate_rows_never_produce_a_nonsense_share() {
        let p = params();

        // No cap to pace against. Such a route has no headroom either, so the
        // walk skips it on quota; the share must simply not be NaN.
        assert_eq!(auto_share(&p, 0, 0, 0.5), p.ceiling);
        assert_eq!(auto_share(&p, -5, 0, 0.5), p.ceiling);

        // D-090 reserves a pinned reply past the cap, so `used` can exceed the
        // allowance. Remaining floors at zero, as §7.4's headroom does.
        assert_eq!(auto_share(&p, 200, 260, 0.5), p.tail.ceiling);

        // And nothing anywhere in the space is NaN.
        for allowance in [1, 50, 800] {
            for used in [0, 1, 25, 800, 10_000] {
                for elapsed in [0.0, 0.5, 1.0] {
                    assert!(!auto_share(&p, allowance, used, elapsed).is_nan());
                }
            }
        }
    }

    #[test]
    fn the_worked_example_holds() {
        // The table in D-097, which is what the operator reads. A cap of 200, a
        // window ending at 60% of the day, gain 4, ceiling 0.5, floor 0.05.
        let p = AutoShare {
            floor: 0.05,
            ceiling: 0.5,
            gain: 4.0,
            fill_by: 0.6,
            tail: Tail {
                below: 0.1,
                ceiling: 1.0,
            },
        };
        let cases = [
            (0.00, 0, 0.50),
            (0.15, 100, 0.20),
            (0.30, 100, 0.50),
            (0.45, 150, 0.50),
            (0.60, 180, 1.00),
        ];
        for (elapsed, used, expected) in cases {
            let got = auto_share(&p, 200, used, elapsed);
            assert!(
                (got - expected).abs() < 0.005,
                "at {elapsed} with {used} used: expected {expected}, got {got}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyer() -> Keyer {
        Keyer::new(b"a fixed test salt".to_vec())
    }

    fn recipients(n: usize) -> impl Iterator<Item = String> {
        (0..n).map(|i| format!("user{i}@example.com"))
    }

    fn offered_set(k: &Keyer, route: &str, day: i64, share: f64) -> Vec<bool> {
        recipients(2000)
            .map(|r| offered(k, "main", route, &r, day, share, &[]))
            .collect()
    }

    #[test]
    fn the_same_message_gets_the_same_answer() {
        let k = keyer();
        assert_eq!(
            offered_set(&k, "warming", 3, 0.3),
            offered_set(&k, "warming", 3, 0.3)
        );
        // And across keyers holding the same salt, which is two instances.
        assert_eq!(
            offered_set(&k, "warming", 3, 0.3),
            offered_set(&keyer(), "warming", 3, 0.3)
        );
    }

    #[test]
    fn the_offered_fraction_is_the_share() {
        let k = keyer();
        for share in [0.05, 0.25, 0.5, 0.9] {
            let n = 10_000;
            let hit = recipients(n)
                .filter(|r| offered(&k, "main", "warming", r, 0, share, &[]))
                .count();
            let got = hit as f64 / n as f64;
            assert!((got - share).abs() < 0.02, "share {share}: offered {got}");
        }
    }

    #[test]
    fn a_share_of_one_offers_everything() {
        let k = keyer();
        assert!(offered_set(&k, "warming", 0, 1.0).iter().all(|o| *o));
    }

    #[test]
    fn the_day_and_the_route_each_change_the_decision() {
        let k = keyer();
        let base = offered_set(&k, "warming", 0, 0.5);
        assert_ne!(base, offered_set(&k, "warming", 1, 0.5), "day index");
        assert_ne!(base, offered_set(&k, "warming-b", 0, 0.5), "route");
    }

    #[test]
    fn the_ramp_changes_the_decision() {
        // D-099: two ramps may each have a route called `warming`, and each
        // offers its own share of recipients.
        let k = keyer();
        let in_ramp = |ramp: &str| -> Vec<bool> {
            recipients(2000)
                .map(|r| offered(&k, ramp, "warming", &r, 0, 0.5, &[]))
                .collect()
        };
        assert_ne!(in_ramp("main"), in_ramp("brand-b"));
    }

    #[test]
    fn recipients_that_normalise_alike_get_the_same_answer() {
        let k = keyer();
        let dots = vec!["gmail.com".to_string()];
        for i in 0..200 {
            let plain = format!("bobsmith{i}@gmail.com");
            let dressed = format!("Bob.Smith{i}+news@GMAIL.com");
            assert_eq!(
                offered(&k, "main", "warming", &plain, 2, 0.5, &dots),
                offered(&k, "main", "warming", &dressed, 2, 0.5, &dots),
            );
        }
    }
}
