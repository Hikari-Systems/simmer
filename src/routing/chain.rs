//! §3.2 step 3 — walk the chain, and take the reservation.
//!
//! ```text
//! For each route:
//!   a. If the route is paused (admin API, §9.3), skip.
//!   b. If the route has a `recipient_frequency` constraint and this recipient is
//!      at or over threshold within the window, skip. Evaluated **first**.
//!   c′. If the route's `schedule.share` is below 1 today and this message is
//!      not in it, skip (D-091).
//!   c. If the route is warming and has no remaining headroom for this domain
//!      group today, skip.
//!   d. Otherwise, attempt reservation (§7.4).
//!   e. First route to reserve successfully is selected.
//! ```
//!
//! Steps (c) and (d) are one operation here, not two. Checking headroom and then
//! reserving would reintroduce the race §7.4 exists to close — the check has to
//! happen *inside* the transaction that holds the row lock, so `reserve` returns
//! either a reservation or [`SkipReason::Quota`].
//!
//! §3.3 governs what happens after: **no failover**. A route that reserves
//! successfully and then fails downstream releases and reports; it does not fall
//! through. Falling through "would silently emit a message under the wrong
//! identity and corrupt both the ramp accounting and the reputation being built".

use std::sync::Arc;

use chrono::Utc;

use super::partial;
use crate::config::{Config, Route};
use crate::frequency::{self, Frequency};
use crate::metrics;
use crate::quota::{
    self,
    store::{QuotaError, QuotaStore, ReserveRequest, Reserved, Usage},
    Allowance, Reservation,
};

/// Why a route was passed over. §9.1's `simmer_route_skipped_total{route,reason}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// §3.2 3a — `POST /routes/{name}/pause`.
    Paused,
    /// §7.2 — `warmup.started` is in the future.
    ///
    /// Not one of §9.1's four enumerated reasons. Folding it into `quota` would
    /// be a lie an operator would waste an afternoon on: "out of quota" and "has
    /// not begun" call for opposite responses.
    NotStarted,
    /// §3.2 3c — no headroom for this domain group today.
    Quota,
    /// §3.2 3b — phase 6.
    Frequency,
    /// §6.7 with `preflight.strict: true` — phase 8.
    Preflight,
    /// §3.2 3c′ (D-091) — `warmup.schedule.share` is below 1 today and this
    /// message is not in it. Not `quota`: the route has headroom, and
    /// is being given less traffic on purpose.
    PartialRamp,
    /// The chain names a route that does not exist. §4.2 rejects this at
    /// startup, so it is unreachable; skipping rather than panicking keeps a
    /// configuration mistake from taking the process down.
    Unknown,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::Paused => "paused",
            SkipReason::NotStarted => "not_started",
            SkipReason::Quota => "quota",
            SkipReason::Frequency => "frequency",
            SkipReason::Preflight => "preflight",
            SkipReason::PartialRamp => "partial_ramp",
            SkipReason::Unknown => "unknown_route",
        }
    }
}

/// One route's verdict, for §9.5's "chain evaluation with skip reasons".
#[derive(Debug, Clone)]
pub struct Step {
    pub route: String,
    pub outcome: Result<(), SkipReason>,
    /// §3.2 step 2a (D-090): selected as a thread-affinity reply's pinned route
    /// with no headroom left, so the reservation was taken past the cap. Only
    /// ever true on an `Ok` step.
    pub over_cap: bool,
}

/// A route selected and its quota reserved.
///
/// Holding one is an obligation: exactly one of [`commit`] or [`release`] must
/// be called. §7.4 has no third outcome.
pub struct Selected<'a> {
    pub route: &'a Route,
    pub domain_group: String,
    pub day_index: i64,
    pub reservation: Reservation,
    /// §7.3 — the recipient keys this route's constraint was evaluated against,
    /// carried forward so §7.4 phase 3 can record them on a downstream `2xx`.
    ///
    /// Empty when the route declares no `recipient_frequency`: the keys are
    /// mode-specific, so they belong to the route that produced them, and a
    /// route with no constraint records nothing at all.
    pub recipient_keys: Vec<frequency::Key>,
    /// §3.2 step 2a (D-090): the reservation was taken past the day's cap,
    /// because this is a thread-affinity reply on its pinned route.
    pub over_cap: bool,
}

/// The result of walking a chain.
pub enum Walk<'a> {
    Selected(Box<Selected<'a>>),
    /// §3.2 step 4 — nothing was eligible. §10.3 decides the reply, and its
    /// default is `451`.
    Exhausted,
}

