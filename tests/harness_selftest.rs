//! The compose harness's own checkers, tested — "checking the checks".
//!
//! Every heavy tier ends by asking [`compose::reconcile`] whether anything was
//! lost or duplicated, and the soak asks [`compose::leak`] whether memory grew. A
//! checker that cannot see the failure it exists for makes every tier that relies
//! on it a green light wired to nothing. So each rule is shown here to catch the
//! defect it names, on synthetic records, in ordinary `cargo test` — no Docker.

mod compose;

use compose::findings::{judge_against, Entry};
use compose::leak::{floors, theil_sen, verdict, Limits};
use compose::mail::{header, without_headers};
use compose::reconcile::{read_jsonl, reconcile, Outcome, Received, Sent};

fn sent(id: &str, code: u16) -> Sent {
    Sent {
        id: id.into(),
        code,
        stage: if code == 0 { "transport" } else { "dot" }.into(),
        text: String::new(),
    }
}

fn got(id: &str, outcome: Outcome) -> Received {
    Received {
        id: id.into(),
        outcome,
        mismatch: false,
    }
}

// -- reconcile -------------------------------------------------------------

#[test]
fn a_clean_run_reconciles() {
    let s = [
        sent("a", 250),
        sent("b", 250),
        sent("c", 451),
        sent("d", 550),
    ];
    let r = [
        got("a", Outcome::Delivered),
        got("b", Outcome::Delivered),
        got("c", Outcome::Rejected),
    ];
    let report = reconcile(&s, &r, Some(0));
    assert!(report.is_clean(), "{:?}", report.violations);
    assert_eq!(
        (report.accepted, report.deferred, report.refused),
        (2, 1, 1)
    );
    assert_eq!(report.stored, 2);
}

#[test]
fn a_lost_message_is_caught() {
    let report = reconcile(
        &[sent("a", 250), sent("b", 250)],
        &[got("a", Outcome::Delivered)],
        None,
    );
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.starts_with("b:") && v.contains("nothing arrived")),
        "{:?}",
        report.violations
    );
}

#[test]
fn a_duplicate_is_caught_whatever_the_client_was_told() {
    for code in [250, 451] {
        let report = reconcile(
            &[sent("a", code)],
            &[got("a", Outcome::Delivered), got("a", Outcome::Delivered)],
            None,
        );
        assert!(
            report.violations.iter().any(|v| v.contains("duplicate")),
            "code {code}: {:?}",
            report.violations
        );
    }
}

#[test]
fn a_deferred_message_that_arrived_anyway_is_caught() {
    // The F2 shape: stored, and the client told to retry.
    let report = reconcile(&[sent("a", 421)], &[got("a", Outcome::Delivered)], None);
    assert!(
        report
            .violations
            .iter()
            .any(|v| v.contains("was delivered")),
        "{:?}",
        report.violations
    );
}

#[test]
fn an_ambiguous_delivery_is_allowed_only_if_simmer_counted_it() {
    let s = [sent("a", 451)];
    let r = [got("a", Outcome::DroppedAfterDot)];
    assert!(reconcile(&s, &r, Some(1)).is_clean());
    let uncounted = reconcile(&s, &r, Some(0));
    assert!(
        uncounted
            .violations
            .iter()
            .any(|v| v.contains("simmer_ambiguous_delivery_total")),
        "{:?}",
        uncounted.violations
    );
}

#[test]
fn a_phantom_and_a_crossed_envelope_are_caught() {
    let mut crossed = got("a", Outcome::Delivered);
    crossed.mismatch = true;
    let report = reconcile(
        &[sent("a", 250)],
        &[crossed, got("zz", Outcome::Delivered)],
        None,
    );
    assert!(report.violations.iter().any(|v| v.contains("phantom")));
    assert!(report
        .violations
        .iter()
        .any(|v| v.contains("different envelope")));
}

#[test]
fn records_parse_from_json_lines() {
    let sent: Vec<Sent> = read_jsonl(
        "{\"id\":\"r-0-1\",\"code\":250,\"stage\":\"dot\"}\n\n{\"id\":\"r-0-2\",\"code\":0,\"stage\":\"transport\",\"text\":\"reset\"}\n",
    );
    let received: Vec<Received> = read_jsonl(
        "{\"id\":\"r-0-1\",\"outcome\":\"delivered\"}\n{\"id\":\"r-0-2\",\"outcome\":\"dropped_after_dot\",\"mismatch\":false}\n",
    );
    assert_eq!(sent.len(), 2);
    assert_eq!(received[1].outcome, Outcome::DroppedAfterDot);
}

// -- leak ------------------------------------------------------------------

const MIB: f64 = 1024.0 * 1024.0;

/// An hour of 10-second samples: a base, a slope in MiB/h, and a sawtooth of
/// bursts that a raw-series fit would mistake for signal.
fn series(base_mib: f64, slope_mib_per_hour: f64) -> Vec<(f64, f64)> {
    (0..360)
        .map(|i| {
            let t = i as f64 * 10.0;
            let burst = if i % 90 < 6 { 40.0 } else { (i % 7) as f64 };
            (
                t,
                (base_mib + slope_mib_per_hour * t / 3600.0 + burst) * MIB,
            )
        })
        .collect()
}

