//! §7.2 — the warm-up day index.
//!
//! ```text
//! day_index = floor((now - warmup.started) / 24h)
//! ```
//!
//! An **elapsed-duration** calculation from a configured instant, not calendar
//! arithmetic. §7.2 is explicit about why: a route started at 14:00 has its
//! boundary at 14:00 every day, is immune to DST transitions, and cannot produce
//! a 23- or 25-hour window. The process timezone governs only how dates are
//! rendered in logs and the admin API.
//!
//! An overflow route has no `warmup` block and therefore no start instant. It is
//! given a synthetic start of the Unix epoch (D-024), so one formula covers every
//! route and overflow traffic buckets on UTC midnight. The two clocks differ —
//! warming routes bucket on their own anniversary — but they are never compared,
//! only used to key rows.

use chrono::{DateTime, TimeZone, Utc};

use crate::config::Route;

const MILLIS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

/// The start instant a route's day index is measured from.
///
/// `warmup.started` for a warming route, the Unix epoch for an overflow route.
pub fn origin(route: &Route) -> DateTime<Utc> {
    route
        .warmup
        .as_ref()
        .map(|w| w.started)
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().expect("epoch is valid"))
}

/// `floor((now - started) / 24h)`.
///
/// Signed, and negative before `started`: §7.2 says a `warmup.started` in the
/// future "makes the route ineligible until it arrives", and a negative index is
/// how that is represented rather than by a separate flag.
///
/// Computed in milliseconds rather than whole seconds so that a start instant a
/// fraction of a second in the future still floors to `-1`. Truncating to
/// seconds first would round it up to `0` and make the route eligible early.
pub fn index(started: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    let elapsed = now.timestamp_millis() - started.timestamp_millis();
    elapsed.div_euclid(MILLIS_PER_DAY)
}

/// The day index for a route at an instant.
pub fn for_route(route: &Route, now: DateTime<Utc>) -> i64 {
    index(origin(route), now)
}

/// The instant a day index begins — equivalently, the instant the previous one
/// ends.
///
/// §9.3's allowance override "expires at the next day boundary", and this is
/// what computes the boundary for an operator to be told about. The override
/// itself needs no expiry job: it is a column on the row for one day index, and
/// tomorrow is a different row.
pub fn boundary(started: DateTime<Utc>, day_index: i64) -> DateTime<Utc> {
    started + chrono::Duration::milliseconds(day_index.saturating_mul(MILLIS_PER_DAY))
}