/// §3.2 step 3, for real: walk and reserve.
///
/// `chain` is walked in the order given, which for a thread-affinity reply is
/// §3.2 step 2a's order with `pinned` first (`thread::order`). The pinned route
/// is treated differently in exactly three ways (D-090): §7.3's threshold is not
/// applied to it, though its events are still recorded on commit; D-091's
/// partial ramp is not applied to it, since a reply that changed identity on
/// the hash's say-so is what D-090 exists to prevent; and when it has no
/// headroom it is reserved **past the cap** rather than skipped. Pause,
/// strict preflight and a future `warmup.started` still eliminate it — those
/// say the route cannot send, not that it has sent enough.
///
/// `smtp::mod::handle`'s precedent on the argument count. The four collaborators
/// — store, frequency, preflight, and the evaluation buffer — are passed
/// explicitly rather than bundled because that is what lets a test drive the walk
/// with a real store and a synthetic registry, which is most of how §6.7 and §7.3
/// are tested at all. A `Deps` struct would move the same four values behind one
/// name and buy nothing.
#[allow(clippy::too_many_arguments)]
pub async fn walk_and_reserve<'a>(
    cfg: &'a Config,
    store: &Arc<dyn QuotaStore>,
    frequency: &Frequency,
    preflight: &crate::preflight::Registry,
    chain: &[String],
    pinned: Option<&str>,
    recipients: &[String],
    correlation_id: &str,
    evaluation: &mut Vec<Step>,
) -> Result<Walk<'a>, QuotaError> {
    let now = Utc::now();
    let states = store.route_states().await?;

    // §3.2: "Quota is decremented per message, by the recipient count, not per
    // recipient" — one reservation of this magnitude (O-7). D-047 makes that
    // count 1 for every message that arrives over SMTP; the arithmetic stays
    // because the walk is callable with any slice and the spec's rule is about
    // magnitude, not about how many recipients a transaction may hold.
    let count = recipients.len().max(1) as i64;

    // §3.2 step 2. One recipient per transaction (D-047), so there is one domain
    // group and no question of a transaction spanning two.
    let group = recipients
        .first()
        .and_then(|r| super::domain_group::resolve(cfg, r))
        .or_else(|| cfg.catchall_group())
        .map(|g| g.name.clone())
        .unwrap_or_else(|| "catchall".to_string());

    for name in chain {
        let Some(route) = cfg.route(name) else {
            record(evaluation, name, Err(SkipReason::Unknown));
            continue;
        };
        let state = states.get(name).copied().unwrap_or_default();
        let is_pinned = pinned == Some(name.as_str());

        // (a) paused.
        if state.paused {
            record(evaluation, name, Err(SkipReason::Paused));
            continue;
        }

        // (a2) §6.7 preflight, when `strict: true`. An in-memory read of the last
        // interval's result, so it sits above §7.3's indexed database read — the
        // cheapest check that can eliminate a route goes first. §3.2 3b calls
        // frequency "evaluated first", which this displaces by one position; the
        // two never disagree, because a route eliminated here is eliminated
        // whatever §7.3 would have said (D-065).
        //
        // Non-strict routes never reach `blocks`, and a route with no report
        // fails open — a slow resolver at boot must not empty a chain.
        if preflight.blocks(route) {
            record(evaluation, name, Err(SkipReason::Preflight));
            continue;
        }

        // (b) §7.3 recipient frequency. Evaluated first among the *eligibility*
        // checks per §3.2 3b — "Evaluated **first** — it can eliminate routes
        // outright" — hence its position above the quota check rather than below
        // it. Being over threshold makes this route ineligible and nothing more:
        // the message falls through to the next link, and a chain with no link
        // left is §10.3's `451`, never a drop.
        let recipient_keys = match &route.recipient_frequency {
            None => Vec::new(),
            Some(constraint) => {
                let keyer = frequency.keyer(store.as_ref()).await?;
                let keys: Vec<_> = recipients
                    .iter()
                    .map(|r| keyer.key_for(r, constraint.mode, &cfg.dot_insensitive_domains))
                    .collect();

                // D-090: a reply the recipient prompted by replying is not the
                // over-mailing §7.3 steers away from, so a pinned route skips
                // the threshold — and still records the event, so the window
                // stays true for the next message that is not a reply.
                let since = frequency::window_start(constraint, now);
                let mut over = false;
                for key in keys.iter().filter(|_| !is_pinned) {
                    let seen = store.recipient_event_count(name, key, since).await?;
                    if seen >= i64::from(constraint.threshold) {
                        // No recipient in the log line, and no recipient label on
                        // the metric: §7.3 hashes precisely so that the container
                        // does not accumulate a record of who was mailed, and a
                        // log line would be that record by another route.
                        tracing::debug!(
                            route = %name,
                            seen,
                            threshold = constraint.threshold,
                            window_start = %since.to_rfc3339(),
                            "route is over its recipient-frequency threshold"
                        );
                        over = true;
                        break;
                    }
                }

                if over {
                    record(evaluation, name, Err(SkipReason::Frequency));
                    continue;
                }
                keys
            }
        };

        let day_index = quota::day::for_route(route, now);
        let allowance = quota::allowance_for(route, &group, day_index, state);

        if allowance == Allowance::NotStarted {
            record(evaluation, name, Err(SkipReason::NotStarted));
            continue;
        }

        // (c′) D-091's partial ramp. After the start check because it needs the
        // day index, and before the reservation so that a message turned away
        // here never touches the row lock. A pinned reply is exempt.
        if let Some(share) = partial::share_today(route, day_index, state).filter(|_| !is_pinned) {
            let keyer = frequency.keyer(store.as_ref()).await?;
            let recipient = recipients.first().map(String::as_str).unwrap_or_default();
            if !partial::offered(
                keyer,
                name,
                recipient,
                day_index,
                share,
                &cfg.dot_insensitive_domains,
            ) {
                record(evaluation, name, Err(SkipReason::PartialRamp));
                continue;
            }
        }

        // (c) + (d) together, under one row lock.
        let request = ReserveRequest {
            route: name.clone(),
            domain_group: group.clone(),
            day_index,
            allowance: allowance.as_column(),
            count,
            correlation_id: correlation_id.to_string(),
            expires_at: now
                + chrono::Duration::from_std(quota::reservation_expiry(route, recipients.len()))
                    .unwrap_or_else(|_| chrono::Duration::seconds(600)),
            over_cap: false,
        };

        // D-090: the ordinary reservation first, even for a pinned route, so
        // that "past the cap" is known rather than assumed — it is what
        // `simmer_thread_affinity_total{outcome="over_cap"}` counts. Only a
        // refusal is retried, and the retry cannot be refused.
        let mut over_cap = false;
        let mut reserved = store.reserve(&request).await?;
        if is_pinned && matches!(reserved, Reserved::NoHeadroom { .. }) {
            over_cap = true;
            reserved = store
                .reserve(&ReserveRequest {
                    over_cap: true,
                    ..request
                })
                .await?;
        }

        match reserved {
            Reserved::Taken(reservation) => {
                if over_cap {
                    tracing::info!(
                        route = %name,
                        domain_group = %group,
                        day_index,
                        correlation_id,
                        "thread-affinity reply reserved past the day's cap (D-090)"
                    );
                }
                evaluation.push(Step {
                    route: name.to_string(),
                    outcome: Ok(()),
                    over_cap,
                });
                metrics::warmup_day(name, day_index);
                if let Allowance::Limited(a) = allowance {
                    metrics::quota_allowance(name, &group, a as f64);
                } else {
                    metrics::quota_allowance(name, &group, f64::INFINITY);
                }
                return Ok(Walk::Selected(Box::new(Selected {
                    route,
                    domain_group: group,
                    day_index,
                    reservation,
                    recipient_keys,
                    over_cap,
                })));
            }
            Reserved::NoHeadroom { usage } => {
                tracing::debug!(
                    route = %name,
                    domain_group = %group,
                    day_index,
                    allowance = ?usage.effective_allowance(),
                    committed = usage.committed,
                    reserved = usage.reserved,
                    "route has no headroom today"
                );
                record(evaluation, name, Err(SkipReason::Quota));
            }
        }
    }

    Ok(Walk::Exhausted)
}

