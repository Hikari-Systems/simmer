//! §3.2 step 3 — walk the chain, and take the reservation.
//!
//! ```text
//! For each route:
//!   a. If the route is paused (admin API, §9.3), skip.
//!   b. If the route has a `recipient_frequency` constraint and this recipient is
//!      at or over threshold within the window, skip. Evaluated **first**.
//!   c′. If the route's `schedule.share` is below 1 today and this message is
//!      not in it, skip (D-091).
//!   c″. If the route has a `rate` and its next slot for this domain group is
//!      later than `now + max_wait`, skip (D-111). Otherwise the slot is booked.
//!   c. If the route is warming and has no remaining headroom for this domain
//!      group today, skip.
//!   d. Otherwise, attempt reservation (§7.4).
//!   e. First route to reserve successfully is selected.
//! ```
//!
//! The checks exist once, as `CHECKS`, and the real walk, §9.4's dry run and
//! §5.4's early check each run that list under a `Mode` (D-109).
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

use chrono::{DateTime, Utc};

use super::domain_group::Grouper;
use super::partial;
use crate::config::{Ramp, Route};
use crate::frequency::{self, Frequency};
use crate::metrics;
use crate::quota::{
    self,
    rate::{Rate, RateBooked},
    store::{QuotaError, QuotaStore, RateBookRequest, RateKey, ReserveRequest, Reserved, Usage},
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
    /// §3.2 step 3c″ (D-111) — the route's next sending slot for this domain
    /// group is later than `now + rate.max_wait`. Not `quota`: the route has
    /// headroom today, and is being paced.
    Rate,
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
            SkipReason::Rate => "rate",
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
    /// D-111 — the rate slot this route was (or, in a dry run, would be)
    /// given. `None` for a route with no `rate`, and on every skip but
    /// [`SkipReason::Rate`], where it carries the earliest slot.
    pub rate: Option<RateStep>,
}

/// D-111 — what the rate check decided for one route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateStep {
    /// When the message would go: `now` or up to `max_wait` after it. For a
    /// [`SkipReason::Rate`] skip, the earliest slot there was.
    pub send_at: DateTime<Utc>,
    /// `send_at − now`, never negative.
    pub wait: chrono::Duration,
    /// A thread-affinity reply's pinned route booked past its limit (D-113):
    /// counted against the bucket, and sent now.
    pub over_limit: bool,
}

/// D-111 — a booked rate slot, held by a [`Selected`]. Exactly as with the
/// reservation, holding one is an obligation: a message that is not sent gives
/// the slot back through [`unbook`].
#[derive(Debug, Clone)]
pub struct RateBooking {
    pub key: RateKey,
    pub rate: Rate,
    pub send_at: DateTime<Utc>,
    pub booked_tat: DateTime<Utc>,
    pub over_limit: bool,
}

/// Give a booked slot back, if nothing has been booked after it (D-111).
/// Best-effort: a failure leaves the bucket one interval conservative until it
/// drains, which is the safe direction, and is logged.
pub async fn unbook(store: &dyn QuotaStore, booking: &RateBooking, why: &str) {
    match store
        .unbook_rate_slot(&booking.key, booking.rate, booking.booked_tat)
        .await
    {
        Ok(true) => metrics::rate_slot_unbooked(&booking.key.ramp, &booking.key.route),
        Ok(false) => tracing::debug!(
            route = %booking.key.route,
            domain_group = %booking.key.domain_group,
            why,
            "rate slot not given back: a later slot was booked after it (D-111)"
        ),
        Err(e) => tracing::warn!(
            route = %booking.key.route,
            domain_group = %booking.key.domain_group,
            why,
            error = %e,
            "could not give a rate slot back; the bucket runs one interval conservative \
             until it drains (D-111)"
        ),
    }
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
    /// D-111 — the rate slot booked for this message, when the route has a
    /// `rate`. The relay waits for `send_at` before relaying, and gives the
    /// slot back on any outcome that does not commit.
    pub rate: Option<RateBooking>,
}

/// The result of walking a chain.
pub enum Walk<'a> {
    Selected(Box<Selected<'a>>),
    /// D-118 — spool mode only: a route with `on_limit: wait` booked a slot
    /// later than now but inside the message's hold. **No reservation was
    /// taken**; the dispatcher stores the booking and comes back at `until`,
    /// when the deferred attempt uses this slot rather than booking another.
    Deferred {
        route: &'a Route,
        domain_group: String,
        booking: RateBooking,
        until: DateTime<Utc>,
    },
    /// §3.2 step 4 — nothing was eligible. §10.3 decides the reply, and its
    /// default is `451`.
    Exhausted,
}

