//! D-093's idle expiry, which is what returns F7's memory.
//!
//! Its own binary because `metrics` allows one global recorder per process, and
//! this one needs a timeout of a second rather than `tests/metrics_endpoint.rs`'s
//! default. `install` takes any duration; §4.2's one-minute floor is for
//! configuration, not for a test that would otherwise wait a minute.

use std::time::Duration;

const IDLE: Duration = Duration::from_millis(800);

fn unmatched_series(body: &str) -> Vec<&str> {
    body.lines()
        .filter(|l| l.starts_with("simmer_unmatched_sender_total{"))
        .collect()
}

#[test]
fn an_idle_counter_is_dropped_and_comes_back_from_zero() {
    let handle = simmer::metrics::install(IDLE).expect("the only recorder in this process");

    // F7's shape: one series per distinct domain.
    for i in 0..200 {
        simmer::metrics::unmatched_sender("main", &format!("d{i}.example"));
    }
    simmer::metrics::unmatched_sender("main", "kept.example");
    assert_eq!(unmatched_series(&handle.render()).len(), 201);

    // Half the timeout later, touch one of them. The exporter prunes while
    // rendering, so the render after the timeout is the one that drops.
    std::thread::sleep(IDLE / 2);
    simmer::metrics::unmatched_sender("main", "kept.example");
    std::thread::sleep(IDLE / 2 + Duration::from_millis(200));

    let body = handle.render();
    let left = unmatched_series(&body);
    assert_eq!(
        left,
        vec![r#"simmer_unmatched_sender_total{ramp="main",domain="kept.example"} 2"#],
        "every domain idle past the timeout is gone; the one touched in time stays"
    );

    // Seen again, a dropped domain starts from zero: a counter reset, which is
    // what Prometheus's rate() and increase() expect.
    simmer::metrics::unmatched_sender("main", "d7.example");
    assert!(handle
        .render()
        .contains(r#"simmer_unmatched_sender_total{ramp="main",domain="d7.example"} 1"#));
}
