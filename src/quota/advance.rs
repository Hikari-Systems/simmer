//! §7.2 (D-115) — which schedule entry an element is on.
//!
//! An *element* is anything that walks a warm-up schedule: a `(ramp, route,
//! domain_group)` quota row, or (D-116) a ramp group's row. Each has two
//! numbers that were one before D-115:
//!
//! - the **day index**, elapsed 24h periods from its start instant
//!   ([`super::day`]). It keys the row and sets the day boundary, unchanged.
//! - the **step**, the position in the schedule. It picks the allowance.
//!
//! Under [`Advance::Auto`] the step *is* the day index — §7.2 exactly as it was.
//! Under [`Advance::Threshold`] the step moves on by one only after a day whose
//! committed volume reached `threshold` of that day's **planned** allowance:
//!
//! ```text
//! P        = the latest row before today
//! required = ceil(threshold × P.allowance)
//! step     = min(today, P.step + (P.committed >= required ? 1 : 0))
//! ```
//!
//! A day with no traffic has no row, so the next day's decision rests on the
//! last day that had one — and "nothing sent" never advances anything.
//!
//! Pure: no clock, no store. [`super::position`] reads the two rows this needs
//! and calls [`decide`]; every caller — the walk, the dry run, the early check,
//! `/routes`, the gauges — goes through that one function, so they cannot
//! disagree about where an element is.

use crate::config::Advance;

/// The part of a stored row a decision reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub day_index: i64,
    /// The step the row was created at. `None` on a row written before D-115's
    /// migration that it did not backfill (an overflow row, or a negative
    /// index): read as the day index, which is what it was then.
    pub step: Option<i64>,
    /// The **planned** allowance — the schedule's value, written when the row
    /// was created. Never §9.3's override (D-115): a day an operator throttled
    /// cannot count as meeting the plan. `None` is no ceiling.
    pub allowance: Option<i64>,
    pub committed: i64,
}

impl Row {
    /// The step, reading a legacy null as the day index.
    pub fn step_or_day(&self) -> i64 {
        self.step.unwrap_or(self.day_index)
    }
}

/// What [`decide`] concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The day index is negative: §7.2's future start. Nothing to decide.
    NotStarted,
    /// `Auto`: the step is the day index.
    Auto,
    /// No earlier row: the first day this element has ever been reserved on.
    First,
    /// The previous row met its threshold.
    Advanced,
    /// It did not.
    Held,
    /// Today's row already exists with a step, which is authoritative (the
    /// D-026 pattern): it was decided when that row was created.
    Existing,
}

impl Outcome {
    /// The `outcome` label of `simmer_warmup_advance_total`.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::NotStarted => "not_started",
            Outcome::Auto => "auto",
            Outcome::First => "first",
            Outcome::Advanced => "advanced",
            Outcome::Held => "held",
            Outcome::Existing => "existing",
        }
    }
}

/// What a threshold decision rested on: the previous row and the bar it was
/// measured against. Kept even when today's row already exists, so `/routes`
/// and the gauges can show *why* an element is where it is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Basis {
    pub previous: Row,
    pub threshold: f64,
    /// `ceil(threshold × previous.allowance)`; 0 for an unlimited row.
    pub required: i64,
    pub met: bool,
}