/// §9.4 — walk a chain and report what *would* happen, reserving nothing.
///
/// The order of the checks below is `walk_and_reserve`'s order, deliberately and
/// fragilely: paused, then §6.7 preflight, then §7.3 frequency, then §7.2's start
/// instant, then D-091's partial ramp, then headroom. A dry run that evaluated them in a different order would report a
/// different reason for the same route, and the reason is the entire product —
/// "why did this message not go via the warming route" is the question the
/// endpoint exists to answer. `tests/admin_api.rs` asserts the two agree rather
/// than trusting this comment.
///
/// `pinned` is `walk_and_reserve`'s, with its three exceptions reproduced: no
/// §7.3 threshold, no partial ramp, and no headroom means selected past the cap
/// (D-090, D-091).
///
/// It stops at the first eligible route, as the real walk does, so the routes
/// after the selected one are absent rather than reported — they would not have
/// been consulted either.
///
/// The one thing it cannot reproduce is the race: the real walk checks headroom
/// *inside* the transaction that takes the row lock, and this reads outside any
/// transaction. So it can say "eligible" for a route that another session
/// empties a millisecond later. That is the same direction of error the §5.4
/// early check makes, and harmless for the same reason — nothing acts on it.
/// `walk_and_reserve`'s argument list, for the same reason (D-090 added `pinned`).
#[allow(clippy::too_many_arguments)]
pub async fn dry_walk(
    cfg: &Config,
    store: &Arc<dyn QuotaStore>,
    frequency: &Frequency,
    preflight: &crate::preflight::Registry,
    chain: &[String],
    pinned: Option<&str>,
    recipient: &str,
    now: chrono::DateTime<Utc>,
) -> Result<Vec<Step>, QuotaError> {
    let states = store.route_states().await?;
    let group = super::domain_group::resolve(cfg, recipient)
        .or_else(|| cfg.catchall_group())
        .map(|g| g.name.clone())
        .unwrap_or_else(|| "catchall".to_string());

    let mut evaluation = Vec::new();

    for name in chain {
        let Some(route) = cfg.route(name) else {
            evaluation.push(step(name, Err(SkipReason::Unknown)));
            continue;
        };
        let state = states.get(name).copied().unwrap_or_default();
        let is_pinned = pinned == Some(name.as_str());

        if state.paused {
            evaluation.push(step(name, Err(SkipReason::Paused)));
            continue;
        }

        // §6.7, in `walk_and_reserve`'s position. See that function's (a2).
        if preflight.blocks(route) {
            evaluation.push(step(name, Err(SkipReason::Preflight)));
            continue;
        }

        if let Some(constraint) = route.recipient_frequency.as_ref().filter(|_| !is_pinned) {
            let keyer = frequency.keyer(store.as_ref()).await?;
            let key = keyer.key_for(recipient, constraint.mode, &cfg.dot_insensitive_domains);
            let since = frequency::window_start(constraint, now);
            if store.recipient_event_count(name, &key, since).await?
                >= i64::from(constraint.threshold)
            {
                evaluation.push(step(name, Err(SkipReason::Frequency)));
                continue;
            }
        }

        let day_index = quota::day::for_route(route, now);
        let allowance = quota::allowance_for(route, &group, day_index, state);
        if allowance == Allowance::NotStarted {
            evaluation.push(step(name, Err(SkipReason::NotStarted)));
            continue;
        }

        // D-091, in `walk_and_reserve`'s position. The hash is the real walk's,
        // so this is its answer and not an estimate of it.
        if let Some(share) = partial::share_today(route, day_index, state).filter(|_| !is_pinned) {
            let keyer = frequency.keyer(store.as_ref()).await?;
            if !partial::offered(
                keyer,
                name,
                recipient,
                day_index,
                share,
                &cfg.dot_insensitive_domains,
            ) {
                evaluation.push(step(name, Err(SkipReason::PartialRamp)));
                continue;
            }
        }

        let usage = store.usage(name, &group, day_index).await?;
        // An absent row reads as all-zero, so a fresh day is eligible against the
        // schedule's ceiling — which is what the reservation would write.
        let effective = Usage {
            allowance: usage.allowance.or(allowance.as_column()),
            ..usage
        };
        if effective.has_headroom_for(1) {
            evaluation.push(step(name, Ok(())));
            return Ok(evaluation);
        }
        if is_pinned {
            evaluation.push(Step {
                over_cap: true,
                ..step(name, Ok(()))
            });
            return Ok(evaluation);
        }

        evaluation.push(step(name, Err(SkipReason::Quota)));
    }

    Ok(evaluation)
}

