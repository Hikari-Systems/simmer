//! Known defects in the compose tiers: `test/known-findings.json`.
//!
//! The compose-tier counterpart of `tests/support::xfail`, with the same rules.
//! A check listed as a known finding still runs every time:
//!
//! - it fails **for the listed reason** → `XFAIL`, and the tier does not fail;
//! - it fails for **any other reason** → a real failure;
//! - it **passes** → `XPASS`, a failure, so the fixing commit must delete the
//!   entry — the list can never outlive the defects it names.
//!
//! Without this, a nightly run carrying a dozen known defects is red every night
//! and a new regression is invisible inside it.

use serde::Deserialize;

/// Embedded at compile time: the list and the tests that consult it change in
/// the same commit, so they cannot be out of step at run time.
const KNOWN: &str = include_str!("../../test/known-findings.json");

#[derive(Debug, Deserialize)]
pub struct Entry {
    /// The findings-table id (`F1`, `F2`, …).
    pub id: String,
    /// The check this entry excuses: `<tier>/<scenario>/<check>`.
    pub check: String,
    /// Substrings, any of which the failure message must contain.
    pub because: Vec<String>,
    /// One line on why, for the job summary.
    pub note: String,
}

pub fn known() -> Vec<Entry> {
    serde_json::from_str(KNOWN).expect("test/known-findings.json parses")
}

/// Judge one named check's result against the list, panicking on a real failure
/// or an XPASS. `check` is the `<tier>/<scenario>/<check>` key.
pub fn judge(check: &str, result: Result<(), String>) {
    judge_against(&known(), check, result);
}

pub fn judge_against(entries: &[Entry], check: &str, result: Result<(), String>) {
    if let Err(failure) = assess_against(entries, check, result) {
        panic!("{failure}");
    }
}

/// [`judge`], returning the real failure or XPASS instead of panicking — for a
/// tier that judges several checks and wants every one of them reported before
/// it fails. An XFAIL is printed and is `Ok`.
pub fn assess(check: &str, result: Result<(), String>) -> Result<(), String> {
    assess_against(&known(), check, result)
}

pub fn assess_against(
    entries: &[Entry],
    check: &str,
    result: Result<(), String>,
) -> Result<(), String> {
    let entry = entries.iter().find(|e| e.check == check);
    match (entry, result) {
        (None, Ok(())) => Ok(()),
        (None, Err(why)) => Err(format!("{check} failed: {why}")),
        (Some(e), Err(why)) if e.because.iter().any(|b| why.contains(b.as_str())) => {
            eprintln!("XFAIL {} {check}: {why}", e.id);
            Ok(())
        }
        (Some(e), Err(why)) => Err(format!(
            "{check} failed, but not for known finding {}'s reason (expected one of {:?}): {why}",
            e.id, e.because
        )),
        (Some(e), Ok(())) => Err(format!(
            "XPASS {} {check}: the check now passes, so the defect looks fixed. Delete its \
             entry from test/known-findings.json in the fixing commit",
            e.id
        )),
    }
}