impl Basis {
    /// `committed / allowance`, or `None` for an unlimited or zero allowance.
    pub fn attainment(&self) -> Option<f64> {
        match self.previous.allowance {
            Some(a) if a > 0 => Some(self.previous.committed as f64 / a as f64),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    /// The schedule position in force on `day_index`. Never above it, never
    /// below 0 once started.
    pub step: i64,
    pub outcome: Outcome,
    /// `None` under `Auto`, before the start, and on an element's first day.
    pub basis: Option<Basis>,
}

impl Decision {
    /// How many days this element is behind its calendar: `day_index - step`.
    pub fn held_days(&self, day_index: i64) -> i64 {
        (day_index - self.step).max(0)
    }
}

/// Tolerance for `threshold × allowance`: 0.9 × 50 is 45.000000000000004 in
/// binary floating point, and 45 sent of 50 planned has met a 90% bar.
const EPSILON: f64 = 1e-9;

/// The committed volume that meets `threshold` of `allowance`.
pub fn required(threshold: f64, allowance: i64) -> i64 {
    if allowance <= 0 {
        return 0;
    }
    let raw = threshold * allowance as f64;
    // Saturating: a threshold is at most 1, so this never exceeds `allowance`.
    ((raw - EPSILON).ceil().max(0.0) as i64).min(allowance)
}

/// The threshold arm, measured against one previous row.
pub fn basis(threshold: f64, previous: Row) -> Basis {
    let required = previous.allowance.map_or(0, |a| required(threshold, a));
    Basis {
        previous,
        threshold,
        required,
        met: previous.committed >= required,
    }
}

/// Where an element is on `day_index`.
///
/// `today` is the element's row for `day_index` if one exists; `previous` the
/// latest row before it. Rows at or after `day_index` passed as `previous` are
/// ignored rather than trusted, so a caller's off-by-one cannot advance
/// anything.
pub fn decide(
    rule: Advance,
    day_index: i64,
    today: Option<Row>,
    previous: Option<Row>,
) -> Decision {
    if day_index < 0 {
        return Decision {
            step: 0,
            outcome: Outcome::NotStarted,
            basis: None,
        };
    }

    let previous = previous.filter(|p| p.day_index < day_index);
    let threshold = match rule {
        Advance::Auto => {
            // A row created under a threshold rule and still today keeps its
            // step: its allowance was written from it, and that is authoritative.
            let step = today
                .and_then(|t| t.step)
                .map_or(day_index, |s| s.clamp(0, day_index));
            let outcome = if step == day_index {
                Outcome::Auto
            } else {
                Outcome::Existing
            };
            return Decision {
                step,
                outcome,
                basis: None,
            };
        }
        Advance::Threshold { threshold } => threshold,
    };

    let basis = previous.map(|p| basis(threshold, p));

    if let Some(step) = today.and_then(|t| t.step) {
        return Decision {
            step: step.clamp(0, day_index),
            outcome: Outcome::Existing,
            basis,
        };
    }

    match basis {
        None => Decision {
            step: 0,
            outcome: Outcome::First,
            basis: None,
        },
        Some(b) => {
            let from = b.previous.step_or_day().max(0);
            let step = (from + i64::from(b.met)).min(day_index);
            Decision {
                step,
                outcome: if b.met {
                    Outcome::Advanced
                } else {
                    Outcome::Held
                },
                basis: Some(b),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T90: Advance = Advance::Threshold { threshold: 0.9 };

    fn row(day_index: i64, step: i64, allowance: i64, committed: i64) -> Row {
        Row {
            day_index,
            step: Some(step),
            allowance: Some(allowance),
            committed,
        }
    }

    /// The schedule an element's allowance comes from, indexed by step with
    /// §7.2's "final value repeats".
    fn at(schedule: &[i64], step: i64) -> i64 {
        schedule[(step as usize).min(schedule.len() - 1)]
    }

    /// Run one element through consecutive days, as the store would: each
    /// day's row is created at the decided step with the schedule's allowance,
    /// and `sent[d]` of it is committed. `None` is a day with no traffic — no
    /// row at all. Returns the allowance in force on each day.
    fn simulate(rule: Advance, schedule: &[i64], sent: &[Option<i64>]) -> Vec<i64> {
        let mut last: Option<Row> = None;
        let mut out = Vec::new();
        for (d, sent) in sent.iter().enumerate() {
            let d = d as i64;
            let decision = decide(rule, d, None, last);
            let allowance = at(schedule, decision.step);
            out.push(allowance);
            if let Some(n) = sent {
                last = Some(row(d, decision.step, allowance, (*n).min(allowance)));
            }
        }
        out
    }

    // -- the author's example -------------------------------------------------

    #[test]
    fn the_four_route_example_holds_only_the_element_that_fell_short() {
        // Day 0 plans 50/50/50/100 and sends 45/40/45/98. At 90% the bars are
        // 45/45/45/90, so B alone is held: day 1 is 100/50/100/200.
        let abc = [50, 100, 200, 400];
        let d = [100, 200, 400, 800];
        let day0 = [(abc, 45), (abc, 40), (abc, 45), (d, 98)];

        let day1: Vec<i64> = day0
            .iter()
            .map(|(schedule, sent)| {
                let prev = row(0, 0, schedule[0], *sent);
                at(schedule, decide(T90, 1, None, Some(prev)).step)
            })
            .collect();
        assert_eq!(day1, vec![100, 50, 100, 200]);
    }

    #[test]
    fn the_example_continued_into_day_two() {
        // Day 1 sends 100/50/80/200 against 100/50/100/200: A, B and D meet
        // (B's bar is now 45 of 50), C misses 90 by ten.
        let abc = [50, 100, 200, 400];
        let d = [100, 200, 400, 800];
        assert_eq!(
            simulate(T90, &abc, &[Some(45), Some(100), None]),
            vec![50, 100, 200]
        );
        assert_eq!(
            simulate(T90, &abc, &[Some(40), Some(50), None]),
            vec![50, 50, 100]
        );
        assert_eq!(
            simulate(T90, &abc, &[Some(45), Some(80), None]),
            vec![50, 100, 100]
        );
        assert_eq!(
            simulate(T90, &d, &[Some(98), Some(200), None]),
            vec![100, 200, 400]
        );
    }

    #[test]
    fn auto_mode_advances_on_elapsed_days_whatever_was_sent() {
        let abc = [50, 100, 200, 400];
        for sent in [Some(0), Some(1), Some(45), Some(50), None] {
            assert_eq!(
                simulate(Advance::Auto, &abc, &[sent, sent, sent]),
                vec![50, 100, 200],
                "{sent:?}"
            );
        }
    }

    // -- the rule's edges -----------------------------------------------------

    #[test]
    fn the_bar_is_exactly_ninety_percent_despite_floating_point() {
        assert_eq!(required(0.9, 50), 45);
        assert_eq!(required(0.9, 100), 90);
        assert_eq!(required(0.9, 10), 9);
        assert_eq!(required(0.9, 7), 7); // 6.3 rounds up: 6 of 7 is under 90%
        assert_eq!(required(0.1, 3), 1);
        assert!(basis(0.9, row(0, 0, 50, 45)).met);
        assert!(!basis(0.9, row(0, 0, 50, 44)).met);
    }

    #[test]
    fn a_threshold_of_one_needs_every_planned_message() {
        let full = Advance::Threshold { threshold: 1.0 };
        assert_eq!(decide(full, 1, None, Some(row(0, 0, 50, 49))).step, 0);
        assert_eq!(decide(full, 1, None, Some(row(0, 0, 50, 50))).step, 1);
    }

    #[test]
    fn a_zero_allowance_day_is_always_met() {
        // A schedule may plan a day of nothing; it must not hold forever.
        let d = decide(T90, 1, None, Some(row(0, 0, 0, 0)));
        assert_eq!((d.step, d.outcome), (1, Outcome::Advanced));
        assert_eq!(d.basis.unwrap().required, 0);
        assert_eq!(d.basis.unwrap().attainment(), None);
    }

    #[test]
    fn a_day_with_no_traffic_does_not_advance() {
        // Day 1 has no row, so day 2 rests on day 0, which met: one step, not
        // two. And a met day 0 followed by a silent week is still one step.
        let schedule = [50, 100, 200, 400];
        assert_eq!(
            simulate(T90, &schedule, &[Some(50), None, Some(0), Some(0)]),
            vec![50, 100, 100, 100]
        );
        let d = decide(T90, 8, None, Some(row(0, 0, 50, 50)));
        assert_eq!((d.step, d.outcome), (1, Outcome::Advanced));
    }

    #[test]
    fn nothing_sent_at_all_holds_at_the_first_entry() {
        let schedule = [50, 100];
        assert_eq!(
            simulate(T90, &schedule, &[None, None, None]),
            vec![50, 50, 50]
        );
        assert_eq!(decide(T90, 5, None, None).outcome, Outcome::First);
    }

    #[test]
    fn the_final_entry_repeats_and_never_uncaps() {
        let schedule = [50, 100];
        assert_eq!(
            simulate(T90, &schedule, &[Some(50); 6]),
            vec![50, 100, 100, 100, 100, 100]
        );
    }

    #[test]
    fn the_step_is_never_ahead_of_the_calendar() {
        // A previous row claiming a step at or above today's index — a mode
        // switch, a hand-edited row — cannot push the element past `day_index`.
        let d = decide(T90, 3, None, Some(row(2, 9, 50, 50)));
        assert_eq!(d.step, 3);
    }

    #[test]
    fn todays_row_is_authoritative_once_written() {
        // Decided when the row was created; what the previous row says now (a
        // late commit landed on it) does not move it.
        let today = Row {
            day_index: 1,
            step: Some(0),
            allowance: Some(50),
            committed: 3,
        };
        let d = decide(T90, 1, Some(today), Some(row(0, 0, 50, 50)));
        assert_eq!((d.step, d.outcome), (0, Outcome::Existing));
        // The basis is still reported, so /routes can say why.
        assert!(d.basis.unwrap().met);
    }

    #[test]
    fn a_legacy_row_without_a_step_reads_as_its_day() {
        // Rows from before D-115 advanced on the calendar; the migration
        // backfills them, and a null that escaped it means the same thing.
        let prev = Row {
            day_index: 4,
            step: None,
            allowance: Some(800),
            committed: 800,
        };
        assert_eq!(decide(T90, 5, None, Some(prev)).step, 5);
        let today_unstepped = Row {
            day_index: 5,
            step: None,
            allowance: Some(800),
            committed: 0,
        };
        // A row with no step is not a decision: decide from the previous row.
        assert_eq!(
            decide(T90, 5, Some(today_unstepped), Some(prev)).outcome,
            Outcome::Advanced
        );
    }

    #[test]
    fn an_override_day_is_measured_against_the_plan_not_the_override() {
        // Row.allowance is the planned value (D-115). An operator who pulled
        // the day down to 10 and sent all 10 has not met a plan of 50.
        let throttled = row(0, 0, 50, 10);
        assert_eq!(decide(T90, 1, None, Some(throttled)).outcome, Outcome::Held);
    }

    #[test]
    fn previous_rows_from_today_or_later_are_ignored() {
        let d = decide(T90, 2, None, Some(row(2, 0, 50, 50)));
        assert_eq!(d.outcome, Outcome::First);
        let d = decide(T90, 2, None, Some(row(7, 0, 50, 50)));
        assert_eq!(d.outcome, Outcome::First);
    }

    #[test]
    fn a_future_start_decides_nothing() {
        for rule in [Advance::Auto, T90] {
            let d = decide(rule, -1, None, Some(row(-2, 0, 50, 50)));
            assert_eq!((d.step, d.outcome), (0, Outcome::NotStarted));
        }
    }

    #[test]
    fn auto_is_the_day_index_on_every_day() {
        // The property that keeps `advance: auto` byte-identical to §7.2 before
        // D-115, whatever the history looks like.
        for d in 0..1000 {
            for prev in [
                None,
                Some(row(d - 1, 0, 50, 0)),
                Some(row(d - 1, 3, 50, 50)),
            ] {
                let got = decide(Advance::Auto, d, None, prev);
                assert_eq!((got.step, got.outcome), (d, Outcome::Auto), "day {d}");
            }
        }
    }

    #[test]
    fn switching_from_auto_to_threshold_continues_from_the_last_rows_step() {
        // Auto wrote step 5 on day 5; the restart is volume-gated from there.
        assert_eq!(decide(T90, 6, None, Some(row(5, 5, 800, 800))).step, 6);
        assert_eq!(decide(T90, 6, None, Some(row(5, 5, 800, 10))).step, 5);
    }

    #[test]
    fn switching_from_threshold_to_auto_catches_up_with_the_calendar() {
        // Held at step 2 on day 5, then `advance: auto`: day 6 is step 6.
        let d = decide(Advance::Auto, 6, None, Some(row(5, 2, 200, 0)));
        assert_eq!(d.step, 6);
        // ...but a row already created today under the threshold rule keeps
        // its step, because its allowance was written from it.
        let today = row(6, 3, 400, 0);
        let d = decide(Advance::Auto, 6, Some(today), None);
        assert_eq!((d.step, d.outcome), (3, Outcome::Existing));
    }

    #[test]
    fn domain_groups_advance_independently() {
        // One route, two groups with their own series: google meets 20/20,
        // catchall sends 30/50. Day 1 is google 40, catchall still 50.
        let google = [20, 40];
        let catchall = [50, 100];
        assert_eq!(simulate(T90, &google, &[Some(20), None]), vec![20, 40]);
        assert_eq!(simulate(T90, &catchall, &[Some(30), None]), vec![50, 50]);
    }

    #[test]
    fn held_days_is_how_far_behind_the_calendar_an_element_is() {
        let d = decide(T90, 4, None, Some(row(3, 1, 100, 0)));
        assert_eq!(d.step, 1);
        assert_eq!(d.held_days(4), 3);
        assert_eq!(decide(Advance::Auto, 4, None, None).held_days(4), 0);
    }

    #[test]
    fn an_unlimited_previous_row_is_met() {
        let prev = Row {
            day_index: 0,
            step: Some(0),
            allowance: None,
            committed: 0,
        };
        assert!(decide(T90, 1, None, Some(prev)).basis.unwrap().met);
    }

    #[test]
    fn attainment_is_committed_over_planned() {
        let b = basis(0.9, row(0, 0, 50, 40));
        assert_eq!(b.attainment(), Some(0.8));
        assert_eq!(b.required, 45);
        assert!(!b.met);
    }
}