/// D-118 — what a spooled attempt adds to [`Mode::Reserve`].
#[derive(Debug, Clone, Copy)]
pub struct SpoolWalk<'s> {
    /// The end of the message's hold. A waiting route defers to a slot at or
    /// before this, and steers past it.
    pub hold_until: DateTime<Utc>,
    /// The slot an earlier attempt booked, which this one uses instead of
    /// booking again if the walk reaches that route in that group.
    pub booked: Option<&'s crate::spool::BookedSlot>,
}

// ---------------------------------------------------------------------------
// The one walk (D-109)
// ---------------------------------------------------------------------------
//
// §3.2 step 3's eligibility checks exist exactly once, as `CHECKS`, and all three
// callers — the real walk, §9.4's dry run and §5.4's early check — run that list
// under a `Mode`. Before D-109 each had its own copy of the loop, kept in step by
// a comment and by `tests/admin_api.rs`'s agreement test; the test remains, and
// now checks a single implementation against itself through two modes.

/// The collaborators the §6.7, §7.3 and D-091 checks need.
///
/// [`Mode::Early`] has none, which is how those three checks are omitted from
/// it *by construction* rather than by a branch someone could forget.
#[derive(Clone, Copy)]
struct Deps<'d> {
    dot_insensitive_domains: &'d [String],
    frequency: &'d Frequency,
    preflight: &'d crate::preflight::Registry,
}

/// How a walk is run. Every mode applies [`CHECKS`] in [`CHECKS`]'s order; a
/// mode decides only which checks it can run (those needing [`Deps`]), what it
/// does once a route passes them, and whether a skip is counted.
#[derive(Clone, Copy)]
enum Mode<'m> {
    /// §3.2 step 3 for real. A route that passes is reserved (§7.4 phase 1),
    /// under the row lock, and the first reservation ends the walk. Every skip
    /// increments `simmer_route_skipped_total`. Keeps §7.3's keys for commit.
    Reserve {
        deps: Deps<'m>,
        correlation_id: &'m str,
        /// D-118 — set by the spool's dispatcher, never by a session.
        spool: Option<SpoolWalk<'m>>,
    },
    /// §9.4. Reads what `Reserve` reads, in the same order, plus the day's row
    /// unconditionally (it reports headroom without taking the lock). Reserves
    /// nothing and **counts nothing**: `simmer_route_skipped_total` measures
    /// steered messages, and an operator's dry run steered none. It also never
    /// asks for the §7.3 keyer on behalf of a pinned route, since it neither
    /// tests nor keeps that route's keys.
    DryRun { deps: Deps<'m> },
    /// §5.4's early check at `RCPT TO`. Deliberately omits three checks, each in
    /// the harmless direction — it may say "eligible" for a chain the final dot
    /// finds exhausted, never the reverse:
    /// - §6.7 preflight and §7.3 frequency: it has no `Deps`, so cannot run them;
    /// - D-091's partial ramp: a route it turns away always has a later link
    ///   (§4.2), so counting it eligible only errs in the harmless direction;
    /// - and an **unlimited** route is eligible without reading its row;
    /// - D-111's rate: §4.2 refuses a rate-limited route last in a chain, so a
    ///   route it would turn away always has a later link, and counting it
    ///   eligible errs only in the harmless direction. Reading the bucket here
    ///   would also be a guess: the slot free at `RCPT TO` is not the one the
    ///   final dot will be offered.
    ///
    /// Counts nothing, and is never given a pinned route.
    Early,
}

impl<'m> Mode<'m> {
    fn deps(&self) -> Option<&Deps<'m>> {
        match self {
            Mode::Reserve { deps, .. } | Mode::DryRun { deps } => Some(deps),
            Mode::Early => None,
        }
    }

    fn counts_skips(&self) -> bool {
        matches!(self, Mode::Reserve { .. })
    }
}

/// One §3.2 step 3 eligibility check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    /// (a) `POST /routes/{name}/pause`.
    Paused,
    /// (a2) §6.7 with `strict: true`. An in-memory read of the last interval's
    /// result, so it sits above §7.3's indexed database read — the cheapest
    /// check that can eliminate a route goes first. §3.2 3b calls frequency
    /// "evaluated first", which this displaces by one position; the two never
    /// disagree, because a route eliminated here is eliminated whatever §7.3
    /// would have said (D-065). Non-strict routes never reach `blocks`, and a
    /// route with no report fails open (D-064).
    Preflight,
    /// (b) §7.3. First among the *eligibility* checks per §3.2 3b — "it can
    /// eliminate routes outright" — hence above the quota check. Over threshold
    /// makes the route ineligible and nothing more: the message falls through,
    /// and a chain with nothing left is §10.3's `451`, never a drop.
    Frequency,
    /// §7.2 — `warmup.started` is in the future.
    Started,
    /// (c′) D-091's partial ramp and D-097's computed share. After the start
    /// check because it needs the day index, and before the reservation so a
    /// message turned away here never touches the row lock.
    PartialRamp,
    /// (c″) D-111's per-segment rate (D-112). Last, because under `Reserve` it
    /// **writes**: it books a slot, and every check before it is read-only, so
    /// a route they eliminate never touches the rate row. Only the headroom
    /// step follows, and a refusal there gives the slot back. `Early` ignores
    /// it (see [`Mode::Early`]).
    Rate,
}

