//! D-111 — per-segment sending rates: GCRA with slot booking, as pure functions.
//!
//! The key is the quota's own (§7.1): `(ramp, route, domain_group)`. The state
//! is one instant per key, the **theoretical arrival time** (`tat`): when the
//! bucket would next be empty if every booked message were sent exactly on its
//! interval. GCRA keeps no counter and no refill timer; the whole bucket is the
//! distance between `tat` and now.
//!
//! - `interval` is `3600 s / per_hour`.
//! - A message conforms at `t` if `t >= tat − (burst − 1)·interval`: after an
//!   idle spell exactly `burst` go back to back, and then one per interval.
//! - Booking a slot moves `tat` to `max(tat, now) + interval`, and the message
//!   is **sent** at its slot: `max(now, tat − (burst − 1)·interval)`, which may
//!   be in the future. A slot later than `now + max_wait` is refused
//!   ([`RateBooked::TooLate`]) and the walk steers (§3.2 step 3c″).
//! - A booked slot that is not used — no headroom, a downstream failure, a
//!   storage error — is given back by [`unbook`], **only if nothing was booked
//!   after it**. Giving back a slot from under a later booking would let the
//!   next message jump the queue; leaving it is the safe direction (the bucket
//!   runs one interval conservative until it drains).
//!
//! No function here reads a clock. `now` is the caller's (D-108), which is what
//! lets the dry run and the tests evaluate "what would happen at T".
//!
//! Every instant is truncated to whole microseconds before it is compared or
//! returned, so that the value the store writes is the value it reads back on
//! either backend (`TIMESTAMPTZ` is microsecond-precise), and [`unbook`]'s
//! equality test means what it says.

use chrono::{DateTime, Duration, SubsecRound, Utc};

use crate::config::Route;
use crate::quota::store::RouteState;

/// One key's rate, resolved for a day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    /// Messages per hour; at least 1 (§4.2).
    pub per_hour: i64,
    /// Messages that may go back to back after an idle spell; at least 1.
    pub burst: i64,
}

impl Rate {
    pub fn new(per_hour: i64, burst: i64) -> Self {
        Self {
            per_hour: per_hour.max(1),
            burst: burst.max(1),
        }
    }

    /// `3600 s / per_hour`, in whole microseconds and never zero.
    pub fn interval(&self) -> Duration {
        Duration::microseconds((3_600_000_000 / self.per_hour.max(1)).max(1))
    }

    /// GCRA's burst tolerance: how far ahead of now `tat` may run while a
    /// message still conforms.
    pub fn tolerance(&self) -> Duration {
        self.interval() * i32::try_from(self.burst.max(1) - 1).unwrap_or(i32::MAX)
    }

    /// The most this rate lets through in one day: `per_hour × 24 + burst`.
    /// §4.2's warning compares it with the day's cap.
    pub fn daily_capacity(&self) -> i64 {
        self.per_hour.saturating_mul(24).saturating_add(self.burst)
    }
}

/// What booking a slot did. The stores return it from one short transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateBooked {
    /// A slot is booked. Send at `send_at`; give it back with `booked_tat` if
    /// the message is not sent.
    Booked {
        send_at: DateTime<Utc>,
        booked_tat: DateTime<Utc>,
        /// D-113: a thread-affinity reply's pinned route had no slot within
        /// `max_wait`, so one was booked anyway and the message goes now —
        /// counted against the bucket, as D-090's `over_cap` is against the cap.
        over_limit: bool,
    },
    /// The earliest slot is later than `now + max_wait`. Nothing was written.
    TooLate { earliest: DateTime<Utc> },
}

/// The route's rate for a domain group on a day index, or `None` when it has no
/// `rate` block or has not started.
///
/// The schedule mirrors §7.2's caps exactly: indexed by the day index, the last
/// value repeats past the end, and a graduated route (§9.3) is at the last value
/// now. A fixed `per_hour` is the same every day.
pub fn rate_for(route: &Route, group: &str, day_index: i64, state: RouteState) -> Option<Rate> {
    let limit = route.rate.as_ref()?;
    if day_index < 0 {
        return None;
    }
    let day = if state.graduated {
        u64::MAX
    } else {
        day_index as u64
    };
    limit
        .per_hour_for(group, day)
        .map(|per_hour| Rate::new(per_hour, limit.burst))
}

fn micros(t: DateTime<Utc>) -> DateTime<Utc> {
    t.trunc_subsecs(6)
}

