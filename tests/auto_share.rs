//! D-097's `share: auto` on the mail-trap tier.
//!
//! Every tier below this one measures the *decision*: the arithmetic in
//! `src/routing/partial.rs`, the walk in `tests/partial_ramp.rs`, the report in
//! `src/admin/view.rs`. None of them measures the **mail**. This one counts
//! messages in two real mailboxes at the end of two real SMTP conversations,
//! which is the only place the claim "the cap fills, and the rest goes to the
//! established route" can actually be made.
//!
//! Three things it proves that nothing else can:
//!
//! 1. A route whose share is computed carries **exactly its allowance and not one
//!    message more** — the property the whole component exists for, which D-097
//!    changes the route *to* and so must be shown not to have broken.
//! 2. Every message it is not offered **arrives at the other provider**. Nothing
//!    is dropped and nothing is deferred; a partial ramp is a steering rule.
//! 3. The cap is reached **later than an unthrottled route would reach it**, and
//!    still reached. That is the trade D-097 exists to make, and it needs two
//!    runs of the same traffic against two configurations to see.
//!
//! # Running it
//!
//! ```sh
//! docker compose -f docker-compose.yml -f test/compose/acceptance.yml \
//!   -f test/compose/autoshare.yml --profile acceptance --profile autoshare \
//!   up -d --build
//! cargo test --test auto_share -- --ignored --test-threads=1
//! ```
//!
//! From the development jail, where published ports are unreachable, join the
//! stack's network and name the containers:
//!
//! ```sh
//! docker network connect simmer_default "$(hostname)"
//! export SIMMER_TEST_ADMIN=http://simmer-app-1:8080
//! export SIMMER_TEST_TRAP_WARMING=http://simmer-trap-warming-1:8025
//! export SIMMER_TEST_TRAP_OVERFLOW=http://simmer-trap-overflow-1:8025
//! ```

mod compose;

use compose::stack::AUTOSHARE;
use compose::traps::{OVERFLOW, WARMING};
use simmer::config::Config;

/// The cap `test/config/simmer.autoshare.yaml` gives the warming route on day 0,
/// read from the file rather than repeated, so the expected numbers exist once.
fn cap() -> usize {
    let cfg = config();
    let schedule = &cfg
        .default_ramp()
        .route("warming-newbrand")
        .expect("the warming route")
        .warmup
        .as_ref()
        .expect("a warmup")
        .schedule;
    schedule.allowance_for("catchall", 0).expect("a cap") as usize
}

fn config() -> Config {
    compose::configs::load("test/config/simmer.autoshare.yaml")
}

fn reset() {
    AUTOSHARE.reset_quota();
    WARMING.reset();
    OVERFLOW.reset();
}

/// Where through the ramp day the suite takes its readings.
///
/// The share depends on how far through the day the route is, so a burst is a
/// reading at one point on this axis and says nothing about the others. The last
/// entry is past `fill_by`, where the window has closed and the route is open to
/// its ceiling.
const DAY_POINTS: [f64; 5] = [0.05, 0.2, 0.35, 0.5, 0.65];

#[test]
#[ignore = "needs the acceptance and autoshare compose profiles"]
fn a_computed_share_fills_its_cap_exactly_and_steers_the_rest() {
    let cap = cap();
    // Comfortably more than the cap at every reading, so the controller always
    // has traffic to turn away and the run never mistakes "no traffic" for
    // "throttled".
    let batch = cap * 2;

    // Once, before the walk. Every reading is the same `day_index` and therefore
    // the same row — clearing it mid-walk would erase the accounting under test.
    AUTOSHARE.restart_app_at_elapsed(DAY_POINTS[0]);
    reset();

    let mut sent = 0;
    for (i, point) in DAY_POINTS.iter().enumerate() {
        AUTOSHARE.restart_app_at_elapsed(*point);

        let replies = compose::loadgen::run(
            &AUTOSHARE,
            &["--count", &batch.to_string(), "--tag", "auto"],
        );
        sent += batch;

        // §10.1: everything is accepted. A partial ramp steers; it never
        // refuses. If this fails the counts below are measuring a rate limit.
        for r in &replies {
            assert_eq!(r.code, 250, "at {point} through the day: {r:?}");
        }

        // Wait for the mail to settle before reading either trap: what is being
        // asserted is a total, and a total read too early is always low.
        let delivered = OVERFLOW.wait_for_count(sent - WARMING.count());
        let warming = WARMING.count();

        // The property the component exists for, checked at every reading
        // rather than only at the end: not one message more than the cap.
        assert!(
            warming <= cap,
            "at {point} through the day the warming route had carried {warming}, \
             against a cap of {cap}"
        );
        assert_eq!(
            warming + delivered,
            sent,
            "nothing may be dropped: {warming} + {delivered} of {sent} sent, at \
             reading {i} ({point} through the day)"
        );
    }

    // And by the time the fill window has closed, the cap is met. This is the
    // whole of D-097's bargain: the ramp spreads the cap across the day instead
    // of spending it in the first burst, and still spends it.
    let warming = WARMING.wait_for_count(cap);
    assert_eq!(
        warming, cap,
        "past fill_by the cap must be met exactly; carried {warming} of {cap}. \
         Short means the controller stalled — which is what the tail release \
         exists to prevent"
    );
}