/// The order is the product: a dry run that evaluated these differently would
/// report a different reason for the same route, and "why did this not go via
/// the warming route" is the question §9.4 exists to answer. Headroom — (c) and
/// (d), one operation under the row lock for `Reserve` — follows the list.
const CHECKS: [Check; 6] = [
    Check::Paused,
    Check::Preflight,
    Check::Frequency,
    Check::Started,
    Check::PartialRamp,
    Check::Rate,
];

/// What every check reads, fixed for one walk. `'w` is the ramp's lifetime —
/// the one a [`Selected`] borrows — and `'m` everything else's.
struct Walker<'w, 'm> {
    mode: Mode<'m>,
    ramp: &'w Ramp,
    store: &'m Arc<dyn QuotaStore>,
    recipients: &'m [String],
    states: std::collections::HashMap<String, quota::store::RouteState>,
    group: String,
    now: DateTime<Utc>,
}

impl<'w, 'm> Walker<'w, 'm> {
    /// The route states, then §3.2 step 2's group — that order, as every walk
    /// has always read them: a store failure costs no DNS lookup.
    async fn new(
        mode: Mode<'m>,
        ramp: &'w Ramp,
        groups: &Grouper,
        store: &'m Arc<dyn QuotaStore>,
        recipients: &'m [String],
        now: DateTime<Utc>,
    ) -> Result<Walker<'w, 'm>, QuotaError> {
        let states = store.route_states(&ramp.name).await?;
        // One recipient per transaction (D-047), so there is one domain group
        // and no question of a transaction spanning two.
        let group = match recipients.first() {
            Some(r) => groups.group_name(ramp, r).await,
            None => ramp
                .catchall_group()
                .map(|g| g.name.clone())
                .unwrap_or_else(|| "catchall".to_string()),
        };
        Ok(Walker {
            mode,
            ramp,
            store,
            recipients,
            states,
            group,
            now,
        })
    }
}

/// One route as the checks see it.
struct Candidate<'r> {
    route: &'r Route,
    state: quota::store::RouteState,
    /// §3.2 step 2a (D-090): exempt from §7.3's threshold and D-091's share, and
    /// reserved past the cap rather than skipped. Pause, strict preflight and a
    /// future start still eliminate it — they say the route cannot send, not
    /// that it has sent enough.
    pinned: bool,
    day_index: i64,
    allowance: Allowance,
    /// The day's row, when a check has read it.
    usage: Option<Usage>,
    /// §7.3's keys, kept by `Reserve` for commit. Empty for a route with no
    /// `recipient_frequency`: keys are mode-specific to the route.
    recipient_keys: Vec<frequency::Key>,
    /// D-111 — the slot `Reserve` booked, to be held by the [`Selected`] or
    /// given back.
    booking: Option<RateBooking>,
    /// D-111 — the slot this route was or would be given, for its [`Step`].
    rate_step: Option<RateStep>,
}

/// How a walk ended.
enum Ended<'a> {
    Reserved(Box<Selected<'a>>),
    Deferred {
        route: &'a Route,
        booking: RateBooking,
        until: DateTime<Utc>,
    },
    /// `DryRun` and `Early`: a route passed, nothing was taken.
    Eligible,
    Exhausted,
}

impl<'w, 'm> Walker<'w, 'm> {
    async fn run(
        &self,
        chain: &[String],
        pinned: Option<&str>,
        evaluation: &mut Vec<Step>,
    ) -> Result<Ended<'w>, QuotaError> {
        'routes: for name in chain {
            let Some(route) = self.ramp.route(name) else {
                self.skip(evaluation, name, SkipReason::Unknown);
                continue;
            };
            let state = self.states.get(name).copied().unwrap_or_default();
            // Both pure: computing them before the checks that use them changes
            // nothing a caller can observe.
            let day_index = quota::day::for_route(route, self.now);
            let mut candidate = Candidate {
                route,
                state,
                pinned: pinned == Some(name.as_str()),
                day_index,
                allowance: quota::allowance_for(route, &self.group, day_index, state),
                usage: None,
                recipient_keys: Vec::new(),
                booking: None,
                rate_step: None,
            };

