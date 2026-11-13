//! §9.2's projections: configuration plus live state, as data.
//!
//! Pure functions over `(config, route state, usage rows, now)`. Nothing here
//! touches the database or the HTTP layer, which is what makes the interesting
//! part — which of two disagreeing numbers an operator is shown — testable
//! without either.
//!
//! **The rule this module exists to enforce (D-026).** `quota_usage.allowance`
//! is authoritative once written: a schedule edit plus a restart does not raise
//! today's ceiling, because the row was created with yesterday's number and the
//! reservation protocol reads the row. A read API that reported the *schedule*
//! would therefore tell an operator the route can send 2000 more while the
//! reservation protocol refuses at 500 — at exactly the moment they are trying
//! to work out why mail is being deferred. So both numbers are reported, always,
//! and [`GroupWindow::drift`] says when they disagree.
//!
//! **And the rule about what must never appear here (§7.3).** No recipient, no
//! recipient key, no per-recipient count. The frequency constraint is reported
//! as *configuration* — mode, window, threshold — and never as observations.
//! §7.3 hashes so that the container does not accumulate a record of who was
//! mailed, and an endpoint that returned keys would hand that record back out.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::{DomainGroup, Ramp, RecipientFrequency, Route, ShareSchedule, TlsMode};
use crate::quota::store::{RouteState, Usage};
use crate::quota::{self, Allowance};

/// One `(route, domain_group)` window: what the schedule says, what the row
/// says, and what is left.
///
/// Not `Eq`: D-097's computed share is an `f64`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupWindow {
    pub domain_group: String,
    /// What §7.2's schedule says for this day index, from the configuration as
    /// it is loaded right now. `null` is no ceiling — an overflow route.
    pub scheduled: Option<i64>,
    /// What the row says, which is what §7.4 actually enforces. Equal to
    /// `scheduled` until a schedule edit or an override makes it otherwise.
    pub allowance: Option<i64>,
    /// §9.3's per-group override, or `null`. Expires at `day_ends_at` by
    /// construction — it is a column on the row for one day index (D-025).
    #[serde(rename = "override")]
    pub allowance_override: Option<i64>,
    pub committed: i64,
    pub reserved: i64,
    /// `allowance - committed - reserved`, floored at zero. `null` is unlimited.
    pub headroom: Option<i64>,
    /// The row and the configured schedule disagree (D-026). Not an error: it is
    /// the expected state for the rest of the day after a schedule edit.
    pub drift: bool,
    /// The partial ramp's share for this window right now, or `null` when every
    /// message is offered (§3.2 step 3c′). Under D-091's list this is the day's
    /// entry, the same for every group; under D-097 it is computed from the
    /// numbers above and the clock, so it moves as the cap fills and differs
    /// between groups.
    ///
    /// Computed by the function the walk calls, so §9's control plane cannot
    /// describe a decision the walk did not make.
    pub partial_ramp_share: Option<f64>,
    /// False means nothing has been sent on this route and group today, so the
    /// numbers above are what the first message will create rather than what is
    /// stored.
    pub row_exists: bool,
    /// D-115 — the schedule entry `scheduled` comes from. Equal to the day index
    /// under `advance: auto`; `null` for an overflow route or before the start.
    pub step: Option<i64>,
    /// D-115 — `day_index - step`: how far this window is held behind its
    /// calendar by low volume.
    pub held_days: Option<i64>,
    /// D-115 — what tomorrow's allowance will be if today meets the threshold.
    /// Under `auto`, what it will be regardless.
    pub next_allowance_if_met: Option<i64>,
    /// D-115 — the day today's step was decided from, under the threshold rule.
    /// `null` under `auto` and on a window's first day.
    pub previous: Option<PreviousDayView>,
}

/// D-115 — the last day a window sent on, and whether it met its bar.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PreviousDayView {
    pub day_index: i64,
    pub step: i64,
    /// The planned allowance — never §9.3's override (D-115).
    pub allowance: Option<i64>,
    pub committed: i64,
    /// `ceil(threshold × allowance)`.
    pub required: i64,
    /// `committed / allowance`; `null` for a zero or unlimited allowance.
    pub attainment: Option<f64>,
    pub met: bool,
}

/// D-115 — how a route moves along its schedule.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct AdvanceView {
    /// `auto` or `threshold`.
    pub mode: &'static str,
    /// `null` under `auto`.
    pub threshold: Option<f64>,
    /// `route`, `ramp` or `default`: which configuration level said so.
    pub source: &'static str,
}

/// D-116 — §9.2's view of one ramp group: its shared ceiling today and what
/// its member ramps' warming routes have spent of it. Names and counts only.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RampGroupView {
    pub name: String,
    pub ramps: Vec<String>,
    pub started: DateTime<Utc>,
    pub schedule: Vec<i64>,
    pub advance: GroupAdvanceView,
    /// Elapsed days since `started`; negative before it.
    pub day_index: i64,
    /// False before `started`: the group caps nothing yet and has no row.
    pub in_force: bool,
    /// `null` before `started`.
    pub step: Option<i64>,
    pub held_days: Option<i64>,
    /// What the row says once it exists, else what the schedule plans.
    pub allowance: Option<i64>,
    pub committed: i64,
    pub reserved: i64,
    pub headroom: Option<i64>,
    pub row_exists: bool,
    pub next_allowance_if_met: Option<i64>,
    pub previous: Option<PreviousDayView>,
    pub day_ends_at: DateTime<Utc>,
}

/// D-116 — how a ramp group moves along its schedule.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct GroupAdvanceView {
    pub mode: &'static str,
    pub threshold: Option<f64>,
}