/// A [`Step`] with no metric increment.
///
/// `record` counts `simmer_route_skipped_total`, and a dry run must not: the
/// counter measures messages that were steered, and an operator testing a
/// configuration has steered nothing. Inflating it would corrupt exactly the
/// series someone would use to decide whether the ramp is working.
fn step(route: &str, outcome: Result<(), SkipReason>) -> Step {
    Step {
        route: route.to_string(),
        outcome,
        over_cap: false,
    }
}

/// The §5.4 early check: is anything in this chain plausibly eligible?
///
/// Read-only and takes no reservation, so it is safe to run at `RCPT TO` where
/// the recipient count is not yet final. §5.4: "When all rules use `envelope`,
/// Simmer should decide early and reject at `RCPT TO` to avoid a wasted body
/// transfer."
///
/// It can be wrong in one direction only — it may say "eligible" for a chain
/// that is exhausted by the time the body arrives, because another session took
/// the last slot in between. That is harmless: the authoritative check is the
/// reservation, and the message is refused at the final dot instead. It must
/// never be wrong the other way, which is why it asks for headroom of 1 rather
/// than for a guess at the eventual recipient count — and why it ignores D-091's
/// partial ramp: a route turned away by it always has a later link (§4.2), so
/// counting it eligible can only err in the harmless direction.
pub async fn any_eligible(
    cfg: &Config,
    store: &Arc<dyn QuotaStore>,
    chain: &[String],
    recipient: &str,
) -> Result<bool, QuotaError> {
    let now = Utc::now();
    let states = store.route_states().await?;
    let group = super::domain_group::resolve(cfg, recipient)
        .map(|g| g.name.clone())
        .unwrap_or_else(|| "catchall".to_string());

    for name in chain {
        let Some(route) = cfg.route(name) else {
            continue;
        };
        let state = states.get(name).copied().unwrap_or_default();
        if state.paused {
            continue;
        }

        let day_index = quota::day::for_route(route, now);
        match quota::allowance_for(route, &group, day_index, state) {
            Allowance::NotStarted => continue,
            Allowance::Unlimited => return Ok(true),
            Allowance::Limited(a) => {
                let usage = store.usage(name, &group, day_index).await?;
                // A row that does not exist yet reads as all-zero, so a fresh
                // day is eligible without a write.
                let effective = usage.effective_allowance().unwrap_or(a);
                if effective - usage.committed - usage.reserved >= 1 {
                    return Ok(true);
                }
            }
        }
    }

    Ok(false)
}