            for check in CHECKS {
                if let Some(reason) = self.apply(check, &mut candidate).await? {
                    let rate = candidate.rate_step.filter(|_| reason == SkipReason::Rate);
                    self.skip_with(evaluation, name, reason, rate);
                    continue 'routes;
                }
            }

            // D-118: a spooled message whose slot is later than now is deferred
            // to it, holding the slot and no reservation.
            if let Some(booking) = candidate
                .booking
                .as_ref()
                .filter(|b| self.defers(candidate.route) && b.send_at > self.now)
            {
                evaluation.push(Step {
                    rate: candidate.rate_step,
                    ..step(name, Ok(()))
                });
                return Ok(Ended::Deferred {
                    route: candidate.route,
                    until: booking.send_at,
                    booking: booking.clone(),
                });
            }

            match self.headroom(candidate, evaluation).await? {
                Ended::Exhausted => continue,
                ended => return Ok(ended),
            }
        }

        Ok(Ended::Exhausted)
    }

    /// Run one check. `Some` is the reason the route is skipped.
    async fn apply(
        &self,
        check: Check,
        c: &mut Candidate<'w>,
    ) -> Result<Option<SkipReason>, QuotaError> {
        let route = c.route;
        let name = &route.name;
        match check {
            Check::Paused => Ok(c.state.paused.then_some(SkipReason::Paused)),

            Check::Preflight => Ok(self
                .mode
                .deps()
                .is_some_and(|d| d.preflight.blocks(route))
                .then_some(SkipReason::Preflight)),

            Check::Frequency => {
                let (Some(deps), Some(constraint)) =
                    (self.mode.deps(), route.recipient_frequency.as_ref())
                else {
                    return Ok(None);
                };
                let keeps_keys = matches!(self.mode, Mode::Reserve { .. });
                if c.pinned && !keeps_keys {
                    return Ok(None);
                }
                let keyer = deps.frequency.keyer(self.store.as_ref()).await?;
                let keys: Vec<_> = self
                    .recipients
                    .iter()
                    .map(|r| keyer.key_for(r, constraint.mode, deps.dot_insensitive_domains))
                    .collect();

                // D-090: a reply the recipient prompted by replying is not the
                // over-mailing §7.3 steers away from, so a pinned route skips the
                // threshold — and still records the event on commit, so the
                // window stays true for the next message that is not a reply.
                let since = frequency::window_start(constraint, self.now);
                let mut over = false;
                for key in keys.iter().filter(|_| !c.pinned) {
                    let seen = self
                        .store
                        .recipient_event_count(&self.ramp.name, name, key, since)
                        .await?;
                    if seen >= i64::from(constraint.threshold) {
                        // No recipient in the log line, and no recipient label on
                        // the metric: §7.3 hashes precisely so that the container
                        // does not accumulate a record of who was mailed.
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
                if keeps_keys {
                    c.recipient_keys = keys;
                }
                Ok(over.then_some(SkipReason::Frequency))
            }

            Check::Started => {
                Ok((c.allowance == Allowance::NotStarted).then_some(SkipReason::NotStarted))
            }

            Check::PartialRamp => {
                let Some(deps) = self.mode.deps() else {
                    return Ok(None);
                };
                // D-097 paces against how full the day's cap is, so the row is
                // read here — without the lock, since nothing here writes. The
                // real walk reads it only when the share needs it; the dry run
                // reads it regardless, since it reports headroom without
                // reserving, pinned or not.
                let read = match self.mode {
                    Mode::DryRun { .. } => true,
                    Mode::Reserve { .. } => !c.pinned && partial::needs_usage(route, c.state),
                    Mode::Early => false,
                };
                if read {
                    c.usage = Some(
                        self.store
                            .usage(&self.ramp.name, name, &self.group, c.day_index)
                            .await?,
                    );
                }
                // A pinned reply is exempt: a reply that changed identity on the
                // hash's say-so is what D-090 exists to prevent.
                if c.pinned {
                    return Ok(None);
                }
                let Some(share) = partial::share_for_group(
                    route,
                    c.day_index,
                    c.state,
                    self.now,
                    c.allowance,
                    c.usage.as_ref(),
                ) else {
                    return Ok(None);
                };
                let keyer = deps.frequency.keyer(self.store.as_ref()).await?;
                let recipient = self
                    .recipients
                    .first()
                    .map(String::as_str)
                    .unwrap_or_default();
                Ok((!partial::offered(
                    keyer,
                    &self.ramp.name,
                    name,
                    recipient,
                    c.day_index,
                    share,
                    deps.dot_insensitive_domains,
                ))
                .then_some(SkipReason::PartialRamp))
            }

            Check::Rate => self.rate(c).await,
        }
    }

    /// (c″) D-111. `Reserve` books under the row lock; `DryRun` reads the
    /// bucket and computes the same decision without writing; `Early` skips it.
    ///
    /// A pinned reply (D-090, D-113) is never skipped here: it books past the limit,
    /// sending now and counted, the way `over_cap` reserves past the cap.
    async fn rate(&self, c: &mut Candidate<'w>) -> Result<Option<SkipReason>, QuotaError> {
        let Some(limit) = c.route.rate.as_ref() else {
            return Ok(None);
        };
        let Some(rate) = quota::rate::rate_for(c.route, &self.group, c.day_index, c.state) else {
            return Ok(None);
        };
        let max_wait = chrono::Duration::from_std(limit.max_wait())
            .unwrap_or_else(|_| chrono::Duration::zero());
        let key = RateKey {
            ramp: self.ramp.name.clone(),
            route: c.route.name.clone(),
            domain_group: self.group.clone(),
        };

        // D-118: an earlier attempt booked this slot; use it. The message is
        // due, so it sends now.
        if let Some(slot) = self
            .spool()
            .and_then(|s| s.booked)
            .filter(|b| b.route == key.route && b.domain_group == key.domain_group)
        {
            c.rate_step = Some(RateStep {
                send_at: self.now,
                wait: chrono::Duration::zero(),
                over_limit: false,
            });
            c.booking = Some(RateBooking {
                key,
                rate,
                send_at: self.now,
                booked_tat: slot.tat,
                over_limit: false,
            });
            return Ok(None);
        }
        // D-118: a waiting route on a spooled message may book as far ahead as
        // the message's hold.
        let max_wait = match self.spool() {
            Some(s) if self.defers(c.route) => (s.hold_until - self.now).max(max_wait),
            _ => max_wait,
        };

        let outcome = match self.mode {
            Mode::Early => return Ok(None),
            Mode::DryRun { .. } => {
                let tat = self
                    .store
                    .rate_tats(&self.ramp.name)
                    .await?
                    .get(&(key.route.clone(), key.domain_group.clone()))
                    .copied();
                quota::rate::decide(tat, rate, self.now, max_wait, c.pinned)
            }
            Mode::Reserve { .. } => {
                self.store
                    .book_rate_slot(&RateBookRequest {
                        key: key.clone(),
                        rate,
                        now: self.now,
                        max_wait,
                        force: c.pinned,
                    })
                    .await?
            }
        };

        match outcome {
            RateBooked::Booked {
                send_at,
                booked_tat,
                over_limit,
            } => {
                c.rate_step = Some(RateStep {
                    send_at,
                    wait: (send_at - self.now).max(chrono::Duration::zero()),
                    over_limit,
                });
                if matches!(self.mode, Mode::Reserve { .. }) {
                    c.booking = Some(RateBooking {
                        key,
                        rate,
                        send_at,
                        booked_tat,
                        over_limit,
                    });
                }
                Ok(None)
            }
            RateBooked::TooLate { earliest } => {
                tracing::debug!(
                    route = %c.route.name,
                    domain_group = %self.group,
                    earliest = %earliest.to_rfc3339(),
                    "route's next rate slot is past max_wait (D-111)"
                );
                c.rate_step = Some(RateStep {
                    send_at: earliest,
                    wait: (earliest - self.now).max(chrono::Duration::zero()),
                    over_limit: false,
                });
                Ok(Some(SkipReason::Rate))
            }
        }
    }

    /// (c) and (d): headroom, and for `Reserve` the reservation, as one step.
    async fn headroom(
        &self,
        c: Candidate<'w>,
        evaluation: &mut Vec<Step>,
    ) -> Result<Ended<'w>, QuotaError> {
        let name = &c.route.name;
        let correlation_id = match self.mode {
            Mode::Reserve { correlation_id, .. } => correlation_id,
            Mode::DryRun { .. } | Mode::Early => {
                // `Early`'s fourth omission — see `Mode::Early`.
                if matches!(self.mode, Mode::Early) && c.allowance == Allowance::Unlimited {
                    evaluation.push(step(name, Ok(())));
                    return Ok(Ended::Eligible);
                }
                let usage = match c.usage {
                    Some(u) => u,
                    None => {
                        self.store
                            .usage(&self.ramp.name, name, &self.group, c.day_index)
                            .await?
                    }
                };
                // An absent row reads as all-zero, so a fresh day is eligible
                // against the schedule's ceiling — what a reservation would write.
                let effective = Usage {
                    allowance: usage.allowance.or(c.allowance.as_column()),
                    ..usage
                };
                if effective.has_headroom_for(1) {
                    evaluation.push(Step {
                        rate: c.rate_step,
                        ..step(name, Ok(()))
                    });
                    return Ok(Ended::Eligible);
                }
                if c.pinned {
                    evaluation.push(Step {
                        over_cap: true,
                        rate: c.rate_step,
                        ..step(name, Ok(()))
                    });
                    return Ok(Ended::Eligible);
                }
                self.skip(evaluation, name, SkipReason::Quota);
                return Ok(Ended::Exhausted);
            }
        };

        // §3.2: "Quota is decremented per message, by the recipient count, not
        // per recipient" — one reservation of this magnitude (O-7). D-047 makes
        // that count 1 for every message that arrives over SMTP; the arithmetic
        // stays because the walk is callable with any slice.
        let count = self.recipients.len().max(1) as i64;
        let request = ReserveRequest {
            ramp: self.ramp.name.clone(),
            route: name.clone(),
            domain_group: self.group.clone(),
            day_index: c.day_index,
            allowance: c.allowance.as_column(),
            count,
            correlation_id: correlation_id.to_string(),
            // D-111: the relay may hold the reservation for up to `max_wait`
            // before the downstream conversation starts, so the expiry covers
            // that too — or the sweeper could release a send still waiting.
            expires_at: self.now
                + chrono::Duration::from_std(
                    quota::reservation_expiry(c.route, self.recipients.len())
                        + c.route
                            .rate
                            .as_ref()
                            .map(|r| r.max_wait())
                            .unwrap_or_default(),
                )
                .unwrap_or_else(|_| chrono::Duration::seconds(600)),
            over_cap: false,
        };

        // D-090: the ordinary reservation first, even for a pinned route, so
        // that "past the cap" is known rather than assumed — it is what
        // `simmer_thread_affinity_total{outcome="over_cap"}` counts. Only a
        // refusal is retried, and the retry cannot be refused.
        let mut over_cap = false;
        let mut reserved = self.store.reserve(&request).await;
        if c.pinned && matches!(reserved, Ok(Reserved::NoHeadroom { .. })) {
            over_cap = true;
            reserved = self
                .store
                .reserve(&ReserveRequest {
                    over_cap: true,
                    ..request
                })
                .await;
        }
        // D-111: a storage failure after the slot was booked gives it back
        // before §7.5 decides the reply.
        let reserved = match reserved {
            Ok(r) => r,
            Err(e) => {
                if let Some(b) = &c.booking {
                    unbook(self.store.as_ref(), b, "reservation error").await;
                }
                return Err(e);
            }
        };

        match reserved {
            Reserved::Taken(reservation) => {
                if over_cap {
                    tracing::info!(
                        route = %name,
                        domain_group = %self.group,
                        day_index = c.day_index,
                        correlation_id,
                        "thread-affinity reply reserved past the day's cap (D-090)"
                    );
                }
                if c.booking.as_ref().is_some_and(|b| b.over_limit) {
                    tracing::info!(
                        route = %name,
                        domain_group = %self.group,
                        correlation_id,
                        "thread-affinity reply booked a rate slot past the limit (D-111)"
                    );
                }
                evaluation.push(Step {
                    route: name.to_string(),
                    outcome: Ok(()),
                    over_cap,
                    rate: c.rate_step,
                });
                metrics::warmup_day(&self.ramp.name, name, c.day_index);
                if let Allowance::Limited(a) = c.allowance {
                    metrics::quota_allowance(&self.ramp.name, name, &self.group, a as f64);
                } else {
                    metrics::quota_allowance(&self.ramp.name, name, &self.group, f64::INFINITY);
                }
                Ok(Ended::Reserved(Box::new(Selected {
                    route: c.route,
                    domain_group: self.group.clone(),
                    day_index: c.day_index,
                    reservation,
                    recipient_keys: c.recipient_keys,
                    over_cap,
                    rate: c.booking,
                })))
            }
            Reserved::NoHeadroom { usage } => {
                tracing::debug!(
                    route = %name,
                    domain_group = %self.group,
                    day_index = c.day_index,
                    allowance = ?usage.effective_allowance(),
                    committed = usage.committed,
                    reserved = usage.reserved,
                    "route has no headroom today"
                );
                // D-111: the slot was booked for a message this route will not
                // send. Give it back, if nothing was booked after it.
                if let Some(b) = &c.booking {
                    unbook(self.store.as_ref(), b, "no headroom").await;
                }
                self.skip(evaluation, name, SkipReason::Quota);
                Ok(Ended::Exhausted)
            }
        }
    }

    fn spool(&self) -> Option<SpoolWalk<'m>> {
        match self.mode {
            Mode::Reserve { spool, .. } => spool,
            _ => None,
        }
    }

    /// D-118: whether this route defers rather than steers in this walk.
    fn defers(&self, route: &Route) -> bool {
        self.spool().is_some()
            && route
                .rate
                .as_ref()
                .is_some_and(|r| r.on_limit == crate::config::OnLimit::Wait)
    }

    /// Record a skipped route, counting it only for a real walk.
    fn skip(&self, evaluation: &mut Vec<Step>, route: &str, reason: SkipReason) {
        self.skip_with(evaluation, route, reason, None);
    }

    /// [`Self::skip`], carrying a [`SkipReason::Rate`] skip's earliest slot.
    fn skip_with(
        &self,
        evaluation: &mut Vec<Step>,
        route: &str,
        reason: SkipReason,
        rate: Option<RateStep>,
    ) {
        if self.mode.counts_skips() {
            metrics::route_skipped(&self.ramp.name, route, reason.as_str());
        }
        evaluation.push(Step {
            rate,
            ..step(route, Err(reason))
        });
    }
}

