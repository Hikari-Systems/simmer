//! §7 — the quota model: day-index arithmetic, allowance resolution, and the
//! §7.4 reserve/send/commit protocol.

pub mod day;
pub mod postgres;
pub mod registry;
pub mod store;
pub mod sweeper;

use std::time::Duration;

use crate::config::Route;
use crate::quota::store::RouteState;

pub use postgres::PgQuotaStore;
pub use registry::ReservationRegistry;
pub use store::{
    QuotaError, QuotaStore, Reservation, ReserveRequest, Reserved, Reset, Usage, UsageKey,
};

/// §7.4: "Reservations carry an expiry (default: downstream timeout budget +
/// 60s)."
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// Today's ceiling for a route and domain group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allowance {
    /// An overflow route: §3.1 says it is "never quota-limited". It still
    /// accounts, so that "how much is spilling to overflow" is answerable
    /// (D-024).
    Unlimited,
    Limited(i64),
    /// §7.2: "`warmup.started` in the future makes the route ineligible until it
    /// arrives."
    NotStarted,
}

impl Allowance {
    /// What to write into a freshly created `quota_usage` row. `None` is
    /// unlimited.
    pub fn as_column(self) -> Option<i64> {
        match self {
            Allowance::Unlimited | Allowance::NotStarted => None,
            Allowance::Limited(n) => Some(n),
        }
    }
}

/// Resolve §7.2's schedule for a route, day index and domain group.
///
/// `graduated` is §9.3's manual pin to the final value. §7.2 is explicit that
/// routes never do this on their own: "Routes do not auto-graduate to uncapped;
/// a warm route is made uncapped by editing the configuration and restarting, or
/// by removing Simmer entirely."
pub fn allowance_for(
    route: &Route,
    domain_group: &str,
    day_index: i64,
    state: RouteState,
) -> Allowance {
    let Some(warmup) = route.warmup.as_ref() else {
        // No warm-up block means an overflow route — §4.2 rejects any other
        // combination at startup.
        return Allowance::Unlimited;
    };

    if day_index < 0 {
        return Allowance::NotStarted;
    }

    // §7.2: past the end of the array, "the final value repeats indefinitely".
    // Graduating just means jumping there now. `usize::MAX` is a safe stand-in
    // for "beyond the end" because `allowance_for` clamps to the last element.
    let effective_day = if state.graduated {
        u64::MAX
    } else {
        day_index as u64
    };

    warmup
        .schedule
        .allowance_for(domain_group, effective_day)
        .map(|n| Allowance::Limited(n as i64))
        // §4.2 rejects an empty schedule at startup, so this is unreachable;
        // treating it as a zero ceiling rather than unlimited keeps the failure
        // safe if that ever stops being true.
        .unwrap_or(Allowance::Limited(0))
}