/// D-116 — project a group from [`quota::group_position`] and its row.
pub fn project_ramp_group(
    group: &crate::config::RampGroup,
    position: &quota::GroupPosition,
    usage: Option<Usage>,
) -> RampGroupView {
    let in_force = position.allowance.is_some();
    let row = usage.unwrap_or_default();
    let allowance = row.allowance.or(position.allowance);
    RampGroupView {
        name: group.name.clone(),
        ramps: group.ramps.clone(),
        started: group.started,
        schedule: group.schedule.clone(),
        advance: GroupAdvanceView {
            mode: position.rule.mode(),
            threshold: position.rule.threshold(),
        },
        day_index: position.day_index,
        in_force,
        step: in_force.then_some(position.step),
        held_days: in_force.then_some(position.held_days()),
        allowance,
        committed: row.committed,
        reserved: row.reserved,
        headroom: allowance.map(|a| (a - row.committed - row.reserved).max(0)),
        row_exists: usage.is_some(),
        next_allowance_if_met: if in_force {
            group.allowance_at(position.step + 1)
        } else {
            group.allowance_at(0)
        },
        previous: position.decision.basis.map(|b| PreviousDayView {
            day_index: b.previous.day_index,
            step: b.previous.step_or_day(),
            allowance: b.previous.allowance,
            committed: b.previous.committed,
            required: b.required,
            attainment: b.attainment(),
            met: b.met,
        }),
        day_ends_at: quota::day::boundary(group.started, position.day_index + 1),
    }
}

/// Whether a route can be selected at all, which is a separate question from
/// whether it has headroom.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteStatus {
    /// Selectable, quota permitting.
    Active,
    /// §9.3 — an operator paused it. Outranks everything else, since it is the
    /// only one of the three anybody chose.
    Paused,
    /// §7.2 — `warmup.started` is in the future.
    NotStarted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DownstreamView {
    pub host: String,
    pub port: u16,
    pub tls: &'static str,
    pub smtputf8: bool,
    /// D-074: the downstream is declared 8-bit clean without `8BITMIME`.
    pub assume_8bitmime: bool,
    /// Whether downstream credentials are configured — never what they are.
    pub authenticated: bool,
}

/// §7.3's constraint as *configuration*. Deliberately carries no observation:
/// see the module comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrequencyView {
    pub mode: &'static str,
    pub window_unit: &'static str,
    pub window_count: u32,
    pub threshold: u32,
}

/// D-091's and D-097's `warmup.schedule.share` as configuration, plus the share
/// in force today. Carries nothing about which messages were or were not
/// offered: that is per recipient, and a read endpoint emits no recipient
/// (§7.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PartialRampView {
    /// `"days"` for D-091's list, `"auto"` for D-097's controller.
    pub mode: &'static str,
    /// The configured list. Empty under `auto`.
    pub share: Vec<f64>,
    /// `null` when every message is offered today: before `warmup.started`,
    /// past the end of `share`, at a share of 1, or when the route is graduated
    /// — the same rule the walk applies, by the same function.
    ///
    /// Always `null` under `auto`, and not because nothing is in force: the
    /// share is then per domain group, and each window carries its own. Saying
    /// one number here would be picking a group and not saying which.
    pub today: Option<f64>,
    /// D-097's parameters, under `auto`.
    pub auto: Option<AutoShareView>,
}

/// D-097's `share: {mode: auto}` as configuration. Mirrored rather than
/// serialising the config struct, for [`tls_name`]'s reason: these are API field
/// values, and renaming a config field should not silently change what a client
/// sees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct AutoShareView {
    pub floor: f64,
    pub ceiling: f64,
    pub gain: f64,
    pub fill_by: f64,
    pub tail_below: f64,
    pub tail_ceiling: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteView {
    pub name: String,
    pub overflow: bool,
    pub status: RouteStatus,
    pub paused: bool,
    /// §9.3 — pinned to the final schedule value.
    pub graduated: bool,
    /// §7.2's elapsed-duration index. Negative before `warmup.started`.
    pub day_index: i64,
    pub day_started_at: DateTime<Utc>,
    /// When this day index ends — equivalently, when a §9.3 allowance override
    /// on this route expires.
    pub day_ends_at: DateTime<Utc>,
    pub warmup_started: Option<DateTime<Utc>>,
    pub downstream: DownstreamView,
    pub recipient_frequency: Option<FrequencyView>,
    pub partial_ramp: Option<PartialRampView>,
    /// D-115 — `null` for an overflow route, which has no schedule.
    pub advance: Option<AdvanceView>,
    pub groups: Vec<GroupWindow>,
    /// §9.2's preflight results (§6.7).
    ///
    /// Still `null` rather than absent when there is no answer, which is now a
    /// statement about the route rather than about the phase: preflight is
    /// disabled, or its identity domain is not a constant so nothing is checkable
    /// (D-064), or the first pass has not completed. An operator must be able to
    /// tell "no answer" from "passed" — reporting a pass we never established is
    /// exactly the §9 lie the control plane must not tell.
    pub preflight: Option<PreflightView>,
    /// §9.2's pool statistics (§8.3).
    ///
    /// `null` only for a route this process has no pool for at all, which cannot
    /// happen for a configured route — the pool is seeded from the same `Config`.
    /// A route nothing has sent through reports zeros against its configured
    /// `max_connections`, which is the honest answer and a more useful one than
    /// an absent field.
    pub pool: Option<crate::downstream::PoolStats>,
    /// D-112 — the route's `downstream_overrides`, each with its own pool, in
    /// group order. Empty when the route has none: every group uses `downstream`.
    pub downstream_overrides: Vec<DownstreamOverrideView>,
}

/// D-112 — one group's override: where that group's mail goes, and its pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DownstreamOverrideView {
    pub group: String,
    pub downstream: DownstreamView,
    pub pool: Option<crate::downstream::PoolStats>,
}

/// §9.2 `GET /routes`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoutesView {
    pub generated_at: DateTime<Utc>,
    pub routes: Vec<RouteView>,
}

