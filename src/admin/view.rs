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

use crate::config::{Config, DomainGroup, RecipientFrequency, Route, ShareSchedule, TlsMode};
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
    let report = registry.report(&route.name)?;
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

/// Project one route. `usage` is keyed `(route, domain_group)` so one query can
/// feed every route.
pub fn project_route(
    cfg: &Config,
    route: &Route,
    state: RouteState,
    usage: &UsageByRoute,
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

    let groups = cfg
        .domain_groups
        .iter()
        .map(|group| {
            let scheduled = quota::allowance_for(route, &group.name, day_index, state);
            let usage = usage.get(&(route.name.clone(), group.name.clone()));
            let share = crate::routing::partial::share_for_group(
                route, day_index, state, now, scheduled, usage,
            );
            project_group(group, scheduled, usage, share)
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
        downstream: DownstreamView {
            host: route.downstream.host.clone(),
            port: route.downstream.port,
            tls: tls_name(route.downstream.tls),
            smtputf8: route.downstream.smtputf8,
            assume_8bitmime: route.downstream.assume_8bitmime,
            authenticated: route.downstream.auth.is_some(),
        },
        recipient_frequency: route.recipient_frequency.as_ref().map(frequency_view),
        partial_ramp: route
            .warmup
            .as_ref()
            .filter(|w| w.schedule.has_partial_ramp())
            .map(|w| match &w.schedule.share {
                ShareSchedule::Days(days) => PartialRampView {
                    mode: "days",
                    share: days.clone(),
                    today: crate::routing::partial::share_today(route, day_index, state),
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
        groups,
        preflight: preflight_view(route, preflight),
        pool: pools.stats(&route.name),
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
        },
    }
}

/// §9.2 `GET /routes`, for every route in configuration order.
pub fn project_routes(
    cfg: &Config,
    states: &HashMap<String, RouteState>,
    usage: &UsageByRoute,
    preflight: &crate::preflight::Registry,
    pools: &crate::downstream::Pool,
    now: DateTime<Utc>,
) -> RoutesView {
    RoutesView {
        generated_at: now,
        routes: cfg
            .routes
            .iter()
            .map(|route| {
                let state = states.get(&route.name).copied().unwrap_or_default();
                project_route(cfg, route, state, usage, preflight, pools, now)
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
        }
    }

    fn view(cfg: &Config, route: &str, state: RouteState, usage: &UsageByRoute) -> RouteView {
        project_route(
            cfg,
            cfg.route(route).unwrap(),
            state,
            usage,
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
            &cfg,
            cfg.route("warming").unwrap(),
            RouteState::default(),
            &UsageByRoute::new(),
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
            &cfg,
            cfg.route("warming").unwrap(),
            RouteState {
                paused: true,
                graduated: false,
            },
            &UsageByRoute::new(),
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
            &cfg,
            cfg.route("warming").unwrap(),
            state,
            &UsageByRoute::new(),
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
            &cfg,
            &states,
            &usage,
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
            &cfg,
            &HashMap::new(),
            &UsageByRoute::new(),
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
            &cfg,
            &HashMap::new(),
            &UsageByRoute::new(),
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
}