/// §3.2 step 3, for real: walk and reserve.
///
/// `chain` is walked in the order given, which for a thread-affinity reply is
/// §3.2 step 2a's order with `pinned` first (`thread::order`). The pinned route
/// is treated differently in exactly three ways (D-090): §7.3's threshold is not
/// applied to it, though its events are still recorded on commit; D-091's
/// partial ramp is not applied to it; and when it has no headroom it is reserved
/// **past the cap** rather than skipped. See [`Mode::Reserve`] and [`CHECKS`].
///
/// `now` is the caller's single clock read (D-108): the day index, §7.3's window,
/// D-097's pacing and the reservation's expiry all use it, and so does the
/// caller's rewrite, so one message is evaluated at one instant.
///
/// `smtp::mod::handle`'s precedent on the argument count. The collaborators are
/// passed explicitly because that is what lets a test drive the walk with a real
/// store and a synthetic registry, which is most of how §6.7 and §7.3 are tested.
#[allow(clippy::too_many_arguments)]
pub async fn walk_and_reserve<'a>(
    ramp: &'a Ramp,
    groups: &Grouper,
    dot_insensitive_domains: &[String],
    store: &Arc<dyn QuotaStore>,
    frequency: &Frequency,
    preflight: &crate::preflight::Registry,
    chain: &[String],
    pinned: Option<&str>,
    recipients: &[String],
    correlation_id: &str,
    evaluation: &mut Vec<Step>,
    now: DateTime<Utc>,
) -> Result<Walk<'a>, QuotaError> {
    let walker = Walker::new(
        Mode::Reserve {
            deps: Deps {
                dot_insensitive_domains,
                frequency,
                preflight,
            },
            correlation_id,
            spool: None,
        },
        ramp,
        groups,
        store,
        recipients,
        now,
    )
    .await?;
    Ok(into_walk(
        walker.run(chain, pinned, evaluation).await?,
        &walker,
    ))
}