/// The earliest instant at or after `now` a message conforms. `tat: None` is a
/// key never booked, which is a full bucket.
pub fn earliest_slot(tat: Option<DateTime<Utc>>, rate: Rate, now: DateTime<Utc>) -> DateTime<Utc> {
    let now = micros(now);
    match tat {
        Some(tat) => now.max(micros(tat) - rate.tolerance()),
        None => now,
    }
}

/// Book the next slot unconditionally: `(new_tat, send_at)`.
pub fn book(
    tat: Option<DateTime<Utc>>,
    rate: Rate,
    now: DateTime<Utc>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let now = micros(now);
    let send_at = earliest_slot(tat, rate, now);
    let base = tat.map(micros).map_or(now, |t| t.max(now));
    (base + rate.interval(), send_at)
}

/// The whole decision a store makes under its row lock: book if the slot is
/// within `max_wait`, otherwise refuse — unless `force`, which books anyway and
/// sends now (D-113's pinned reply). Returns the outcome and, when booked, the
/// `tat` to write.
pub fn decide(
    tat: Option<DateTime<Utc>>,
    rate: Rate,
    now: DateTime<Utc>,
    max_wait: Duration,
    force: bool,
) -> RateBooked {
    let now = micros(now);
    let earliest = earliest_slot(tat, rate, now);
    let within = earliest <= now + max_wait.max(Duration::zero());
    if !within && !force {
        return RateBooked::TooLate { earliest };
    }
    let (booked_tat, send_at) = book(tat, rate, now);
    RateBooked::Booked {
        send_at: if within { send_at } else { now },
        booked_tat,
        over_limit: !within,
    }
}