#[test]
#[ignore = "needs the acceptance and autoshare compose profiles"]
fn the_ramp_takes_less_of_a_burst_than_an_ungated_route_would() {
    // The trade D-097 makes, measured against the thing it is a trade against:
    // the same burst, at the same point in the same day, with and without the
    // computed share. This is what "the cap is reached later" means for a relay
    // that cannot delay a message — it takes a smaller bite of each burst.
    let cap = cap();
    let burst = cap * 2;
    // Early, where an ungated route would empty the whole cap at once and the
    // controller is at its most cautious.
    const WHEN: f64 = 0.05;

    let take = |graduated: bool| -> usize {
        AUTOSHARE.restart_app_at_elapsed(WHEN);
        reset();
        if graduated {
            // §9.3's graduate jumps to the end of the ramp, and the partial ramp
            // is part of the ramp (D-091) — so this is the same stack with the
            // share taken out of the picture, rather than a second config that
            // could differ in some other way.
            let (status, body) = compose::admin::post(
                &AUTOSHARE,
                "/routes/warming-newbrand/graduate",
                &serde_json::json!({}),
            );
            assert!((200..300).contains(&status), "graduate: {status} {body}");
        }
        compose::loadgen::run(
            &AUTOSHARE,
            &["--count", &burst.to_string(), "--tag", "burst"],
        );
        OVERFLOW.wait_for_count(burst - WARMING.count());
        WARMING.count()
    };

    let throttled = take(false);
    let ungated = take(true);

    assert_eq!(
        ungated, cap,
        "an ungated route takes its whole cap from a burst this size, or the \
         comparison below is not measuring the ramp"
    );
    assert!(
        throttled < ungated,
        "the computed share must take less of the burst than an ungated route: \
         {throttled} against {ungated}"
    );
}

#[test]
#[ignore = "needs the acceptance and autoshare compose profiles"]
fn routes_reports_the_share_it_is_actually_applying() {
    // §9: the control plane must not lie. The share moves as the cap fills, so
    // this is the reading that could most easily become decorative.
    let cap = cap();
    AUTOSHARE.restart_app_at_elapsed(0.05);
    reset();

    let route = || compose::admin::get(&AUTOSHARE, "/routes/warming-newbrand");
    let catchall = |v: &serde_json::Value| -> serde_json::Value {
        v["groups"]
            .as_array()
            .expect("groups")
            .iter()
            .find(|g| g["domain_group"] == "catchall")
            .expect("catchall")
            .clone()
    };
    let share_of =
        |v: &serde_json::Value| catchall(v)["partial_ramp_share"].as_f64().expect("a share");

    let before = route();
    let ramp = &before["partial_ramp"];
    assert_eq!(ramp["mode"], "auto", "reported as: {ramp}");
    assert_eq!(
        ramp["today"],
        serde_json::Value::Null,
        "the share is per group under auto: {ramp}"
    );
    assert_eq!(ramp["auto"]["ceiling"], 0.5, "{ramp}");
    assert_eq!(ramp["auto"]["tail_below"], 0.1, "{ramp}");

    let empty = share_of(&before);
    assert!(
        empty <= 0.5,
        "the ceiling is the one hard promise, and the share was {empty}"
    );

    compose::loadgen::run(
        &AUTOSHARE,
        &["--count", &(cap * 2).to_string(), "--tag", "fill"],
    );
    OVERFLOW.wait_for_count(cap * 2 - WARMING.count());

    let after = route();
    let filled = share_of(&after);
    assert!(
        filled < empty,
        "the share must fall as the cap fills: {empty} on an untouched cap, \
         {filled} after a burst"
    );

    // And the number it reports is the one the mail agrees with.
    let group = catchall(&after);
    assert_eq!(
        group["committed"].as_u64().expect("committed") as usize,
        WARMING.count(),
        "the row and the trap disagree: {group}"
    );
}

// ---------------------------------------------------------------------------
// the one test that needs no Docker
// ---------------------------------------------------------------------------

#[test]
fn the_tier_config_is_valid_and_still_carries_an_auto_share() {
    // The tier's numbers are read off this file, so a config that stopped being
    // an auto share would make every test above pass while measuring D-091.
    let cfg = config();
    let schedule = &cfg
        .default_ramp()
        .route("warming-newbrand")
        .expect("the warming route")
        .warmup
        .as_ref()
        .expect("a warmup")
        .schedule;

    let auto = schedule
        .share
        .auto()
        .expect("this tier exists to exercise share: {mode: auto}");
    assert_eq!(auto.ceiling, 0.5, "the tests assert against this ceiling");
    assert_eq!(auto.tail.below, 0.1, "and against this threshold");

    // A route that turns messages away may not be last in its chain (§4.2), and
    // `config()` would already have refused — but say so, because it is the
    // property that makes "the rest goes to overflow" true.
    assert!(cap() > 0);
}