/// When the current day ends, for a route.
pub fn next_boundary(route: &Route, now: DateTime<Utc>) -> DateTime<Utc> {
    boundary(origin(route), for_route(route, now) + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid RFC 3339")
            .with_timezone(&Utc)
    }

    #[test]
    fn the_first_day_is_zero() {
        let started = utc("2026-08-01T09:00:00Z");
        assert_eq!(index(started, started), 0);
        assert_eq!(index(started, utc("2026-08-01T09:00:01Z")), 0);
        assert_eq!(index(started, utc("2026-08-02T08:59:59Z")), 0);
    }

    #[test]
    fn the_boundary_is_the_anniversary_of_the_start_time() {
        // Not midnight. A route started at 09:00 rolls over at 09:00.
        let started = utc("2026-08-01T09:00:00Z");
        assert_eq!(index(started, utc("2026-08-02T08:59:59Z")), 0);
        assert_eq!(index(started, utc("2026-08-02T09:00:00Z")), 1);
        assert_eq!(index(started, utc("2026-08-03T09:00:00Z")), 2);
        assert_eq!(index(started, utc("2026-08-31T09:00:00Z")), 30);
    }

    #[test]
    fn a_future_start_gives_a_negative_index() {
        // §7.2: "warmup.started in the future makes the route ineligible until
        // it arrives." The chain walk reads that off the sign.
        let started = utc("2026-08-10T09:00:00Z");
        assert_eq!(index(started, utc("2026-08-09T09:00:00Z")), -1);
        assert_eq!(index(started, utc("2026-08-01T09:00:00Z")), -9);
    }

    #[test]
    fn an_instant_barely_before_the_start_still_floors_to_minus_one() {
        // The reason this is computed in milliseconds. Truncating the elapsed
        // duration to whole seconds first would yield 0 here, and a route would
        // become eligible a fraction of a second early — a small window, but the
        // kind that turns up at exactly the wrong moment.
        let started = utc("2026-08-01T09:00:00Z");
        let just_before = started - chrono::Duration::milliseconds(1);
        assert_eq!(index(started, just_before), -1);
    }

    // -- the §12.3 / O-12 cases ------------------------------------------

    #[test]
    fn a_spring_forward_dst_transition_does_not_shorten_a_day() {
        // Europe/London went to BST at 01:00 UTC on 2026-03-29, so local clocks
        // jumped 01:00 -> 02:00. Calendar arithmetic on local time would make
        // this a 23-hour day and advance the index early. Elapsed duration does
        // not care.
        let started = utc("2026-03-28T12:00:00Z");
        assert_eq!(index(started, utc("2026-03-29T11:59:59Z")), 0);
        assert_eq!(index(started, utc("2026-03-29T12:00:00Z")), 1);

        // And a full 24 hours really did elapse across the transition.
        let across = utc("2026-03-29T12:00:00Z") - utc("2026-03-28T12:00:00Z");
        assert_eq!(across.num_hours(), 24);
    }

    #[test]
    fn an_autumn_back_dst_transition_does_not_lengthen_a_day() {
        // Europe/London returned to GMT at 02:00 local on 2026-10-25.
        let started = utc("2026-10-24T12:00:00Z");
        assert_eq!(index(started, utc("2026-10-25T11:59:59Z")), 0);
        assert_eq!(index(started, utc("2026-10-25T12:00:00Z")), 1);
    }

    #[test]
    fn a_start_inside_a_dst_gap_is_still_a_well_defined_instant() {
        // 01:30 local on a spring-forward date does not exist in London, but
        // `warmup.started` is an RFC 3339 *instant*, so there is no gap to fall
        // into. This is the payoff of §7.2's choice.
        let started = utc("2026-03-29T01:30:00Z");
        assert_eq!(index(started, utc("2026-03-30T01:29:59Z")), 0);
        assert_eq!(index(started, utc("2026-03-30T01:30:00Z")), 1);
    }

    #[test]
    fn the_leap_second_case_is_a_no_op() {
        // O-12: §12.3 asks for tests "across DST boundaries and leap seconds".
        // Unix time cannot represent a leap second — 23:59:60 is not a distinct
        // timestamp — so an elapsed-millisecond calculation cannot observe one.
        // Asserted rather than merely argued: the day either side of the most
        // recent insertion (2016-12-31) advances exactly once.
        let started = utc("2016-12-31T00:00:00Z");
        assert_eq!(index(started, utc("2016-12-31T23:59:59Z")), 0);
        assert_eq!(index(started, utc("2017-01-01T00:00:00Z")), 1);
        assert_eq!(index(started, utc("2017-01-02T00:00:00Z")), 2);
    }

    #[test]
    fn indices_advance_monotonically_over_a_long_ramp() {
        let started = utc("2026-01-01T00:00:00Z");
        let mut previous = i64::MIN;
        for day in 0..400 {
            let now = started + chrono::Duration::days(day);
            let idx = index(started, now);
            assert_eq!(idx, day, "day {day}");
            assert!(idx > previous);
            previous = idx;
        }
    }

    // -- boundaries -------------------------------------------------------

    #[test]
    fn boundary_is_the_inverse_of_index() {
        let started = utc("2026-08-01T09:00:00Z");
        for day in -3..30 {
            let b = boundary(started, day);
            assert_eq!(index(started, b), day, "start of day {day}");
            assert_eq!(
                index(started, b - chrono::Duration::milliseconds(1)),
                day - 1,
                "just before day {day}"
            );
        }
    }

    // -- routes -----------------------------------------------------------

    fn config() -> crate::config::Config {
        let yaml = r#"
server:
  listeners:
    - address: "127.0.0.1:25"
  hostname: simmer.test
  max_message_bytes: 1024
  max_recipients: 10
  max_concurrent_sessions: 4
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth: { allow_insecure_auth: true }
database:
  url: "postgres://u:p@localhost/simmer"
  connect_timeout: 5s
admin: { listen: "127.0.0.1:8080", auth_token: "t" }
domain_groups:
  - { name: catchall, domains: ["*"] }
senders:
  - { match: "oldbrand.com", match_on: envelope, chain: [warming, overflow] }
default_chain: [overflow]
routes:
  - name: warming
    downstream:
      host: w.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@newbrand.com" }
    warmup:
      started: "2026-08-01T09:00:00Z"
      schedule: { default: [50, 100, 200] }
  - name: overflow
    overflow: true
    downstream:
      host: o.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@established.com" }
"#;
        crate::config::from_str(yaml, "test").expect("fixture is valid")
    }

    #[test]
    fn a_warming_route_measures_from_its_configured_start() {
        let cfg = config();
        let route = cfg.route("warming").expect("route");
        assert_eq!(origin(route), utc("2026-08-01T09:00:00Z"));
        assert_eq!(for_route(route, utc("2026-08-03T10:00:00Z")), 2);
    }

    #[test]
    fn an_overflow_route_measures_from_the_epoch_and_buckets_on_utc_midnight() {
        // D-024: an overflow route has no `warmup`, so it has no boundary of its
        // own. The epoch gives it one formula and a UTC-midnight bucket, which
        // is what makes "how much spilled to overflow today" answerable.
        let cfg = config();
        let route = cfg.route("overflow").expect("route");
        assert_eq!(origin(route), Utc.timestamp_opt(0, 0).unwrap());

        let a = for_route(route, utc("2026-08-03T00:00:00Z"));
        let b = for_route(route, utc("2026-08-03T23:59:59Z"));
        let c = for_route(route, utc("2026-08-04T00:00:00Z"));
        assert_eq!(a, b, "the whole UTC day is one bucket");
        assert_eq!(c, a + 1, "and midnight starts the next");
    }

    #[test]
    fn an_overflow_routes_next_boundary_is_the_next_utc_midnight() {
        let cfg = config();
        let route = cfg.route("overflow").expect("route");
        assert_eq!(
            next_boundary(route, utc("2026-08-03T14:23:11Z")),
            utc("2026-08-04T00:00:00Z")
        );
    }

    #[test]
    fn a_warming_routes_next_boundary_is_its_own_anniversary() {
        let cfg = config();
        let route = cfg.route("warming").expect("route");
        assert_eq!(
            next_boundary(route, utc("2026-08-03T14:23:11Z")),
            utc("2026-08-04T09:00:00Z")
        );
    }
}