fn into_walk<'w>(ended: Ended<'w>, walker: &Walker<'w, '_>) -> Walk<'w> {
    match ended {
        Ended::Reserved(s) => Walk::Selected(s),
        Ended::Deferred {
            route,
            booking,
            until,
        } => Walk::Deferred {
            route,
            domain_group: walker.group.clone(),
            booking,
            until,
        },
        Ended::Eligible | Ended::Exhausted => Walk::Exhausted,
    }
}

/// [`walk_and_reserve`] for a spooled attempt (D-118): a waiting route books a
/// slot up to `spool.hold_until` ahead and defers to it, and a slot an earlier
/// attempt booked is used rather than booked again.
#[allow(clippy::too_many_arguments)]
pub async fn walk_and_reserve_spooled<'a>(
    ramp: &'a Ramp,
    groups: &Grouper,
    dot_insensitive_domains: &[String],
    store: &Arc<dyn QuotaStore>,
    frequency: &Frequency,
    preflight: &crate::preflight::Registry,
    chain: &[String],
    pinned: Option<&str>,
    recipients: &[String],
    correlation_id: &str,
    evaluation: &mut Vec<Step>,
    now: DateTime<Utc>,
    spool: SpoolWalk<'_>,
) -> Result<Walk<'a>, QuotaError> {
    let walker = Walker::new(
        Mode::Reserve {
            deps: Deps {
                dot_insensitive_domains,
                frequency,
                preflight,
            },
            correlation_id,
            spool: Some(spool),
        },
        ramp,
        groups,
        store,
        recipients,
        now,
    )
    .await?;
    Ok(into_walk(
        walker.run(chain, pinned, evaluation).await?,
        &walker,
    ))
}

