//! No loss and no duplicates — the accounting every load tier ends with (U3).
//!
//! Two records of the same run, written independently: the loadgen's (what each
//! client was *told*) and the sink's (what each downstream *received*). Both carry
//! the loadgen's per-message id — in the RCPT local part and an `X-Test-Id`
//! header, never `Message-ID`, which routes rewrite. Reconciling them is the only
//! way to know that a `250` meant delivered-once and a `451` meant not delivered,
//! under load, without trusting Simmer's own counters.
//!
//! Pure functions over the two record sets, so the rules themselves are tested
//! (`tests/harness_selftest.rs`) without a stack.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

/// One loadgen line: what the client was told about one message.
#[derive(Debug, Clone, Deserialize)]
pub struct Sent {
    pub id: String,
    /// The final reply's code; `0` for a transport failure (no reply at all).
    pub code: u16,
    /// Where the conversation ended: `banner`, `tls`, `auth`, `rset` (a later
    /// message refused before it began, as at D-081's session deadline), `mail`,
    /// `rcpt`, `data`, `dot` (the final reply), `transport`, or `not_sent`.
    pub stage: String,
    #[serde(default)]
    pub text: String,
    /// How long the client waited for this reply. The load tiers gate on it where
    /// being answered *promptly* is the point (S3).
    #[serde(default)]
    pub latency_ms: f64,
}

/// One sink line: what a downstream did with one message.
#[derive(Debug, Clone, Deserialize)]
pub struct Received {
    pub id: String,
    pub outcome: Outcome,
    /// The sink found different ids in the RCPT and in `X-Test-Id` — a body on
    /// the wrong envelope, which a pooled connection must never produce.
    #[serde(default)]
    pub mismatch: bool,
    /// The `X-Simmer-Correlation` header, where the route stamps one: the id
    /// Simmer logged this message under, so a slow one can be found in its log.
    #[serde(default)]
    pub correlation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Accepted at the final dot and answered `250`.
    Delivered,
    /// Refused by a scripted fault before the dot.
    Rejected,
    /// The sink hung up before the dot: nothing was stored.
    DroppedBeforeDot,
    /// Stored, then the sink hung up without answering the dot — §10.2's window.
    DroppedAfterDot,
    /// Stored, and the reply to the dot was held past Simmer's budget — also §10.2.
    StalledAtDot,
}

impl Outcome {
    /// Did the downstream end up holding the message?
    fn stored(self) -> bool {
        matches!(
            self,
            Outcome::Delivered | Outcome::DroppedAfterDot | Outcome::StalledAtDot
        )
    }

    /// Stored without a positive reply: Simmer cannot know, so must say so.
    fn ambiguous(self) -> bool {
        matches!(self, Outcome::DroppedAfterDot | Outcome::StalledAtDot)
    }
}

#[derive(Debug, Default)]
pub struct Report {
    /// Every rule broken, one line each. Empty is a pass.
    pub violations: Vec<String>,
    pub accepted: usize,
    pub deferred: usize,
    pub refused: usize,
    pub transport: usize,
    pub stored: usize,
    pub ambiguous: usize,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Reconcile a run.
///
/// `ambiguous_delta` is the change in `simmer_ambiguous_delivery_total` over the
/// run, when the caller has it: every message a downstream stored without Simmer
/// seeing the reply must be counted there, and nothing else.
pub fn reconcile(sent: &[Sent], received: &[Received], ambiguous_delta: Option<u64>) -> Report {
    let mut report = Report::default();

    let mut told: BTreeMap<&str, &Sent> = BTreeMap::new();
    for s in sent {
        if told.insert(s.id.as_str(), s).is_some() {
            report
                .violations
                .push(format!("{}: the loadgen reported this id twice", s.id));
        }
        match s.code {
            0 => report.transport += 1,
            200..=299 => report.accepted += 1,
            400..=499 => report.deferred += 1,
            _ => report.refused += 1,
        }
    }

    let mut stored: BTreeMap<&str, Vec<Outcome>> = BTreeMap::new();
    for r in received {
        if r.mismatch {
            report.violations.push(format!(
                "{}: the sink received a body under a different envelope id",
                r.id
            ));
        }
        stored.entry(r.id.as_str()).or_default().push(r.outcome);
    }

    // R5 — nothing arrives that no client sent.
    for id in stored.keys() {
        if !told.contains_key(id) {
            report
                .violations
                .push(format!("{id}: received but never sent (phantom)"));
        }
    }

    let mut ambiguous_seen = 0u64;
    for (id, s) in &told {
        let outcomes = stored.get(id).map(Vec::as_slice).unwrap_or(&[]);
        let copies = outcomes.iter().filter(|o| o.stored()).count();
        report.stored += copies.min(1);

        // R2 — no message is stored twice, whatever the client was told.
        if copies > 1 {
            report
                .violations
                .push(format!("{id}: stored {copies} times (duplicate delivery)"));
        }

        // A stall is ambiguous only if Simmer gave up on it. Simmer tells a client
        // 2xx only after the downstream's own 2xx (§7.4, §10.1), so a stalled
        // message its client was told 2xx for had its late reply seen in time.
        // Until D-081, S9's stalls were all cut, which made "every stall is
        // ambiguous" true there by accident.
        let ambiguous = outcomes.iter().any(|o| match o {
            Outcome::StalledAtDot => !(200..=299).contains(&s.code),
            o => o.ambiguous(),
        });
        if ambiguous {
            ambiguous_seen += 1;
        }

        match s.code {
            // R1 — a 250 means stored, exactly once.
            200..=299 => {
                if copies == 0 {
                    report.violations.push(format!(
                        "{id}: the client was told {} but nothing arrived",
                        s.code
                    ));
                }
            }
            // R3 — anything else means not stored, unless the downstream took it
            // in the §10.2 window, which Simmer cannot see and must count.
            _ => {
                if copies > 0 && !ambiguous {
                    report.violations.push(format!(
                        "{id}: the client was told {} ({}) but the message was delivered",
                        s.code, s.stage
                    ));
                }
            }
        }
    }
    report.ambiguous = ambiguous_seen as usize;

    if let Some(delta) = ambiguous_delta {
        if delta != ambiguous_seen {
            report.violations.push(format!(
                "simmer_ambiguous_delivery_total rose by {delta}, but the sink recorded \
                 {ambiguous_seen} messages stored without a reply"
            ));
        }
    }

    report
}

/// Parse a JSON-lines file of records, skipping blank lines and naming the line
/// that fails.
pub fn read_jsonl<T: serde::de::DeserializeOwned>(text: &str) -> Vec<T> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(n, l)| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("line {}: {e}: {l}", n + 1))
        })
        .collect()
}

/// Ids that appear in `received` but whose outcomes disagree with each other in a
/// way no single delivery attempt can produce — for diagnostics only.
pub fn ids_with_mixed_outcomes(received: &[Received]) -> BTreeSet<String> {
    let mut by_id: BTreeMap<&str, BTreeSet<&'static str>> = BTreeMap::new();
    for r in received {
        let name = match r.outcome {
            Outcome::Delivered => "delivered",
            Outcome::Rejected => "rejected",
            Outcome::DroppedBeforeDot => "dropped_before_dot",
            Outcome::DroppedAfterDot => "dropped_after_dot",
            Outcome::StalledAtDot => "stalled_at_dot",
        };
        by_id.entry(r.id.as_str()).or_default().insert(name);
    }
    by_id
        .into_iter()
        .filter(|(_, o)| o.len() > 1)
        .map(|(id, _)| id.to_string())
        .collect()
}