/// §6.7's last verdict for one route, as §9.2 reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreflightView {
    pub domain: String,
    pub checked_at: DateTime<Utc>,
    /// Whether a failure here actually makes the route ineligible. Without it an
    /// operator cannot tell a warning from a block, and those call for different
    /// responses at different hours of the night.
    pub strict: bool,
    pub ok: bool,
    pub checks: Vec<PreflightCheckView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreflightCheckView {
    pub check: String,
    pub ok: bool,
    pub detail: String,
}

fn preflight_view(route: &Route, registry: &crate::preflight::Registry) -> Option<PreflightView> {
    let report = registry.report(&route.ramp, &route.name)?;
    Some(PreflightView {
        domain: report.domain.clone(),
        checked_at: report.checked_at,
        strict: route.preflight.as_ref().is_some_and(|p| p.strict),
        ok: report.all_ok(),
        checks: report
            .checks
            .iter()
            .map(|c| PreflightCheckView {
                check: c.check.as_str().to_string(),
                ok: c.ok,
                detail: c.detail.clone(),
            })
            .collect(),
    })
}

/// Everything the projection needs from storage, keyed the way it is read.
pub type UsageByRoute = HashMap<(String, String), Usage>;

/// D-115 — the rows each `(route, domain_group)`'s step is decided from, keyed
/// like [`UsageByRoute`]. Absent is "no rows read", which is the right answer
/// for a route under `advance: auto` and for one that has never sent.
pub type AdvanceByRoute = HashMap<(String, String), quota::AdvanceRows>;

/// D-115 — every `(route, domain_group)`'s position, from rows already read.
/// The same pure function `quota::position` calls, so `/routes` and the gauges
/// cannot describe a step the walk is not using.
pub fn position_for(
    route: &Route,
    group: &str,
    state: RouteState,
    advance: &AdvanceByRoute,
    now: DateTime<Utc>,
) -> quota::Position {
    let day_index = quota::day::for_route(route, now);
    let rows = advance
        .get(&(route.name.clone(), group.to_string()))
        .copied()
        .unwrap_or_default();
    quota::position_from(route, group, day_index, state, rows)
}

/// Project one route. `usage` is keyed `(route, domain_group)` so one query can
/// feed every route.
#[allow(clippy::too_many_arguments)]
pub fn project_route(
    ramp: &Ramp,
    route: &Route,
    state: RouteState,
    usage: &UsageByRoute,
    advance: &AdvanceByRoute,
    preflight: &crate::preflight::Registry,
    pools: &crate::downstream::Pool,
    now: DateTime<Utc>,
) -> RouteView {
    let day_index = quota::day::for_route(route, now);
    let origin = quota::day::origin(route);

    let status = if state.paused {
        RouteStatus::Paused
    } else if day_index < 0 {
        RouteStatus::NotStarted
    } else {
        RouteStatus::Active
    };

    let groups = ramp
        .domain_groups
        .iter()
        .map(|group| {
            let position = position_for(route, &group.name, state, advance, now);
            let scheduled = position.allowance;
            let usage = usage.get(&(route.name.clone(), group.name.clone()));
            let share = crate::routing::partial::share_for_group(
                route,
                day_index,
                position.step,
                state,
                now,
                scheduled,
                usage,
            );
            let mut window = project_group(group, scheduled, usage, share);
            if route.warmup.is_some() && day_index >= 0 {
                window.step = Some(position.step);
                window.held_days = Some(position.held_days());
                window.next_allowance_if_met = quota::allowance_for(
                    route,
                    &group.name,
                    day_index + 1,
                    position.step + 1,
                    state,
                )
                .as_column();
                window.previous = position.decision.basis.map(|b| PreviousDayView {
                    day_index: b.previous.day_index,
                    step: b.previous.step_or_day(),
                    allowance: b.previous.allowance,
                    committed: b.previous.committed,
                    required: b.required,
                    attainment: b.attainment(),
                    met: b.met,
                });
            }
            window
        })
        .collect();

    RouteView {
        name: route.name.clone(),
        overflow: route.overflow,
        status,
        paused: state.paused,
        graduated: state.graduated,
        day_index,
        day_started_at: quota::day::boundary(origin, day_index),
        day_ends_at: quota::day::boundary(origin, day_index + 1),
        warmup_started: route.warmup.as_ref().map(|w| w.started),
        downstream: downstream_view(&route.downstream),
        recipient_frequency: route.recipient_frequency.as_ref().map(frequency_view),
        partial_ramp: route
            .warmup
            .as_ref()
            .filter(|w| w.schedule.has_partial_ramp())
            .map(|w| match &w.schedule.share {
                ShareSchedule::Days(days) => PartialRampView {
                    mode: "days",
                    share: days.clone(),
                    // A list is indexed by step (D-115), which may differ per
                    // group; the route-level summary is the first group's.
                    today: crate::routing::partial::share_today(
                        route,
                        day_index,
                        ramp.domain_groups.first().map_or(day_index, |g| {
                            position_for(route, &g.name, state, advance, now).step
                        }),
                        state,
                    ),
                    auto: None,
                },
                ShareSchedule::Auto(a) => PartialRampView {
                    mode: "auto",
                    share: Vec::new(),
                    today: None,
                    auto: Some(AutoShareView {
                        floor: a.floor,
                        ceiling: a.ceiling,
                        gain: a.gain,
                        fill_by: a.fill_by,
                        tail_below: a.tail.below,
                        tail_ceiling: a.tail.ceiling,
                    }),
                },
            }),
        advance: route.warmup.as_ref().map(|w| AdvanceView {
            mode: w.advance().mode(),
            threshold: w.advance().threshold(),
            source: match w.advance_source {
                crate::config::AdvanceSource::Route => "route",
                crate::config::AdvanceSource::Ramp => "ramp",
                crate::config::AdvanceSource::Default => "default",
            },
        }),
        groups,
        preflight: preflight_view(route, preflight),
        pool: pools.stats(&route.ramp, &route.name),
        downstream_overrides: route
            .downstream_overrides
            .iter()
            .map(|(group, ds)| DownstreamOverrideView {
                group: group.clone(),
                downstream: downstream_view(ds),
                pool: pools.stats_for(&route.ramp, &route.name, Some(group)),
            })
            .collect(),
    }
}