fn record(evaluation: &mut Vec<Step>, route: &str, outcome: Result<(), SkipReason>) {
    if let Err(reason) = outcome {
        metrics::route_skipped(route, reason.as_str());
    }
    evaluation.push(Step {
        route: route.to_string(),
        outcome,
        over_cap: false,
    });
}

/// Render a chain evaluation for §9.5's log line.
pub fn render(evaluation: &[Step]) -> String {
    evaluation
        .iter()
        .map(|s| match s.outcome {
            Ok(()) if s.over_cap => format!("{}=selected_over_cap", s.route),
            Ok(()) => format!("{}=selected", s.route),
            Err(r) => format!("{}={}", s.route, r.as_str()),
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_reasons_have_stable_metric_labels() {
        // These are Prometheus label values (§9.1). Renaming one silently splits
        // a time series, so they are pinned here rather than left to the enum.
        assert_eq!(SkipReason::Paused.as_str(), "paused");
        assert_eq!(SkipReason::Quota.as_str(), "quota");
        assert_eq!(SkipReason::Frequency.as_str(), "frequency");
        assert_eq!(SkipReason::Preflight.as_str(), "preflight");
        assert_eq!(SkipReason::NotStarted.as_str(), "not_started");
        assert_eq!(SkipReason::PartialRamp.as_str(), "partial_ramp");
    }

    #[test]
    fn renders_a_chain_evaluation_for_the_log() {
        let steps = vec![
            Step {
                route: "warming".into(),
                outcome: Err(SkipReason::Quota),
                over_cap: false,
            },
            Step {
                route: "overflow".into(),
                outcome: Ok(()),
                over_cap: false,
            },
        ];
        assert_eq!(render(&steps), "warming=quota,overflow=selected");
    }

    #[test]
    fn renders_an_exhausted_chain() {
        let steps = vec![
            Step {
                route: "a".into(),
                outcome: Err(SkipReason::Paused),
                over_cap: false,
            },
            Step {
                route: "b".into(),
                outcome: Err(SkipReason::NotStarted),
                over_cap: false,
            },
        ];
        assert_eq!(render(&steps), "a=paused,b=not_started");
    }
}
