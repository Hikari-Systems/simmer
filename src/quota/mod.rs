//! §7 — the quota model: day-index arithmetic, allowance resolution, and the
//! §7.4 reserve/send/commit protocol.

pub mod advance;
pub mod day;
#[cfg(feature = "mssql")]
pub mod mssql;
#[cfg(feature = "postgres")]
pub mod postgres;
pub mod registry;
pub mod store;
pub mod sweeper;

use std::time::Duration;

use crate::config::{Downstream, Route};
use crate::quota::store::RouteState;

#[cfg(feature = "mssql")]
pub use mssql::MssqlQuotaStore;
#[cfg(feature = "postgres")]
pub use postgres::PgQuotaStore;
pub use registry::ReservationRegistry;
pub use store::{
    Adoption, AdvanceRows, GroupHold, GroupReserve, QuotaError, QuotaStore, Reservation,
    ReserveRequest, Reserved, Reset, Usage, UsageKey,
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
///
/// `step` is D-115's schedule position — [`position`]'s answer, which is the day
/// index itself under `advance: auto`. `day_index` still decides whether the
/// route has started.
pub fn allowance_for(
    route: &Route,
    domain_group: &str,
    day_index: i64,
    step: i64,
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
        step.clamp(0, day_index) as u64
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

/// D-115 — where one `(ramp, route, domain_group)` element is today: its
/// schedule step, the allowance that step plans, and what the step was decided
/// from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub day_index: i64,
    pub step: i64,
    pub allowance: Allowance,
    pub decision: advance::Decision,
    pub rule: crate::config::Advance,
}

impl Position {
    pub fn held_days(&self) -> i64 {
        self.decision.held_days(self.day_index)
    }

    /// The step to write into a row this position creates. `None` for an
    /// overflow route or before the start, whose rows record no step.
    pub fn step_column(&self) -> Option<i64> {
        matches!(self.allowance, Allowance::Limited(_)).then_some(self.step)
    }
}

/// D-115's position from rows already read — the pure half of [`position`],
/// for callers that read many elements in one query.
pub fn position_from(
    route: &Route,
    domain_group: &str,
    day_index: i64,
    state: RouteState,
    rows: AdvanceRows,
) -> Position {
    let rule = route
        .warmup
        .as_ref()
        .map_or(crate::config::Advance::Auto, |w| w.advance());
    let decision = if route.warmup.is_none() {
        // An overflow route has no schedule to walk; its "step" is the day.
        advance::decide(crate::config::Advance::Auto, day_index, None, None)
    } else {
        advance::decide(rule, day_index, rows.today, rows.previous)
    };
    Position {
        day_index,
        step: decision.step,
        allowance: allowance_for(route, domain_group, day_index, decision.step, state),
        decision,
        rule,
    }
}

/// Does [`position`] need to read anything for this route? Only a warming,
/// started route under a threshold rule: under `auto` the step is the day
/// index, and an overflow route has no schedule.
pub fn position_reads_rows(route: &Route, day_index: i64) -> bool {
    day_index >= 0
        && route
            .warmup
            .as_ref()
            .is_some_and(|w| matches!(w.advance(), crate::config::Advance::Threshold { .. }))
}

/// D-115 — the ONE entry point for "where is this element today". The walk
/// (every `Mode`), the dry run, the §5.4 early check, `/routes`, the allowance
/// mutation and the gauges all call this, so none of them can disagree about
/// which schedule entry is in force.
///
/// One unlocked read, and only for a threshold-mode warming route. The answer
/// is advisory until a reservation writes it: the row's `step` and `allowance`
/// are authoritative once the row exists (D-026), and every instance computes
/// the same value from the same earlier row.
pub async fn position(
    store: &dyn QuotaStore,
    route: &Route,
    domain_group: &str,
    day_index: i64,
    state: RouteState,
) -> Result<Position, QuotaError> {
    let rows = if position_reads_rows(route, day_index) {
        store
            .advance_rows(&route.ramp, &route.name, domain_group, day_index)
            .await?
    } else {
        AdvanceRows::default()
    };
    Ok(position_from(route, domain_group, day_index, state, rows))
}

/// D-115 — a reservation just created the day's row for this element, and with
/// it fixed the element's step for the day. Count it, and say why, once: the
/// row lock means exactly one reservation per row ever reports `created_row`,
/// whichever instance took it.
///
/// No recipient anywhere in it: it is about a schedule, not a message.
pub fn report_row_created(route: &Route, domain_group: &str, p: &Position) {
    let outcome = match p.decision.outcome {
        // A row created now cannot have existed when `position` read; if one
        // did, a racing instance created and its own report covers it. Count
        // what the step amounts to.
        advance::Outcome::Existing | advance::Outcome::NotStarted => return,
        o => o,
    };
    if route.warmup.is_none() {
        return;
    }
    crate::metrics::warmup_advance(&route.ramp, &route.name, domain_group, outcome.as_str());

    let basis = p.decision.basis;
    tracing::info!(
        event = "simmer.warmup.advance",
        ramp = %route.ramp,
        route = %route.name,
        domain_group,
        day_index = p.day_index,
        step = p.step,
        held_days = p.held_days(),
        advance_mode = p.rule.mode(),
        threshold = p.rule.threshold(),
        outcome = outcome.as_str(),
        allowance = p.allowance.as_column(),
        previous_day_index = basis.map(|b| b.previous.day_index),
        previous_step = basis.map(|b| b.previous.step_or_day()),
        previous_committed = basis.map(|b| b.previous.committed),
        previous_allowance = basis.and_then(|b| b.previous.allowance),
        required = basis.map(|b| b.required),
        "warm-up schedule step decided for the day (D-115)"
    );
}

/// D-116 — where a ramp group is today: its own day index (from the group's
/// `started`, by §7.2's arithmetic), its step under D-115's rule applied to its
/// own rows, and the ceiling that step plans.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupPosition {
    pub day_index: i64,
    pub step: i64,
    /// `None` before the group's `started`: the group is not yet in force and
    /// caps nothing, and no group row is written (D-116).
    pub allowance: Option<i64>,
    pub decision: advance::Decision,
    pub rule: crate::config::Advance,
}