fn downstream_view(ds: &crate::config::Downstream) -> DownstreamView {
    DownstreamView {
        host: ds.host.clone(),
        port: ds.port,
        tls: tls_name(ds.tls),
        smtputf8: ds.smtputf8,
        assume_8bitmime: ds.assume_8bitmime,
        authenticated: ds.auth.is_some(),
    }
}

/// One window. The whole of D-026 is in the `drift` line.
pub fn project_group(
    group: &DomainGroup,
    scheduled: Allowance,
    usage: Option<&Usage>,
    partial_ramp_share: Option<f64>,
) -> GroupWindow {
    let scheduled_column = scheduled.as_column();

    match usage {
        // No row yet: nothing has been sent on this route and group today, so
        // the schedule *is* what the first message will write. Reporting it as
        // the allowance is not a guess — `lock_usage` inserts exactly this.
        None => GroupWindow {
            domain_group: group.name.clone(),
            scheduled: scheduled_column,
            allowance: scheduled_column,
            allowance_override: None,
            committed: 0,
            reserved: 0,
            headroom: scheduled_column,
            drift: false,
            partial_ramp_share,
            row_exists: false,
            step: None,
            held_days: None,
            next_allowance_if_met: None,
            previous: None,
        },
        Some(u) => GroupWindow {
            domain_group: group.name.clone(),
            scheduled: scheduled_column,
            allowance: u.allowance,
            allowance_override: u.allowance_override,
            committed: u.committed,
            reserved: u.reserved,
            headroom: u.headroom(),
            // Compared against `allowance` rather than `effective_allowance`: an
            // override is a deliberate act by an operator who already knows,
            // whereas drift is something that happened to them.
            drift: u.allowance != scheduled_column,
            partial_ramp_share,
            row_exists: true,
            step: None,
            held_days: None,
            next_allowance_if_met: None,
            previous: None,
        },
    }
}

/// §9.2 `GET /routes`, for every route in configuration order.
pub fn project_routes(
    ramp: &Ramp,
    states: &HashMap<String, RouteState>,
    usage: &UsageByRoute,
    advance: &AdvanceByRoute,
    preflight: &crate::preflight::Registry,
    pools: &crate::downstream::Pool,
    now: DateTime<Utc>,
) -> RoutesView {
    RoutesView {
        generated_at: now,
        routes: ramp
            .routes
            .iter()
            .map(|route| {
                let state = states.get(&route.name).copied().unwrap_or_default();
                project_route(ramp, route, state, usage, advance, preflight, pools, now)
            })
            .collect(),
    }
}

fn frequency_view(c: &RecipientFrequency) -> FrequencyView {
    use crate::config::{FrequencyMode, WindowUnit};

    FrequencyView {
        mode: match c.mode {
            FrequencyMode::ToAddress => "to_address",
            FrequencyMode::ToDomain => "to_domain",
        },
        window_unit: match c.window.unit {
            WindowUnit::Hourly => "hourly",
            WindowUnit::Daily => "daily",
            WindowUnit::Weekly => "weekly",
        },
        window_count: c.window.count,
        threshold: c.threshold,
    }
}

/// Spelled out rather than derived from the enum: these are API field values,
/// and renaming a config variant should not silently change what a client sees.
fn tls_name(mode: TlsMode) -> &'static str {
    match mode {
        TlsMode::Off => "off",
        TlsMode::Opportunistic => "opportunistic",
        TlsMode::Required => "required",
        TlsMode::RequiredVerify => "required_verify",
    }
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
  max_recipients: 1
  max_concurrent_sessions: 4
  allowed_cidrs: ["10.0.0.0/8"]
  timeouts: { command: 30s, data: 300s, session: 600s }
  auth: { allow_insecure_auth: true }
database: { url: "postgres://u:p@localhost/simmer", connect_timeout: 5s }
admin: { listen: "127.0.0.1:8080", auth_token: "0123456789abcdef" }
default_ramp: main
ramps:
 main:
  domain_groups:
  - { name: google, domains: ["gmail.com"] }
  - { name: catchall, domains: ["*"] }
  senders:
  - { match: "oldbrand.com", match_on: envelope, chain: [warming, overflow] }
  default_chain: [overflow]
  # These tests are about D-026's drift and the calendar's day index; D-115's
  # threshold rule has its own below, from `threshold_config`.
  warmup_advance: auto
  routes:
  - name: warming
    downstream:
      host: w.example
      port: 587
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@newbrand.com" }
    recipient_frequency: { mode: to_address, window: { unit: daily, count: 1 }, threshold: 2 }
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
      tls: required
      pool: { max_connections: 1, idle_ttl: 60s, max_messages_per_connection: 10 }
    identity: { envelope_from: "b@established.com" }