/// Give a booked slot back. Returns the `tat` to write, or `None` to leave the
/// row alone because something was booked after this slot (or the row was
/// reset under it).
pub fn unbook(
    current: Option<DateTime<Utc>>,
    rate: Rate,
    booked_tat: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let booked_tat = micros(booked_tat);
    (current.map(micros) == Some(booked_tat)).then(|| booked_tat - rate.interval())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        "2026-10-03T12:00:00Z".parse().unwrap()
    }

    fn secs(n: i64) -> Duration {
        Duration::seconds(n)
    }

    /// 3600/h is one a second, which keeps the arithmetic legible.
    fn per_second(burst: i64) -> Rate {
        Rate::new(3600, burst)
    }

    /// Book `n` messages at `now` with no waiting allowed; how many got a slot.
    fn burst_at(
        tat: &mut Option<DateTime<Utc>>,
        rate: Rate,
        now: DateTime<Utc>,
        n: usize,
    ) -> usize {
        let mut got = 0;
        for _ in 0..n {
            if let RateBooked::Booked { booked_tat, .. } =
                decide(*tat, rate, now, Duration::zero(), false)
            {
                *tat = Some(booked_tat);
                got += 1;
            }
        }
        got
    }

    #[test]
    fn the_interval_is_an_hour_divided_by_the_rate() {
        assert_eq!(Rate::new(3600, 1).interval(), secs(1));
        assert_eq!(Rate::new(4, 1).interval(), Duration::minutes(15));
        assert_eq!(
            Rate::new(7, 1).interval(),
            Duration::microseconds(514_285_714)
        );
        // Never zero, whatever the rate.
        assert!(Rate::new(i64::MAX, 1).interval() > Duration::zero());
    }

    #[test]
    fn a_fresh_key_takes_exactly_burst_then_refuses() {
        for burst in [1, 2, 5, 20] {
            let mut tat = None;
            assert_eq!(
                burst_at(&mut tat, per_second(burst), t0(), 50),
                burst as usize
            );
        }
    }

    #[test]
    fn a_burst_of_one_is_strict_pacing() {
        let rate = per_second(1);
        let mut tat = None;
        assert_eq!(burst_at(&mut tat, rate, t0(), 3), 1);
        // Not before the interval...
        assert_eq!(
            burst_at(&mut tat, rate, t0() + Duration::milliseconds(999), 3),
            0
        );
        // ...and exactly one at it.
        assert_eq!(burst_at(&mut tat, rate, t0() + secs(1), 3), 1);
    }

    #[test]
    fn idle_time_refills_the_bucket_but_never_past_burst() {
        let rate = per_second(4);
        let mut tat = None;
        assert_eq!(burst_at(&mut tat, rate, t0(), 10), 4);
        // Two seconds idle refill two slots.
        assert_eq!(burst_at(&mut tat, rate, t0() + secs(2), 10), 2);
        // An hour idle refills only to burst.
        assert_eq!(burst_at(&mut tat, rate, t0() + Duration::hours(1), 10), 4);
    }

    #[test]
    fn a_booking_inside_max_wait_is_given_a_future_slot() {
        let rate = per_second(1);
        let (tat, send) = book(None, rate, t0());
        assert_eq!((tat, send), (t0() + secs(1), t0()));

        match decide(Some(tat), rate, t0(), secs(5), false) {
            RateBooked::Booked {
                send_at,
                booked_tat,
                over_limit,
            } => {
                assert_eq!(send_at, t0() + secs(1), "the next slot, not now");
                assert_eq!(booked_tat, t0() + secs(2));
                assert!(!over_limit);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn max_wait_is_inclusive_at_its_boundary() {
        let rate = per_second(1);
        let tat = Some(t0() + secs(3)); // the next slot is t0 + 3s
        assert_eq!(
            decide(tat, rate, t0(), secs(3) - Duration::microseconds(1), false),
            RateBooked::TooLate {
                earliest: t0() + secs(3)
            }
        );
        assert!(matches!(
            decide(tat, rate, t0(), secs(3), false),
            RateBooked::Booked { send_at, .. } if send_at == t0() + secs(3)
        ));
    }

    #[test]
    fn queued_bookings_get_consecutive_slots() {
        let rate = per_second(2);
        let mut tat = None;
        let mut sends = Vec::new();
        for _ in 0..5 {
            match decide(tat, rate, t0(), secs(60), false) {
                RateBooked::Booked {
                    send_at,
                    booked_tat,
                    ..
                } => {
                    tat = Some(booked_tat);
                    sends.push(send_at);
                }
                other => panic!("{other:?}"),
            }
        }
        // Two at once (the burst), then one a second.
        assert_eq!(
            sends,
            vec![t0(), t0(), t0() + secs(1), t0() + secs(2), t0() + secs(3)]
        );
    }

    #[test]
    fn too_late_names_the_earliest_slot_and_books_nothing() {
        let rate = per_second(1);
        let tat = Some(t0() + secs(10));
        assert_eq!(
            decide(tat, rate, t0(), Duration::zero(), false),
            RateBooked::TooLate {
                earliest: t0() + secs(10)
            }
        );
    }

    #[test]
    fn a_forced_booking_is_counted_and_sent_now() {
        // D-111's pinned reply: past the limit, it still moves the bucket.
        let rate = per_second(1);
        let tat = Some(t0() + secs(10));
        assert_eq!(
            decide(tat, rate, t0(), Duration::zero(), true),
            RateBooked::Booked {
                send_at: t0(),
                booked_tat: t0() + secs(11),
                over_limit: true,
            }
        );
        // Inside the limit, force changes nothing.
        assert_eq!(
            decide(None, rate, t0(), Duration::zero(), true),
            decide(None, rate, t0(), Duration::zero(), false)
        );
    }

    #[test]
    fn unbooking_the_last_slot_gives_it_back() {
        let rate = per_second(1);
        let (tat, _) = book(None, rate, t0());
        let back = unbook(Some(tat), rate, tat).expect("last booking");
        assert_eq!(back, t0());
        // ...so the next message at t0 conforms again.
        assert_eq!(earliest_slot(Some(back), rate, t0()), t0());
    }

    #[test]
    fn unbooking_is_refused_when_a_later_slot_was_booked() {
        let rate = per_second(1);
        let (first, _) = book(None, rate, t0());
        let (second, _) = book(Some(first), rate, t0());
        assert_eq!(unbook(Some(second), rate, first), None, "not the last");
        // The later one can be given back, and then the earlier one can.
        let after = unbook(Some(second), rate, second).unwrap();
        assert_eq!(after, first);
        assert_eq!(unbook(Some(after), rate, first), Some(t0()));
    }

    #[test]
    fn unbooking_a_missing_row_does_nothing() {
        assert_eq!(unbook(None, per_second(1), t0()), None);
    }

    #[test]
    fn sub_microsecond_instants_compare_as_stored() {
        // What the store writes is what it reads back; equality must survive.
        let rate = Rate::new(7, 1);
        let now = t0() + Duration::nanoseconds(123_456_789);
        let (tat, send) = book(None, rate, now);
        assert_eq!(tat, tat.trunc_subsecs(6));
        assert_eq!(send, send.trunc_subsecs(6));
        assert!(unbook(Some(tat), rate, tat).is_some());
    }

    #[test]
    fn daily_capacity_is_a_day_of_intervals_plus_the_burst() {
        assert_eq!(Rate::new(10, 5).daily_capacity(), 245);
    }
}