fn soak_limits() -> Limits {
    Limits {
        slope_per_hour: 2.0 * MIB,
    }
}

#[test]
fn floors_take_the_minimum_of_each_window() {
    let f = floors(
        &[
            (0.0, 5.0),
            (10.0, 3.0),
            (299.0, 9.0),
            (300.0, 7.0),
            (310.0, 6.0),
        ],
        300.0,
    );
    assert_eq!(f, vec![(0.0, 3.0), (300.0, 6.0)]);
}

#[test]
fn theil_sen_ignores_a_few_outliers() {
    let mut pts: Vec<(f64, f64)> = (0..20).map(|i| (i as f64, 2.0 * i as f64)).collect();
    pts[3].1 = 1000.0;
    pts[15].1 = -1000.0;
    let slope = theil_sen(&pts).unwrap();
    assert!((slope - 2.0).abs() < 0.01, "{slope}");
}

#[test]
fn a_flat_bursty_series_is_not_a_leak() {
    let v = verdict(&series(200.0, 0.0), 600.0, 300.0, soak_limits());
    assert!(!v.inconclusive);
    assert!(!v.leaking, "{v:?}");
}

#[test]
fn a_planted_leak_just_over_the_threshold_is_caught_in_one_hour() {
    // The calibration target, exactly: a 64 B/message leak at 10 msg/s is
    // 2.2 MiB/h, over the one-hour nightly run. This is the case a fixed quartile
    // gate got wrong — see leak.rs — so it is planted at that rate, not at some
    // larger, easier one.
    let v = verdict(&series(200.0, 2.2), 600.0, 300.0, soak_limits());
    assert!(v.leaking, "{v:?}");
    assert!((v.slope_per_hour / MIB - 2.2).abs() < 0.3, "{v:?}");
}

#[test]
fn a_gross_leak_is_caught() {
    let v = verdict(&series(200.0, 50.0), 600.0, 300.0, soak_limits());
    assert!(v.leaking, "{v:?}");
}

#[test]
fn growth_just_under_the_threshold_passes() {
    // The other side of the line: the gate is a threshold, not a hair trigger.
    let v = verdict(&series(200.0, 1.6), 600.0, 300.0, soak_limits());
    assert!(!v.leaking, "{v:?}");
}

#[test]
fn a_one_off_step_is_not_a_leak() {
    // A cache warming up mid-run: one step, then flat.
    let s: Vec<(f64, f64)> = (0..360)
        .map(|i| (i as f64 * 10.0, if i < 60 { 200.0 } else { 230.0 } * MIB))
        .collect();
    let v = verdict(&s, 900.0, 300.0, soak_limits());
    assert!(!v.leaking, "{v:?}");
}

#[test]
fn too_short_a_run_is_inconclusive_rather_than_passed() {
    let v = verdict(&series(200.0, 100.0)[..60], 60.0, 300.0, soak_limits());
    assert!(v.inconclusive);
    assert!(!v.leaking);
}

// -- known findings --------------------------------------------------------

fn entry(check: &str, because: &str) -> Entry {
    Entry {
        id: "F9".into(),
        check: check.into(),
        because: vec![because.into()],
        note: String::new(),
    }
}

#[test]
fn the_known_findings_file_parses_and_names_real_ids() {
    let known = compose::findings::known();
    assert!(!known.is_empty());
    for e in &known {
        assert!(e.id.starts_with('F'), "{e:?}");
        assert_eq!(
            e.check.split('/').count(),
            3,
            "check is <tier>/<scenario>/<check>: {e:?}"
        );
        assert!(!e.because.is_empty(), "{e:?}");
    }
}

#[test]
fn judging_follows_xfail_rules() {
    let list = [entry("t/s/c", "known reason")];
    let run = |check: &str, r: Result<(), String>| {
        std::panic::catch_unwind(|| judge_against(&list, check, r)).is_ok()
    };
    assert!(run("t/s/other", Ok(())), "unlisted pass");
    assert!(!run("t/s/other", Err("boom".into())), "unlisted failure");
    assert!(run("t/s/c", Err("a known reason here".into())), "XFAIL");
    assert!(!run("t/s/c", Err("something else".into())), "wrong reason");
    assert!(!run("t/s/c", Ok(())), "XPASS");
}

// -- mail ------------------------------------------------------------------

#[test]
fn headers_unfold_and_match_case_insensitively() {
    let raw = "Subject: one\r\n two\r\nX-Test-Id: r-1\r\n\r\nX-Test-Id: body\r\n";
    assert_eq!(header(raw, "subject").as_deref(), Some("one two"));
    assert_eq!(header(raw, "x-test-id").as_deref(), Some("r-1"));
}

#[test]
fn excluded_headers_go_with_their_continuations_and_the_body_stays() {
    let raw = "Received: a\r\n b\r\nFrom: x\r\n\r\nReceived: in the body\r\n";
    assert_eq!(
        without_headers(raw, &["received"]),
        ["From: x", "", "Received: in the body"]
    );
}