/// §7.4's reservation expiry: the route's whole downstream timeout budget, plus
/// a margin.
///
/// Deliberately generous. Expiring early means the sweeper releases headroom for
/// a send that is still in flight, and the commit then lands against a row that
/// has already been given back — recoverable (see `PgQuotaStore::commit`) but
/// noisy. Expiring late only delays the release after a crash.
pub fn reservation_expiry(route: &Route, recipients: usize) -> Duration {
    let t = route.downstream.timeouts.as_ref();
    let connect = t.and_then(|t| t.connect).unwrap_or(Duration::from_secs(10));
    let command = t.and_then(|t| t.command).unwrap_or(Duration::from_secs(30));
    let data = t.and_then(|t| t.data).unwrap_or(Duration::from_secs(120));

    // greeting + EHLO + STARTTLS + EHLO + AUTH + MAIL + one per RCPT + DATA.
    let commands = 7 + recipients.max(1) as u32;
    connect + command * commands + data + EXPIRY_MARGIN
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const CFG: &str = r#"
server:
  listen: "127.0.0.1:25"
  hostname: simmer.test
  max_message_bytes: 1024
  max_recipients: 10
  max_concurrent_sessions: 4
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth: { required: false, allow_insecure_auth: true }
database: { url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }
admin: { listen: "127.0.0.1:8080", auth_token: "t" }
domain_groups:
  - { name: google, domains: ["gmail.com"] }
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
      timeouts: { connect: 10s, command: 30s, data: 120s }
    identity: { envelope_from: "b@newbrand.com" }
    warmup:
      started: "2026-08-01T09:00:00Z"
      schedule:
        default: [50, 100, 200]
        overrides:
          google: [20, 40]
  - name: overflow
    overflow: true
    downstream:
      host: o.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@established.com" }
"#;

    fn config() -> Config {
        crate::config::from_str(CFG, "test").expect("fixture is valid")
    }

    fn plain() -> RouteState {
        RouteState::default()
    }

    #[test]
    fn walks_the_default_schedule() {
        let cfg = config();
        let r = cfg.route("warming").unwrap();
        assert_eq!(
            allowance_for(r, "catchall", 0, plain()),
            Allowance::Limited(50)
        );
        assert_eq!(
            allowance_for(r, "catchall", 1, plain()),
            Allowance::Limited(100)
        );
        assert_eq!(
            allowance_for(r, "catchall", 2, plain()),
            Allowance::Limited(200)
        );
    }

    #[test]
    fn a_domain_group_override_replaces_the_default_series() {
        let cfg = config();
        let r = cfg.route("warming").unwrap();
        assert_eq!(
            allowance_for(r, "google", 0, plain()),
            Allowance::Limited(20)
        );
        assert_eq!(
            allowance_for(r, "google", 1, plain()),
            Allowance::Limited(40)
        );
    }

    #[test]
    fn the_final_value_repeats_rather_than_uncapping() {
        // §7.2: "When day_index exceeds the array bounds, the final value
        // repeats indefinitely. Routes do not auto-graduate to uncapped."
        let cfg = config();
        let r = cfg.route("warming").unwrap();
        assert_eq!(
            allowance_for(r, "catchall", 3, plain()),
            Allowance::Limited(200)
        );
        assert_eq!(
            allowance_for(r, "catchall", 500, plain()),
            Allowance::Limited(200)
        );
        assert_eq!(
            allowance_for(r, "catchall", i64::MAX, plain()),
            Allowance::Limited(200)
        );
        // ...and the override series repeats its own final value, not the
        // default's.
        assert_eq!(
            allowance_for(r, "google", 99, plain()),
            Allowance::Limited(40)
        );
    }

    #[test]
    fn a_future_start_is_not_started_rather_than_a_zero_ceiling() {
        // The distinction matters for the §9.1 skip reason: "not started" and
        // "out of quota" are different operational conditions.
        let cfg = config();
        let r = cfg.route("warming").unwrap();
        assert_eq!(
            allowance_for(r, "catchall", -1, plain()),
            Allowance::NotStarted
        );
        assert_eq!(
            allowance_for(r, "catchall", -9, plain()),
            Allowance::NotStarted
        );
    }

    #[test]
    fn an_overflow_route_is_unlimited_at_every_day_index() {
        let cfg = config();
        let r = cfg.route("overflow").unwrap();
        for day in [-5, 0, 1, 10_000] {
            assert_eq!(
                allowance_for(r, "catchall", day, plain()),
                Allowance::Unlimited
            );
        }
    }

    #[test]
    fn graduating_pins_a_route_to_its_final_value_immediately() {
        let cfg = config();
        let r = cfg.route("warming").unwrap();
        let graduated = RouteState {
            paused: false,
            graduated: true,
        };
        assert_eq!(
            allowance_for(r, "catchall", 0, graduated),
            Allowance::Limited(200)
        );
        assert_eq!(
            allowance_for(r, "google", 0, graduated),
            Allowance::Limited(40)
        );
    }

    #[test]
    fn graduating_does_not_start_a_route_that_has_not_begun() {
        // Graduation changes the ceiling, not the clock. A route whose start is
        // still in the future has no business sending.
        let cfg = config();
        let r = cfg.route("warming").unwrap();
        let graduated = RouteState {
            paused: false,
            graduated: true,
        };
        assert_eq!(
            allowance_for(r, "catchall", -1, graduated),
            Allowance::NotStarted
        );
    }

    #[test]
    fn an_unlimited_allowance_writes_a_null_column() {
        assert_eq!(Allowance::Unlimited.as_column(), None);
        assert_eq!(Allowance::NotStarted.as_column(), None);
        assert_eq!(Allowance::Limited(50).as_column(), Some(50));
    }

    #[test]
    fn the_reservation_expiry_covers_the_whole_downstream_budget() {
        let cfg = config();
        let r = cfg.route("warming").unwrap();
        // 10 + 30*8 + 120 + 60 = 430s for one recipient.
        assert_eq!(reservation_expiry(r, 1), Duration::from_secs(430));
        // ...and grows with the recipient count, because each RCPT TO is
        // another command round trip.
        assert!(reservation_expiry(r, 10) > reservation_expiry(r, 1));
    }

    #[test]
    fn the_reservation_expiry_exceeds_the_time_a_send_can_actually_take() {
        // The property that matters: the sweeper must never release a
        // reservation for a send that is still legitimately in flight.
        let cfg = config();
        let r = cfg.route("warming").unwrap();
        for recipients in [1, 5, 100] {
            let budget = Duration::from_secs(10)
                + Duration::from_secs(30) * (7 + recipients)
                + Duration::from_secs(120);
            assert!(
                reservation_expiry(r, recipients as usize) > budget,
                "{recipients} recipients"
            );
        }
    }

    // -- headroom --------------------------------------------------------

    #[test]
    fn headroom_accounts_for_both_committed_and_reserved() {
        // The §7.4 race in one assertion: a post-hoc increment would let two
        // sessions both see the last slot, because neither would count the
        // other's reservation.
        let u = Usage {
            allowance: Some(10),
            allowance_override: None,
            committed: 7,
            reserved: 2,
        };
        assert_eq!(u.headroom(), Some(1));
        assert!(u.has_headroom_for(1));
        assert!(!u.has_headroom_for(2));
    }

    #[test]
    fn an_unlimited_row_always_has_headroom() {
        let u = Usage {
            allowance: None,
            allowance_override: None,
            committed: 1_000_000,
            reserved: 500,
        };
        assert_eq!(u.headroom(), None);
        assert!(u.has_headroom_for(i64::MAX));
    }

    #[test]
    fn an_override_wins_over_the_scheduled_allowance() {
        let u = Usage {
            allowance: Some(10),
            allowance_override: Some(100),
            committed: 50,
            reserved: 0,
        };
        assert_eq!(u.effective_allowance(), Some(100));
        assert_eq!(u.headroom(), Some(50));
    }

    #[test]
    fn an_override_can_lower_the_ceiling_below_what_is_already_committed() {
        // An operator pulling the ceiling down mid-day must not produce negative
        // headroom, which would underflow into "unlimited" on a naive compare.
        let u = Usage {
            allowance: Some(1000),
            allowance_override: Some(10),
            committed: 50,
            reserved: 0,
        };
        assert_eq!(u.headroom(), Some(0));
        assert!(!u.has_headroom_for(1));
    }
}
