//! §3.2 step 3c′ (D-091) — the partial ramp.
//!
//! On day `i` of a route's ramp, only `warmup.schedule.share[i]` of the messages
//! that reach it in the walk are offered to it. The rest skip to the next link,
//! exactly as §7.3 steers, so the cap is reached later in the day instead of in
//! its first hours. Past the end of `share`, every message is offered and the
//! walk is what it always was.
//!
//! Which messages are offered is a **keyed hash**, not a dice roll: the §7.3
//! salt over the normalised recipient, the route and the day index. That is what
//! lets two instances agree without sharing any state, and what lets §9.4's dry
//! run report the real result rather than a probability. The route is in the
//! hash so two partial routes in one chain decide independently; the day index
//! is in it so a recipient's route can change from one day to the next.
//!
//! Nothing about the recipient leaves this module — not in a log line, not in a
//! metric. §7.3 hashes so that the container keeps no record of who was mailed.

use crate::config::{FrequencyMode, Route};
use crate::frequency::{self, Keyer};
use crate::quota::store::RouteState;

/// The share in force for this route today, or `None` when every message is
/// offered: no `share` list, past its end, a share of 1, or the route graduated
/// — §9.3's graduate jumps to the end of the ramp, and this is part of the ramp.
pub fn share_today(route: &Route, day_index: i64, state: RouteState) -> Option<f64> {
    if state.graduated {
        return None;
    }
    route.warmup.as_ref()?.schedule.share_for(day_index)
}

/// Is this message offered to `route` today, at `share`?
pub fn offered(
    keyer: &Keyer,
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
    let key = keyer.key(&format!("{normalised}\0{route}\0{day_index}"));
    let mut head = [0u8; 8];
    head.copy_from_slice(&key.as_bytes()[..8]);
    // A uniform draw from [0, 1) with 53 bits of precision, which is all an f64
    // share can distinguish anyway.
    let draw = (u64::from_be_bytes(head) >> 11) as f64 / (1u64 << 53) as f64;
    draw < share
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
            .map(|r| offered(k, route, &r, day, share, &[]))
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
                .filter(|r| offered(&k, "warming", r, 0, share, &[]))
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
    fn recipients_that_normalise_alike_get_the_same_answer() {
        let k = keyer();
        let dots = vec!["gmail.com".to_string()];
        for i in 0..200 {
            let plain = format!("bobsmith{i}@gmail.com");
            let dressed = format!("Bob.Smith{i}+news@GMAIL.com");
            assert_eq!(
                offered(&k, "warming", &plain, 2, 0.5, &dots),
                offered(&k, "warming", &dressed, 2, 0.5, &dots),
            );
        }
    }
}