"#;

    fn config() -> Config {
        crate::config::from_str(CFG, "test").expect("fixture is valid")
    }

    /// Two days and change after `warmup.started`, so day_index is 2.
    fn now() -> DateTime<Utc> {
        "2026-08-03T15:00:00Z".parse().unwrap()
    }

    fn group(name: &str) -> DomainGroup {
        DomainGroup {
            name: name.to_string(),
            domains: vec![],
            mx: vec![],
        }
    }

    fn view(cfg: &Config, route: &str, state: RouteState, usage: &UsageByRoute) -> RouteView {
        project_route(
            cfg.default_ramp(),
            cfg.default_ramp().route(route).unwrap(),
            state,
            usage,
            &AdvanceByRoute::new(),
            &crate::preflight::Registry::new(),
            &crate::downstream::Pool::build(cfg),
            now(),
        )
    }

    fn window<'a>(v: &'a RouteView, group: &str) -> &'a GroupWindow {
        v.groups
            .iter()
            .find(|g| g.domain_group == group)
            .expect("group is configured")
    }

    // -- D-026: the row wins, and drift is visible ------------------------

    #[test]
    fn a_row_that_disagrees_with_the_schedule_is_reported_as_drift() {
        // The case that matters: the operator edited the schedule and restarted,
        // so the configuration says 200 and the row — written this morning under
        // the old schedule — says 100. §7.4 enforces 100. Reporting 200 would
        // send them looking for a bug in the ramp.
        let cfg = config();
        let mut usage = UsageByRoute::new();
        usage.insert(
            ("warming".into(), "catchall".into()),
            Usage {
                allowance: Some(100),
                allowance_override: None,
                committed: 30,
                reserved: 0,
            },
        );

        let v = view(&cfg, "warming", RouteState::default(), &usage);
        let w = window(&v, "catchall");
        assert_eq!(w.scheduled, Some(200), "the configuration's number");
        assert_eq!(
            w.allowance,
            Some(100),
            "the row's number, which §7.4 enforces"
        );
        assert!(w.drift);
        assert_eq!(w.headroom, Some(70));
    }

    #[test]
    fn agreement_between_row_and_schedule_is_not_drift() {
        let cfg = config();
        let mut usage = UsageByRoute::new();
        usage.insert(
            ("warming".into(), "catchall".into()),
            Usage {
                allowance: Some(200),
                allowance_override: None,
                committed: 1,
                reserved: 2,
            },
        );

        let v = view(&cfg, "warming", RouteState::default(), &usage);
        let w = window(&v, "catchall");
        assert!(!w.drift);
        assert_eq!(w.headroom, Some(197), "reserved counts against headroom");
    }

    #[test]
    fn an_override_is_not_drift() {
        // An override is a deliberate act by an operator who already knows the
        // ceiling moved. Flagging it as drift would train them to ignore the
        // flag that exists for the case they did not choose.
        let cfg = config();
        let mut usage = UsageByRoute::new();
        usage.insert(
            ("warming".into(), "catchall".into()),
            Usage {
                allowance: Some(200),
                allowance_override: Some(500),
                committed: 0,
                reserved: 0,
            },
        );

        let v = view(&cfg, "warming", RouteState::default(), &usage);
        let w = window(&v, "catchall");
        assert!(!w.drift);
        assert_eq!(w.allowance_override, Some(500));
        assert_eq!(
            w.headroom,
            Some(500),
            "the override wins over the row (D-025)"
        );
    }

    #[test]
    fn an_override_below_what_is_committed_reports_zero_headroom_not_a_negative() {
        let cfg = config();
        let mut usage = UsageByRoute::new();
        usage.insert(
            ("warming".into(), "catchall".into()),
            Usage {
                allowance: Some(200),
                allowance_override: Some(10),
                committed: 50,
                reserved: 0,
            },
        );

        let v = view(&cfg, "warming", RouteState::default(), &usage);
        let w = window(&v, "catchall");
        assert_eq!(w.headroom, Some(0));
    }

    // -- rows that do not exist yet ---------------------------------------

    #[test]
    fn a_group_with_no_row_reports_the_schedule_and_says_so() {
        let cfg = config();
        let v = view(&cfg, "warming", RouteState::default(), &UsageByRoute::new());

        let w = window(&v, "catchall");
        assert!(!w.row_exists);
        assert_eq!(w.scheduled, Some(200));
        assert_eq!(w.allowance, Some(200), "what the first message will write");
        assert_eq!(w.headroom, Some(200));
        assert!(!w.drift, "nothing to disagree with yet");
    }

    #[test]
    fn every_configured_domain_group_appears_even_with_no_traffic() {
        // "Which groups am I not sending to" is as operationally useful as the
        // ones with rows, and it is the only way to see an override series that
        // is never being exercised.
        let cfg = config();
        let v = view(&cfg, "warming", RouteState::default(), &UsageByRoute::new());
        assert_eq!(v.groups.len(), 2);
        assert_eq!(
            window(&v, "google").scheduled,
            Some(40),
            "the google override series"
        );
        assert_eq!(window(&v, "catchall").scheduled, Some(200));
    }

    // -- status ------------------------------------------------------------

    #[test]
    fn a_paused_route_reports_paused_even_though_it_has_headroom() {
        let cfg = config();
        let state = RouteState {
            paused: true,
            graduated: false,
        };
        let v = view(&cfg, "warming", state, &UsageByRoute::new());
        assert_eq!(v.status, RouteStatus::Paused);
        assert!(v.paused);
        // Headroom is a fact about quota, not about eligibility. Zeroing it here
        // would make "why is this route not sending" unanswerable from one call.
        assert_eq!(window(&v, "catchall").headroom, Some(200));
    }

    #[test]
    fn a_future_start_reports_not_started() {
        let cfg = config();
        let earlier: DateTime<Utc> = "2026-07-30T00:00:00Z".parse().unwrap();
        let v = project_route(
            cfg.default_ramp(),
            cfg.default_ramp().route("warming").unwrap(),
            RouteState::default(),
            &UsageByRoute::new(),
            &AdvanceByRoute::new(),
            &crate::preflight::Registry::new(),
            &crate::downstream::Pool::build(&cfg),
            earlier,
        );
        assert_eq!(v.status, RouteStatus::NotStarted);
        assert!(v.day_index < 0);
    }

    #[test]
    fn pausing_outranks_not_started() {
        // Both are true; only one of them was chosen by a person, and that is
        // the one an operator needs to see first.
        let cfg = config();
        let earlier: DateTime<Utc> = "2026-07-30T00:00:00Z".parse().unwrap();
        let v = project_route(
            cfg.default_ramp(),
            cfg.default_ramp().route("warming").unwrap(),
            RouteState {
                paused: true,
                graduated: false,
            },
            &UsageByRoute::new(),
            &AdvanceByRoute::new(),
            &crate::preflight::Registry::new(),
            &crate::downstream::Pool::build(&cfg),
            earlier,
        );
        assert_eq!(v.status, RouteStatus::Paused);
    }

    #[test]
    fn a_graduated_route_reports_its_pinned_allowance() {
        let cfg = config();
        let state = RouteState {
            paused: false,
            graduated: true,
        };
        // Day index 2 would be 200 anyway on the default series, so assert on
        // google, whose series ends at 40 after two entries.
        let earlier: DateTime<Utc> = "2026-08-01T10:00:00Z".parse().unwrap();
        let v = project_route(
            cfg.default_ramp(),
            cfg.default_ramp().route("warming").unwrap(),
            state,
            &UsageByRoute::new(),
            &AdvanceByRoute::new(),
            &crate::preflight::Registry::new(),
            &crate::downstream::Pool::build(&cfg),
            earlier,
        );
        assert!(v.graduated);
        assert_eq!(v.day_index, 0);
        assert_eq!(
            window(&v, "google").scheduled,
            Some(40),
            "pinned to the final value"
        );
        assert_eq!(window(&v, "catchall").scheduled, Some(200));
    }

    // -- overflow ----------------------------------------------------------

    #[test]
    fn an_overflow_route_reports_no_ceiling_but_still_accounts() {
        // D-024: never quota-limited, but it accounts, so "how much is spilling
        // to overflow" is answerable.
        let cfg = config();
        let mut usage = UsageByRoute::new();
        usage.insert(
            ("overflow".into(), "catchall".into()),
            Usage {
                allowance: None,
                allowance_override: None,
                committed: 4_000,
                reserved: 1,
            },
        );

        let v = view(&cfg, "overflow", RouteState::default(), &usage);
        assert!(v.overflow);
        assert_eq!(v.warmup_started, None);
        let w = window(&v, "catchall");
        assert_eq!(w.scheduled, None, "null is no ceiling, not zero");
        assert_eq!(w.headroom, None);
        assert_eq!(w.committed, 4_000);
        assert!(!w.drift);
    }

    // -- day boundaries ----------------------------------------------------

    #[test]
    fn the_day_window_is_the_route_anniversary_not_midnight() {
        // §7.2 is elapsed duration from `warmup.started`, so a route started at
        // 09:00 rolls over at 09:00 — which is also when a §9.3 override expires.
        let cfg = config();
        let v = view(&cfg, "warming", RouteState::default(), &UsageByRoute::new());
        assert_eq!(v.day_index, 2);
        assert_eq!(v.day_started_at.to_rfc3339(), "2026-08-03T09:00:00+00:00");
        assert_eq!(v.day_ends_at.to_rfc3339(), "2026-08-04T09:00:00+00:00");
    }

    #[test]
    fn an_overflow_route_buckets_on_utc_midnight() {
        // D-024's synthetic Unix-epoch origin. The two clocks differ and are
        // never compared.
        let cfg = config();
        let v = view(
            &cfg,
            "overflow",
            RouteState::default(),
            &UsageByRoute::new(),
        );
        assert_eq!(v.day_started_at.to_rfc3339(), "2026-08-03T00:00:00+00:00");
        assert_eq!(v.day_ends_at.to_rfc3339(), "2026-08-04T00:00:00+00:00");
    }

    // -- what must not be exposed ------------------------------------------

    #[test]
    fn the_frequency_constraint_is_reported_as_configuration_only() {
        let cfg = config();
        let v = view(&cfg, "warming", RouteState::default(), &UsageByRoute::new());
        let f = v.recipient_frequency.as_ref().expect("configured");
        assert_eq!(f.mode, "to_address");
        assert_eq!(f.threshold, 2);
        assert_eq!(f.window_unit, "daily");
        assert_eq!(f.window_count, 1);
    }

    #[test]
    fn no_projection_carries_a_recipient_or_a_recipient_key() {
        // §7.3's whole reason for hashing is that the container does not
        // accumulate a record of who was mailed. This asserts the shape of the
        // serialised document rather than any one field, so a future addition
        // has to think about it.
        let cfg = config();
        let mut usage = UsageByRoute::new();
        usage.insert(
            ("warming".into(), "catchall".into()),
            Usage {
                allowance: Some(200),
                allowance_override: None,
                committed: 3,
                reserved: 0,
            },
        );
        let mut states = HashMap::new();
        states.insert(
            "warming".to_string(),
            RouteState {
                paused: true,
                graduated: false,
            },
        );

        let json = serde_json::to_string(&project_routes(
            cfg.default_ramp(),
            &states,
            &usage,
            &AdvanceByRoute::new(),
            &crate::preflight::Registry::new(),
            &crate::downstream::Pool::build(&cfg),
            now(),
        ))
        .unwrap();
        for forbidden in [
            "recipient_hash",
            "recipient_key",
            "recipients",
            "@",
            "seen",
            "events",
        ] {
            assert!(
                !json.contains(forbidden),
                "the §9.2 projection must not contain '{forbidden}': {json}"
            );
        }
    }

    #[test]
    fn downstream_credentials_are_never_projected() {
        let cfg = config();
        let v = view(&cfg, "warming", RouteState::default(), &UsageByRoute::new());
        assert!(!v.downstream.authenticated);
        assert_eq!(v.downstream.host, "w.example");
        assert_eq!(v.downstream.tls, "required_verify", "the §8.2 default");
        let overflow = view(
            &cfg,
            "overflow",
            RouteState::default(),
            &UsageByRoute::new(),
        );
        assert_eq!(overflow.downstream.tls, "required");
    }

    // -- the two fields §9.2 asks for beyond the quota ---------------------

    #[test]
    fn an_unchecked_preflight_is_null_rather_than_absent() {
        // Omitting the key would let a reader conclude the checks passed; null
        // says the field exists and has no answer.
        let cfg = config();
        let v = view(&cfg, "warming", RouteState::default(), &UsageByRoute::new());
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["preflight"], serde_json::Value::Null);
    }

    #[test]
    fn a_route_nothing_has_sent_through_reports_its_pool_as_zeros() {
        // §9.2's pool statistics. The contrast with preflight above is the point:
        // "no connection has been opened" is a fact we know, and reporting it as
        // zeros against the configured ceiling is more use than null. Null here
        // would mean the process has no pool for this route at all, which cannot
        // happen for a configured one.
        let cfg = config();
        let v = view(&cfg, "warming", RouteState::default(), &UsageByRoute::new());
        let pool = v.pool.expect("a configured route always has a pool");
        assert_eq!(pool.max_connections, 1);
        assert_eq!(pool.idle, 0);
        assert_eq!(pool.active, 0);
        assert_eq!(pool.opened, 0);
        assert_eq!(pool.reused, 0);
    }

    #[test]
    fn routes_are_projected_in_configuration_order() {
        // Chain order is configuration order, and an operator reading /routes is
        // usually asking "what does the chain do next".
        let cfg = config();
        let v = project_routes(
            cfg.default_ramp(),
            &HashMap::new(),
            &UsageByRoute::new(),
            &AdvanceByRoute::new(),
            &crate::preflight::Registry::new(),
            &crate::downstream::Pool::build(&cfg),
            now(),
        );
        let names: Vec<_> = v.routes.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["warming", "overflow"]);
    }

    #[test]
    fn a_route_with_no_state_row_reads_as_the_default() {
        let cfg = config();
        let v = project_routes(
            cfg.default_ramp(),
            &HashMap::new(),
            &UsageByRoute::new(),
            &AdvanceByRoute::new(),
            &crate::preflight::Registry::new(),
            &crate::downstream::Pool::build(&cfg),
            now(),
        );
        assert!(!v.routes[0].paused);
        assert!(!v.routes[0].graduated);
        assert_eq!(v.routes[0].status, RouteStatus::Active);
    }

    // -- D-091 and D-097: what §9.2 says about a partial ramp -------------

    /// `CFG` with a `share` on the warming route's schedule.
    fn with_share(share: &str) -> Config {
        let yaml = CFG.replace(
            "        default: [50, 100, 200]\n",
            &format!("        default: [50, 100, 200]\n        share: {share}\n"),
        );
        crate::config::from_str(&yaml, "test").expect("fixture is valid")
    }

    #[test]
    fn a_listed_share_reports_the_list_and_todays_entry() {
        let cfg = with_share("[0.1, 0.25, 0.5]");
        let v = view(&cfg, "warming", RouteState::default(), &UsageByRoute::new());
        let ramp = v.partial_ramp.as_ref().expect("a ramp is configured");

        assert_eq!(ramp.mode, "days");
        assert_eq!(ramp.share, vec![0.1, 0.25, 0.5]);
        assert_eq!(ramp.today, Some(0.5), "day 2 of the list");
        assert!(ramp.auto.is_none());

        // A list applies one value to every group, so each window repeats it.
        for group in ["google", "catchall"] {
            assert_eq!(window(&v, group).partial_ramp_share, Some(0.5));
        }
    }

    #[test]
    fn an_auto_share_reports_its_parameters_and_a_share_per_group() {
        // Day 2: catchall's cap is 200 and google's 40. Half of catchall is
        // gone and none of google's, so the two windows cannot share a number —
        // which is why `today` is null and the windows carry it instead.
        let cfg = with_share("{mode: auto, ceiling: 0.8, floor: 0.01, tail: {below: 0}}");
        let mut usage = UsageByRoute::new();
        usage.insert(
            ("warming".into(), "catchall".into()),
            Usage {
                allowance: Some(200),
                allowance_override: None,
                committed: 100,
                reserved: 0,
            },
        );

        let v = view(&cfg, "warming", RouteState::default(), &usage);
        let ramp = v.partial_ramp.as_ref().expect("a ramp is configured");
        assert_eq!(ramp.mode, "auto");
        assert!(ramp.share.is_empty());
        assert_eq!(
            ramp.today, None,
            "the share is per group under auto, and naming one would not say which"
        );
        let auto = ramp.auto.expect("parameters");
        assert_eq!(auto.ceiling, 0.8);
        assert_eq!(auto.floor, 0.01);
        assert_eq!(auto.fill_by, crate::config::AutoShare::DEFAULT_FILL_BY);
        assert_eq!(auto.tail_below, 0.0);

        let filled = window(&v, "catchall")
            .partial_ramp_share
            .expect("throttled");
        let untouched = window(&v, "google").partial_ramp_share;
        assert!(
            untouched.is_none() || untouched.expect("share") > filled,
            "the group whose cap is untouched must not be the throttled one: \
             google {untouched:?} vs catchall {filled}"
        );
    }

    #[test]
    fn a_graduated_route_reports_no_share_at_all() {
        // §9.3's graduate jumps to the end of the ramp, and the ramp is part of
        // it — under either kind of share.
        for share in ["[0.1, 0.25, 0.5]", "{mode: auto}"] {
            let cfg = with_share(share);
            let state = RouteState {
                graduated: true,
                ..RouteState::default()
            };
            let v = view(&cfg, "warming", state, &UsageByRoute::new());
            assert_eq!(v.partial_ramp.as_ref().expect("configured").today, None);
            for group in ["google", "catchall"] {
                assert_eq!(window(&v, group).partial_ramp_share, None, "{share}");
            }
        }
    }

    #[test]
    fn a_route_with_no_ramp_reports_none() {
        let cfg = config();
        let v = view(&cfg, "warming", RouteState::default(), &UsageByRoute::new());
        assert!(v.partial_ramp.is_none());
        assert_eq!(window(&v, "catchall").partial_ramp_share, None);
    }

    #[test]
    fn project_group_is_callable_on_its_own() {
        let g = group("catchall");
        let w = project_group(&g, Allowance::Limited(10), None, None);
        assert_eq!(w.domain_group, "catchall");
        assert_eq!(w.headroom, Some(10));
        assert_eq!(
            w.partial_ramp_share, None,
            "no ramp means no share reported"
        );

        let w = project_group(&g, Allowance::Limited(10), None, Some(0.25));
        assert_eq!(w.partial_ramp_share, Some(0.25));

        let w = project_group(&g, Allowance::NotStarted, None, None);
        assert_eq!(
            w.scheduled, None,
            "a route that has not started has no ceiling to report; RouteStatus says why"
        );
    }

    // -- D-115: the threshold rule, as /routes reports it ----------------------

    fn threshold_config() -> Config {
        crate::config::from_str(
            &CFG.replace(
                "warmup_advance: auto",
                "warmup_advance: { mode: threshold, threshold: 0.9 }",
            ),
            "test",
        )
        .expect("fixture is valid")
    }

    fn advance_rows(entries: &[(&str, crate::quota::AdvanceRows)]) -> AdvanceByRoute {
        entries
            .iter()
            .map(|(g, rows)| (("warming".to_string(), g.to_string()), *rows))
            .collect()
    }

    fn prior(
        day_index: i64,
        step: i64,
        allowance: i64,
        committed: i64,
    ) -> crate::quota::AdvanceRows {
        crate::quota::AdvanceRows {
            today: None,
            previous: Some(crate::quota::advance::Row {
                day_index,
                step: Some(step),
                allowance: Some(allowance),
                committed,
            }),
        }
    }

    fn threshold_view(advance: &AdvanceByRoute) -> RouteView {
        let cfg = threshold_config();
        project_route(
            cfg.default_ramp(),
            cfg.default_ramp().route("warming").unwrap(),
            RouteState::default(),
            &UsageByRoute::new(),
            advance,
            &crate::preflight::Registry::new(),
            &crate::downstream::Pool::build(&cfg),
            now(),
        )
    }

    #[test]
    fn a_held_window_reports_its_step_and_how_far_behind_it_is() {
        // Day 2. catchall sent 40 of 100 on day 1 at step 1: held at step 1, so
        // 100 again, one day behind. google met 36 of 40 at step 1: step 2,
        // which past the end of [20, 40] repeats 40.
        let v = threshold_view(&advance_rows(&[
            ("catchall", prior(1, 1, 100, 40)),
            ("google", prior(1, 1, 40, 36)),
        ]));
        let catchall = window(&v, "catchall");
        assert_eq!(catchall.step, Some(1));
        assert_eq!(catchall.held_days, Some(1));
        assert_eq!(catchall.scheduled, Some(100));
        assert_eq!(catchall.next_allowance_if_met, Some(200));
        let p = catchall.previous.expect("the day it was decided from");
        assert_eq!(
            (p.day_index, p.committed, p.required, p.met),
            (1, 40, 90, false)
        );
        assert_eq!(p.attainment, Some(0.4));

        let google = window(&v, "google");
        assert_eq!(
            (google.step, google.held_days, google.scheduled),
            (Some(2), Some(0), Some(40))
        );
        assert!(google.previous.unwrap().met);
    }

    #[test]
    fn a_window_that_never_sent_is_on_its_first_step() {
        let v = threshold_view(&AdvanceByRoute::new());
        let w = window(&v, "catchall");
        assert_eq!(
            (w.step, w.held_days, w.scheduled),
            (Some(0), Some(2), Some(50))
        );
        assert_eq!(w.previous, None);
        assert_eq!(w.next_allowance_if_met, Some(100));
    }

    #[test]
    fn the_route_says_how_it_advances_and_where_that_came_from() {
        let v = threshold_view(&AdvanceByRoute::new());
        let a = v.advance.expect("a warming route");
        assert_eq!(
            (a.mode, a.threshold, a.source),
            ("threshold", Some(0.9), "ramp")
        );

        let auto = view(
            &config(),
            "warming",
            RouteState::default(),
            &UsageByRoute::new(),
        );
        let a = auto.advance.expect("a warming route");
        assert_eq!((a.mode, a.threshold), ("auto", None));
        assert_eq!(
            view(
                &config(),
                "overflow",
                RouteState::default(),
                &UsageByRoute::new()
            )
            .advance,
            None
        );
    }

    #[test]
    fn under_auto_the_step_is_the_day_and_nothing_is_held() {
        let v = view(
            &config(),
            "warming",
            RouteState::default(),
            &UsageByRoute::new(),
        );
        let w = window(&v, "catchall");
        assert_eq!(
            (w.step, w.held_days, w.scheduled),
            (Some(2), Some(0), Some(200))
        );
        assert_eq!(w.previous, None);
    }

    #[test]
    fn an_overflow_route_reports_no_step() {
        let v = view(
            &config(),
            "overflow",
            RouteState::default(),
            &UsageByRoute::new(),
        );
        for w in &v.groups {
            assert_eq!((w.step, w.held_days, w.previous), (None, None, None));
        }
    }
}