impl GroupPosition {
    pub fn held_days(&self) -> i64 {
        self.decision.held_days(self.day_index)
    }

    /// What a reservation passes to have the group row created and counted.
    /// `None` when the group is not in force.
    pub fn reserve(&self, group: &crate::config::RampGroup) -> Option<GroupReserve> {
        self.allowance.map(|allowance| GroupReserve {
            name: group.name.clone(),
            day_index: self.day_index,
            allowance,
            step: self.step,
        })
    }
}

/// The pure half of [`group_position`], from rows already read.
pub fn group_position_from(
    group: &crate::config::RampGroup,
    day_index: i64,
    rows: AdvanceRows,
) -> GroupPosition {
    let rule = group.advance();
    let decision = advance::decide(rule, day_index, rows.today, rows.previous);
    // Today's row is authoritative once it exists (D-026), so an edited
    // schedule does not move a day already being spent.
    let allowance = if day_index < 0 {
        None
    } else {
        rows.today
            .and_then(|t| t.allowance)
            .or_else(|| group.allowance_at(decision.step))
    };
    GroupPosition {
        day_index,
        step: decision.step,
        allowance,
        decision,
        rule,
    }
}

/// D-116 — the ONE entry point for "where is this ramp group today". The walk
/// (once per message, not per route), the dry run, the early check, the read
/// API and the gauges all call it.
///
/// One unlocked read, of the group's two most recent rows: under `auto` the
/// step needs none, but the allowance of a row that already exists wins over
/// the schedule's, and the read API wants the row anyway.
pub async fn group_position(
    store: &dyn QuotaStore,
    group: &crate::config::RampGroup,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<GroupPosition, QuotaError> {
    let day_index = day::index(group.started, now);
    let rows = if day_index >= 0 {
        store.group_advance_rows(&group.name, day_index).await?
    } else {
        AdvanceRows::default()
    };
    Ok(group_position_from(group, day_index, rows))
}

/// D-116 — a reservation created the group's row for the day, fixing its step.
/// [`report_row_created`] for the group: counted once across every instance.
pub fn report_group_row_created(group: &crate::config::RampGroup, p: &GroupPosition) {
    let outcome = match p.decision.outcome {
        advance::Outcome::Existing | advance::Outcome::NotStarted => return,
        o => o,
    };
    crate::metrics::ramp_group_advance(&group.name, outcome.as_str());

    let basis = p.decision.basis;
    tracing::info!(
        event = "simmer.ramp_group.advance",
        ramp_group = %group.name,
        day_index = p.day_index,
        step = p.step,
        held_days = p.held_days(),
        advance_mode = p.rule.mode(),
        threshold = p.rule.threshold(),
        outcome = outcome.as_str(),
        allowance = p.allowance,
        previous_day_index = basis.map(|b| b.previous.day_index),
        previous_step = basis.map(|b| b.previous.step_or_day()),
        previous_committed = basis.map(|b| b.previous.committed),
        previous_allowance = basis.and_then(|b| b.previous.allowance),
        required = basis.map(|b| b.required),
        "ramp group schedule step decided for the day (D-116)"
    );
}

/// §7.4's reservation expiry: the whole timeout budget of the downstream the
/// message will meet — the route's own or its group's override (D-112) — plus a
/// margin.
///
/// Deliberately generous. Expiring early means the sweeper releases headroom for
/// a send that is still in flight, and the commit then lands against a row that
/// has already been given back — recoverable (see `PgQuotaStore::commit`) but
/// noisy. Expiring late only delays the release after a crash.
pub fn reservation_expiry(downstream: &Downstream, recipients: usize) -> Duration {
    let t = downstream.timeouts.as_ref();
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
  listeners:
    - address: "127.0.0.1:25"
  hostname: simmer.test
  max_message_bytes: 1024
  max_recipients: 10
  max_concurrent_sessions: 4
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth: { allow_insecure_auth: true }
database: { url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }
admin: { listen: "127.0.0.1:8080", auth_token: "t" }
default_ramp: main
ramps:
 main:
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
        let r = cfg.default_ramp().route("warming").unwrap();
        assert_eq!(
            allowance_for(r, "catchall", 0, 0, plain()),
            Allowance::Limited(50)
        );
        assert_eq!(
            allowance_for(r, "catchall", 1, 1, plain()),
            Allowance::Limited(100)
        );
        assert_eq!(
            allowance_for(r, "catchall", 2, 2, plain()),
            Allowance::Limited(200)
        );
    }

    #[test]
    fn a_domain_group_override_replaces_the_default_series() {
        let cfg = config();
        let r = cfg.default_ramp().route("warming").unwrap();
        assert_eq!(
            allowance_for(r, "google", 0, 0, plain()),
            Allowance::Limited(20)
        );
        assert_eq!(
            allowance_for(r, "google", 1, 1, plain()),
            Allowance::Limited(40)
        );
    }

    #[test]
    fn the_final_value_repeats_rather_than_uncapping() {
        // §7.2: "When day_index exceeds the array bounds, the final value
        // repeats indefinitely. Routes do not auto-graduate to uncapped."
        let cfg = config();
        let r = cfg.default_ramp().route("warming").unwrap();
        assert_eq!(
            allowance_for(r, "catchall", 3, 3, plain()),
            Allowance::Limited(200)
        );
        assert_eq!(
            allowance_for(r, "catchall", 500, 500, plain()),
            Allowance::Limited(200)
        );
        assert_eq!(
            allowance_for(r, "catchall", i64::MAX, i64::MAX, plain()),
            Allowance::Limited(200)
        );
        // ...and the override series repeats its own final value, not the
        // default's.
        assert_eq!(
            allowance_for(r, "google", 99, 99, plain()),
            Allowance::Limited(40)
        );
    }

    #[test]
    fn a_future_start_is_not_started_rather_than_a_zero_ceiling() {
        // The distinction matters for the §9.1 skip reason: "not started" and
        // "out of quota" are different operational conditions.
        let cfg = config();
        let r = cfg.default_ramp().route("warming").unwrap();
        assert_eq!(
            allowance_for(r, "catchall", -1, -1, plain()),
            Allowance::NotStarted
        );
        assert_eq!(
            allowance_for(r, "catchall", -9, -9, plain()),
            Allowance::NotStarted
        );
    }

    #[test]
    fn an_overflow_route_is_unlimited_at_every_day_index() {
        let cfg = config();
        let r = cfg.default_ramp().route("overflow").unwrap();
        for day in [-5, 0, 1, 10_000] {
            assert_eq!(
                allowance_for(r, "catchall", day, day, plain()),
                Allowance::Unlimited
            );
        }
    }

    #[test]
    fn graduating_pins_a_route_to_its_final_value_immediately() {
        let cfg = config();
        let r = cfg.default_ramp().route("warming").unwrap();
        let graduated = RouteState {
            paused: false,
            graduated: true,
        };
        assert_eq!(
            allowance_for(r, "catchall", 0, 0, graduated),
            Allowance::Limited(200)
        );
        assert_eq!(
            allowance_for(r, "google", 0, 0, graduated),
            Allowance::Limited(40)
        );
    }

    #[test]
    fn graduating_does_not_start_a_route_that_has_not_begun() {
        // Graduation changes the ceiling, not the clock. A route whose start is
        // still in the future has no business sending.
        let cfg = config();
        let r = cfg.default_ramp().route("warming").unwrap();
        let graduated = RouteState {
            paused: false,
            graduated: true,
        };
        assert_eq!(
            allowance_for(r, "catchall", -1, -1, graduated),
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
        let r = cfg.default_ramp().route("warming").unwrap();
        // 10 + 30*8 + 120 + 60 = 430s for one recipient.
        assert_eq!(
            reservation_expiry(&r.downstream, 1),
            Duration::from_secs(430)
        );
        // ...and grows with the recipient count, because each RCPT TO is
        // another command round trip.
        assert!(reservation_expiry(&r.downstream, 10) > reservation_expiry(&r.downstream, 1));
    }

    #[test]
    fn the_reservation_expiry_exceeds_the_time_a_send_can_actually_take() {
        // The property that matters: the sweeper must never release a
        // reservation for a send that is still legitimately in flight.
        let cfg = config();
        let r = cfg.default_ramp().route("warming").unwrap();
        for recipients in [1, 5, 100] {
            let budget = Duration::from_secs(10)
                + Duration::from_secs(30) * (7 + recipients)
                + Duration::from_secs(120);
            assert!(
                reservation_expiry(&r.downstream, recipients as usize) > budget,
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