/// §9.4 — walk a chain and report what *would* happen, reserving nothing.
///
/// The same [`CHECKS`] as [`walk_and_reserve`], in the same order, under
/// [`Mode::DryRun`] — so the same reason for the same route, which is the entire
/// product. `tests/admin_api.rs` still asserts the two agree.
///
/// It stops at the first eligible route, as the real walk does, so the routes
/// after the selected one are absent rather than reported.
///
/// The one thing it cannot reproduce is the race: the real walk checks headroom
/// *inside* the transaction that takes the row lock, and this reads outside any
/// transaction. So it can say "eligible" for a route that another session
/// empties a millisecond later — the same direction of error the §5.4 early
/// check makes, and harmless for the same reason: nothing acts on it.
#[allow(clippy::too_many_arguments)]
pub async fn dry_walk(
    ramp: &Ramp,
    groups: &Grouper,
    dot_insensitive_domains: &[String],
    store: &Arc<dyn QuotaStore>,
    frequency: &Frequency,
    preflight: &crate::preflight::Registry,
    chain: &[String],
    pinned: Option<&str>,
    recipient: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Step>, QuotaError> {
    let recipients = [recipient.to_string()];
    let walker = Walker::new(
        Mode::DryRun {
            deps: Deps {
                dot_insensitive_domains,
                frequency,
                preflight,
            },
        },
        ramp,
        groups,
        store,
        &recipients,
        now,
    )
    .await?;
    let mut evaluation = Vec::new();
    walker.run(chain, pinned, &mut evaluation).await?;
    Ok(evaluation)
}

/// A [`Step`] with no metric increment.
///
/// `Walker::skip` counts `simmer_route_skipped_total` only for a real walk: the
/// counter measures messages that were steered, and an operator testing a
/// configuration has steered nothing. Inflating it would corrupt exactly the
/// series someone would use to decide whether the ramp is working.
fn step(route: &str, outcome: Result<(), SkipReason>) -> Step {
    Step {
        route: route.to_string(),
        outcome,
        over_cap: false,
        rate: None,
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
/// that is exhausted by the time the body arrives. That is harmless: the
/// authoritative check is the reservation. It must never be wrong the other
/// way, which is why it asks for headroom of 1, and why [`Mode::Early`] omits
/// exactly the checks whose omission errs in the harmless direction.
pub async fn any_eligible(
    ramp: &Ramp,
    groups: &Grouper,
    store: &Arc<dyn QuotaStore>,
    chain: &[String],
    recipient: &str,
    now: DateTime<Utc>,
) -> Result<bool, QuotaError> {
    let recipients = [recipient.to_string()];
    let walker = Walker::new(Mode::Early, ramp, groups, store, &recipients, now).await?;
    let mut evaluation = Vec::new();
    Ok(matches!(
        walker.run(chain, None, &mut evaluation).await?,
        Ended::Eligible
    ))
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
        assert_eq!(SkipReason::Rate.as_str(), "rate");
    }

    #[test]
    fn the_checks_run_in_section_3_2_order() {
        // D-109: the one list every mode walks. Reordering it changes the reason
        // the real walk counts and the dry run reports for the same route, so a
        // change here is a decision, not a refactor (D-065 placed preflight).
        assert_eq!(
            CHECKS,
            [
                Check::Paused,
                Check::Preflight,
                Check::Frequency,
                Check::Started,
                Check::PartialRamp,
                Check::Rate,
            ]
        );
    }

    #[test]
    fn only_a_real_walk_counts_skips_and_the_early_check_has_no_deps() {
        assert!(!Mode::Early.counts_skips());
        assert!(Mode::Early.deps().is_none());
    }

    #[test]
    fn renders_a_chain_evaluation_for_the_log() {
        let steps = vec![
            Step {
                route: "warming".into(),
                outcome: Err(SkipReason::Quota),
                over_cap: false,
                rate: None,
            },
            Step {
                route: "overflow".into(),
                outcome: Ok(()),
                over_cap: false,
                rate: None,
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
                rate: None,
            },
            Step {
                route: "b".into(),
                outcome: Err(SkipReason::NotStarted),
                over_cap: false,
                rate: None,
            },
        ];
        assert_eq!(render(&steps), "a=paused,b=not_started");
    }
}
