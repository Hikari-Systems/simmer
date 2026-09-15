//! T4 — soak (docs/TESTING.md; the test programme's step 5).
//!
//! Hours of ordinary traffic, watching for the things that only appear over time:
//! memory that creeps, descriptors that are never given back, threads and tasks
//! that accumulate, rows that are written and never pruned. Nothing here is about
//! peak load — that is T3's job — and everything is about the difference between
//! a system that returns to where it started and one that does not.
//!
//! ```sh
//! # the stress stack's services; the soak differs only in SIMMER_CONFIG
//! SIMMER_CONFIG=/config/simmer.soak.yaml docker compose \
//!   -f docker-compose.yml -f test/compose/acceptance.yml -f test/compose/stress.yml \
//!   --profile acceptance --profile stress up -d --wait app app2 sink
//!
//! SOAK_DURATION=10m cargo test --test soak -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! ## Two tests, not one
//!
//! [`soak_run`] drives the load and writes every sample to `target/soak/*.csv` as
//! it goes; [`soak_analyze`] reads those files and judges them. They are separate
//! so that a twenty-four hour run which dies in its twenty-third hour still leaves
//! everything it learned on disk, and so that a verdict can be recomputed — with a
//! different warm-up, say — without paying for the run again.
//!
//! Samples are written on the **host**, not in a container. The instances are
//! re-created during a run, and anything inside them goes with them.
//!
//! ## What a short run can and cannot say
//!
//! `leak::verdict` needs eight post-warm-up floors before it will fail anything,
//! and a floor is the minimum over a five-minute window — so it takes forty
//! minutes of *steady state*, on top of the warm-up, before the slope gate means
//! anything. Below that it reports `inconclusive` and the run is a smoke test of
//! the machinery. That is deliberate: a slope through a handful of points is a
//! guess, and a gate built on a guess is a flake.
//!
//! ## V4 — relays cancelled by the session timeout (F2)
//!
//! A third stream per instance, on a sender (`cancel.soak.test`) and a route
//! (`warming-cancel`) of its own, so V2's pair and V3's series are untouched. The
//! sink holds each V4 message 15 s at the dot, so the 300 s session timeout lands
//! in the middle of a relay about once a session. S9 shows that F2 exists; V4 asks
//! what it costs over hours — in particular whether anything a cancellation
//! strands is ever given back. `SOAK_V4=off` leaves it out, for a like-for-like
//! comparison with a run from before it.
//!
//! ## At rest (step 5c)
//!
//! `soak_run` measures each instance before its first message and again once the
//! run is over — the load stopped, V4 drained, [`REST_SETTLE`] waited out — and
//! `soak_analyze` holds the second to the first (`soak/rest/baseline`): no
//! session, reservation, or pooled or database connection still in use, no more
//! threads or tasks than before, and no descriptor that was not open before and
//! that no pool accounts for. The same measurement completes the trend gate for
//! threads and descriptors, which move in whole units: a count that was back at
//! its baseline at rest was a ratchet, not a leak
//! (`leak::Verdict::released_at_rest`).
//!
//! ## Tracing a slow message
//!
//! Every soak route stamps `X-Simmer-Correlation: {{correlation_id}}` and the sink
//! records it. `soak_run` copies out every message a client waited more than
//! [`SLOW_MS`] for, and `soak_analyze` prints the slowest with the correlation id
//! Simmer's own log lines carry — so a latency tail is followed message by
//! message, where `docs/SOAK.md` §5 had to infer it from populations.

mod compose;

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use compose::leak;
use compose::reconcile::{self, Received, Sent};
use compose::stack::SOAK;

const SOAK_CONFIG: &str = "test/config/simmer.soak.yaml";

/// Where samples land, on the host.
const SAMPLE_DIR: &str = "target/soak";

/// The instances: `app` is scraped every 30 s, `app2` never is until the end.
/// The difference between them is what isolates a leak in the exporter itself
/// (F8) from one on the message path.
const INSTANCES: [&str; 2] = ["app", "app2"];

/// How often each instance is sampled.
const SAMPLE_EVERY: Duration = Duration::from_secs(10);

/// The floor window `leak::floors` uses, in seconds.
const FLOOR_WINDOW: f64 = 300.0;

/// V4's route, and the sender whose rule reaches it and nothing else.
const V4_ROUTE: &str = "warming-cancel";
const V4_SENDER: &str = "jane@cancel.soak.test";

/// How long the sink holds every V4 message at the dot before storing it.
const V4_SLOW: Duration = Duration::from_secs(15);

/// Messages per V4 session: more than the session timeout leaves room for, so
/// every session is ended by the timeout rather than by running out of mail.
const V4_PER_SESSION: u32 = 25;

/// `timeouts.session` in the soak config. V4's stream stops this long before the
/// run does, so its last session is cut inside the run rather than after it.
/// Kept as a constant because `soak_run` must not load the config: that sets
/// variables compose would then render `app` with.
const V4_LEAD_OUT: Duration = Duration::from_secs(300);

/// The loadgen's own wait for a reply (`read_reply` in `src/bin/loadgen.rs`).
const LOADGEN_REPLY_WAIT: Duration = Duration::from_secs(30);

/// §7.4's sweeper interval (`src/quota/sweeper.rs`).
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The most V4 reservation rows a sample may show. Per instance, one in flight
/// and one stranded awaiting the sweeper — expiry plus a sweep is under the
/// session timeout, so a stranded row is gone before the next cancellation — and
/// one more each for a sweep delayed by the shared database's stalls (§3a). A
/// sweeper that fell behind would pass this within the hour.
const V4_MAX_ROWS: u64 = 6;

/// How long the instances are left once the load and V4's drain are over before
/// they are measured at rest. Tokio's blocking pool keeps an idle thread for
/// 10 s, so a thread count read any sooner is still the load's.
const REST_SETTLE: Duration = Duration::from_secs(30);

/// A message a client waited longer than this for is traced: `soak_run` copies
/// out its loadgen record and its sink record, correlation id and all.
const SLOW_MS: f64 = 200.0;

/// At most this many slow messages per instance have their sink record copied
/// out, the slowest first: a run gone badly wrong has thousands.
const SLOW_TRACED: usize = 200;

// ---------------------------------------------------------------------------
// the one test that needs no Docker
// ---------------------------------------------------------------------------

#[test]
fn the_soak_config_is_valid_and_cannot_run_out_of_allowance() {
    let cfg = compose::configs::load(SOAK_CONFIG);

    // The warming route has to survive the whole run: 24 h at 10 msg/s is ~864k
    // messages, and a route that quietly exhausted its allowance after an hour
    // would leave the soak measuring overflow for the other twenty-three.
    let warming = cfg.route("warming-newbrand").expect("a warming route");
    let schedule = &warming
        .warmup
        .as_ref()
        .expect("a warming route has a warm-up")
        .schedule
        .default;
    assert!(
        schedule.iter().all(|&n| n > 1_000_000),
        "the soak's allowance must outlast a 24 h run: {schedule:?}"
    );

    assert_eq!(
        cfg.server.max_concurrent_sessions, 64,
        "the soak runs at the real session ceiling"
    );

    // V4 (F2). Every one of these is a way for the variant to stop cancelling
    // relays while still looking as if it runs.
    let cancel = cfg.route(V4_ROUTE).expect("V4's route");
    assert!(
        cancel
            .warmup
            .as_ref()
            .expect("V4's route is a warming route: its reservation is what F2 strands")
            .schedule
            .default
            .iter()
            .all(|&n| n > 1_000_000),
        "V4's allowance must outlast a 24 h run"
    );
    let domain = V4_SENDER.split_once('@').expect("an address").1;
    let rule = cfg
        .senders
        .iter()
        .find(|r| r.pattern == domain)
        .expect("a sender rule for V4's domain");
    assert_eq!(
        rule.chain,
        [V4_ROUTE],
        "V4's sender reaches V4's route alone"
    );

    let session = cfg.server.timeouts.session;
    assert_eq!(
        session, V4_LEAD_OUT,
        "soak_run stops V4 one session timeout early"
    );
    let data = cancel
        .downstream
        .timeouts
        .as_ref()
        .and_then(|t| t.data)
        .expect("V4's route sets a data timeout");
    // Held past the route's own data timeout, a message would take §10.2's
    // ambiguous path instead of being cancelled — and F2 would look fixed.
    assert!(
        V4_SLOW < data,
        "the sink's hold must end inside the route's data timeout"
    );
    assert!(
        V4_SLOW < LOADGEN_REPLY_WAIT,
        "the loadgen must wait out the sink's hold"
    );
    assert!(
        V4_SLOW * V4_PER_SESSION > session,
        "every V4 session must still be sending when the session timeout fires"
    );
    assert!(
        simmer::quota::reservation_expiry(cancel, 1) + SWEEP_INTERVAL < session,
        "a stranded reservation must be swept before the next session's cancellation"
    );

    // Step 5c: every route stamps the id Simmer logs a message under, so a slow
    // message can be followed from the loadgen's record into the server's log.
    // Not declared in `unstable_headers`: §6.6's probe pins volatile variables,
    // so the header is stable, and declaring it would draw the stale-declaration
    // WARN instead.
    for route in &cfg.routes {
        assert!(
            route
                .identity
                .set_headers
                .iter()
                .any(|(k, v)| k == "X-Simmer-Correlation" && v == "{{correlation_id}}"),
            "{} does not stamp X-Simmer-Correlation",
            route.name
        );
    }
    let warnings = simmer::config::validate::warnings(&cfg);
    assert!(
        !warnings
            .iter()
            .any(|w| w.path.ends_with("unstable_headers")),
        "{warnings:?}"
    );
}

// ---------------------------------------------------------------------------
// the run
// ---------------------------------------------------------------------------

#[test]
#[ignore = "needs the stress compose profile; SOAK_DURATION sets the length"]
fn soak_run() {
    let duration = duration_from_env("SOAK_DURATION", "10m");
    let rate = std::env::var("SOAK_RATE")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(10.0);
    eprintln!(
        "soak: {:.0} minutes at {rate} msg/s per instance, sampling every {}s",
        duration.as_secs_f64() / 60.0,
        SAMPLE_EVERY.as_secs()
    );

    fs::create_dir_all(SAMPLE_DIR).expect("creating the sample directory");
    // Start from empty. The samplers append, and every run's clock restarts at
    // zero, so leaving the last run's rows in place would interleave two series
    // and make the slope through them meaningless.
    for instance in INSTANCES {
        let _ = fs::remove_file(csv_path(instance));
        let _ = fs::remove_file(final_path(instance));
        let _ = fs::remove_file(sample_path(&format!("v4-{instance}.jsonl")));
        let _ = fs::remove_file(base_prom_path(instance));
        let _ = fs::remove_file(rest_path(instance));
        let _ = fs::remove_file(fds_path(instance, "base"));
        let _ = fs::remove_file(fds_path(instance, "rest"));
        let _ = fs::remove_file(sample_path(&format!("slow-{instance}.jsonl")));
    }
    for file in ["metrics.csv", "v4.csv", "v4-sink.jsonl", "slow-sink.jsonl"] {
        let _ = fs::remove_file(sample_path(file));
    }
    recreate_instances();
    SOAK.reset_quota();
    fresh_sink();

    // Baseline before a single message: the return-to-baseline checks are all
    // relative to this, so it is taken after the instances are up and settled.
    thread::sleep(Duration::from_secs(5));
    for instance in INSTANCES {
        if let Some(sample) = sample_instance(instance) {
            append_sample(instance, 0.0, &sample);
        }
        // What `soak/rest/baseline` holds the end to: what each descriptor is,
        // then the gauges — listed first so the scrape's own connection is not
        // among them. `app2` is scraped here, before its first message, when the
        // exporter holds nothing a scrape could drain; V2's asymmetry is about
        // draining it (F8), so it is untouched.
        if let Some(listing) = list_fds(instance) {
            let _ = fs::write(fds_path(instance, "base"), listing);
        }
        if let Some(body) = scrape_in_container(instance) {
            let _ = fs::write(base_prom_path(instance), body);
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let samplers: Vec<_> = INSTANCES
        .iter()
        .map(|instance| {
            let flag = Arc::clone(&stop);
            let name = instance.to_string();
            thread::spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    if let Some(sample) = sample_instance(&name) {
                        append_sample(&name, started.elapsed().as_secs_f64(), &sample);
                    }
                    thread::sleep(SAMPLE_EVERY);
                }
            })
        })
        .collect();

    // V2 — `app` is scraped every 30 s and `app2` never is. The two carry
    // identical streams, so a difference between their slopes is the exporter's
    // own doing (F8) rather than the message path's; without this, a difference
    // between the instances means nothing at all.
    //
    // The series count is recorded alongside, because unbounded label cardinality
    // (F7, driven by V3) is a growth curve like any other — and so is V4's
    // `reservations_in_flight`, which only `app` can show while the load runs.
    let scraper = {
        let flag = Arc::clone(&stop);
        thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                if let Some(scrape) = scrape_app(started.elapsed().as_secs_f64()) {
                    append_metrics(&scrape);
                }
                thread::sleep(Duration::from_secs(30));
            }
        })
    };

    // One open-loop stream per instance, identical but for the host they point
    // at, so anything that differs between them is the instances' doing.
    let seconds = format!("{}s", duration.as_secs());
    let loads: Vec<_> = INSTANCES
        .iter()
        .map(|instance| {
            let args = vec![
                "--host".to_string(),
                (*instance).to_string(),
                "--port".to_string(),
                "25".to_string(),
                "--username".to_string(),
                "soakapp".to_string(),
                "--from".to_string(),
                "jane@oldbrand.com".to_string(),
                "--tag".to_string(),
                format!("soak-{instance}"),
                "--rate".to_string(),
                rate.to_string(),
                "--duration".to_string(),
                seconds.clone(),
                "--concurrency".to_string(),
                "16".to_string(),
                "--per-session".to_string(),
                "10".to_string(),
                "--size".to_string(),
                "dist:4k:80,100k:15,1m:4,4m:1".to_string(),
                // V3 (F7): one message in twenty from a domain nobody has seen
                // before, granted by the ACL but matched by no `senders:` rule,
                // so it falls through to `default_chain` and mints a new
                // `simmer_unmatched_sender_total{domain}` series.
                "--fresh-sender".to_string(),
                "--jsonl".to_string(),
                format!("/results/soak-{instance}.jsonl"),
            ];
            let name = instance.to_string();
            thread::spawn(move || run_loadgen(&name, &args))
        })
        .collect();

    // V4 (F2) — a third stream per instance, identical on both so that V2's pair
    // stays a pair, with the route's quota row sampled from the database: the one
    // view of it that includes `app2` without scraping it.
    let v4 = v4_enabled() && duration >= 2 * V4_LEAD_OUT;
    if v4_enabled() && !v4 {
        eprintln!(
            "soak: V4 skipped: a run under {} minutes has no room for a cancellation",
            (2 * V4_LEAD_OUT).as_secs() / 60
        );
    }
    let v4_stop = Arc::new(AtomicBool::new(false));
    let v4_sampler = v4.then(|| {
        let flag = Arc::clone(&v4_stop);
        thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                if let Some(ledger) = sample_v4_ledger() {
                    append_v4_ledger(started.elapsed().as_secs_f64(), &ledger);
                }
                thread::sleep(Duration::from_secs(30));
            }
        })
    });
    let v4_loads: Vec<_> = if v4 {
        INSTANCES
            .iter()
            .map(|instance| {
                let args = v4_args(instance, duration - V4_LEAD_OUT);
                let name = format!("{instance} V4");
                thread::spawn(move || run_loadgen(&name, &args))
            })
            .collect()
    } else {
        Vec::new()
    };

    for load in loads {
        let summary = load.join().expect("a load thread");
        eprintln!("  {summary}");
    }

    stop.store(true, Ordering::Relaxed);
    for sampler in samplers {
        let _ = sampler.join();
    }
    let _ = scraper.join();
    eprintln!(
        "soak: load finished after {:.1} minutes; samples in {SAMPLE_DIR}/",
        started.elapsed().as_secs_f64() / 60.0
    );

    for load in v4_loads {
        let summary = load.join().expect("a V4 load thread");
        eprintln!("  {summary}");
    }
    v4_stop.store(true, Ordering::Relaxed);
    if let Some(sampler) = v4_sampler {
        let _ = sampler.join();
    }
    if v4 {
        if drain_v4(started) {
            eprintln!("soak: the sweeper has cleared V4's stranded reservations");
        } else {
            eprintln!("soak: V4's reservations had not drained; soak_analyze will say so");
        }
    }

    // At rest: the load over, V4 drained, and long enough since for the blocking
    // pool's idle threads to have exited. Sampled before the final scrape, so
    // the scrape's own connection is not among the descriptors.
    thread::sleep(REST_SETTLE);
    for instance in INSTANCES {
        match sample_instance(instance) {
            Some(sample) => append_sample_to(
                &rest_path(instance),
                started.elapsed().as_secs_f64(),
                &sample,
            ),
            None => eprintln!("soak: {instance} could not be sampled at rest"),
        }
        if let Some(listing) = list_fds(instance) {
            let _ = fs::write(fds_path(instance, "rest"), listing);
        }
    }

    // The last scrape of each instance — `app2`'s only one since before its first
    // message — and the evidence the analyser reads for the state each was left in.
    for instance in INSTANCES {
        match scrape_in_container(instance) {
            Some(body) => {
                let _ = fs::write(final_path(instance), body);
            }
            None => eprintln!("soak: {instance} did not answer its final scrape"),
        }
    }
    if v4 {
        copy_v4_evidence();
    }
    copy_slow_evidence();
    eprintln!(
        "soak: finished after {:.1} minutes",
        started.elapsed().as_secs_f64() / 60.0
    );
}

// ---------------------------------------------------------------------------
// the verdict
// ---------------------------------------------------------------------------

#[test]
#[ignore = "reads target/soak/*.csv from a previous soak_run"]
fn soak_analyze() {
    let mut judged = 0;
    // Every instance is analysed before anything fails. `app` and `app2` carry
    // identical streams and differ only in that `app` is scraped, so `app2`'s
    // numbers are what make `app`'s mean anything (F8) — and a bare `assert!`
    // inside the loop threw exactly that away, ending the run on the first
    // instance and leaving the comparison to be rebuilt by hand.
    let mut failures: Vec<String> = Vec::new();
    let mut baseline: Vec<String> = Vec::new();
    let mut baseline_judged = false;
    for instance in INSTANCES {
        let samples = read_samples(instance);
        if samples.is_empty() {
            eprintln!("{instance}: no samples; run soak_run first");
            continue;
        }
        judged += 1;
        let rest = read_rest(instance, &samples[0]);

        let span = samples.last().expect("samples").t - samples[0].t;
        let warmup = warmup_for(span);

        eprintln!(
            "\n{instance}: {} samples over {:.1} minutes, warm-up {:.1} minutes",
            samples.len(),
            span / 60.0,
            warmup / 60.0
        );

        for (name, series, limit, unit) in [
            (
                "anon",
                series(&samples, |s| s.anon as f64),
                2.0 * 1_048_576.0,
                "MiB/h",
            ),
            ("fds", series(&samples, |s| s.fds as f64), 1.0, "fds/h"),
            (
                "threads",
                series(&samples, |s| s.threads as f64),
                1.0,
                "threads/h",
            ),
        ] {
            let mut v = leak::verdict(
                &series,
                warmup,
                FLOOR_WINDOW,
                leak::Limits {
                    slope_per_hour: limit,
                },
            );
            // Threads and descriptors move in whole units, so the trend alone
            // cannot tell a ratchet from a climb: V4's hour failed on one +1
            // that was gone at rest. A count back at its baseline once the load
            // stopped was the former.
            match (name, &rest) {
                (
                    "threads",
                    Rest {
                        threads: Some((before, after)),
                        ..
                    },
                ) => v = v.released_at_rest(*before, *after, 0.0),
                (
                    "fds",
                    Rest {
                        unaccounted_fds: Some(extra),
                        ..
                    },
                ) => v = v.released_at_rest(0.0, extra.len() as f64, 0.0),
                _ => {}
            }
            let scale = if unit == "MiB/h" { 1_048_576.0 } else { 1.0 };
            eprintln!(
                "  {name:<8} slope {:+.2} {unit}, quartile step {:+.2}{}{}",
                v.slope_per_hour / scale,
                v.quartile_step / scale,
                if unit == "MiB/h" { " MiB" } else { "" },
                if v.inconclusive {
                    "  (inconclusive: fewer than eight post-warm-up floors)"
                } else if v.released {
                    "  a ratchet: the trend rose, and it was back at its baseline at rest"
                } else if v.leaking {
                    "  LEAKING"
                } else {
                    ""
                }
            );
            if v.leaking {
                failures.push(format!(
                    "{instance}: {name} is growing at {:+.2} {unit} with a quartile step of {:+.2}",
                    v.slope_per_hour / scale,
                    v.quartile_step / scale
                ));
            }
        }

        // Hard bounds, at every sample rather than on the trend: these are not
        // allowed to drift even briefly.
        let peak_established = samples.iter().map(|s| s.established).max().unwrap_or(0);
        let peak_close_wait = samples.iter().map(|s| s.close_wait).max().unwrap_or(0);
        eprintln!("  peak established {peak_established}, peak CLOSE_WAIT {peak_close_wait}");
        if peak_close_wait > 64 {
            failures.push(format!(
                "{instance}: CLOSE_WAIT peaked at {peak_close_wait}, which is more sockets \
                 half-closed than every downstream pool put together"
            ));
        }

        let pair = |p: Option<(f64, f64)>| {
            p.map_or("not measured".to_string(), |(before, after)| {
                format!("{after} (before the first message: {before})")
            })
        };
        eprintln!(
            "  at rest: threads {}, tasks {}, unaccounted descriptors {}",
            pair(rest.threads),
            pair(rest.tasks),
            rest.unaccounted_fds.as_ref().map_or(
                "not measured (a run from before step 5c)".to_string(),
                |extra| extra.len().to_string()
            )
        );
        if rest.final_prom.is_some() {
            baseline_judged = true;
            baseline.extend(baseline_problems(instance, &rest));
        } else {
            eprintln!("  no final scrape of {instance}; its return to baseline is not judged");
        }
    }
    // F7 — `simmer_unmatched_sender_total{domain}` takes its label from a domain
    // the client chose, so V3's fresh `u<n>.soak.test` senders mint a new series
    // every time one arrives. Judged from the post-warm-up baseline, not from
    // zero: the series present at start-up are the configured routes' and are
    // nobody's defect.
    let scrapes = read_metrics();
    // V2 is `app` scraped and `app2` not. With no scrapes at all there is no
    // asymmetry, no F7 verdict and no registry curve — and nothing else says so.
    if judged > 0 && scrapes.is_empty() {
        failures.push(
            "app was never scraped (no metrics.csv): V2's asymmetry and F7 went unmeasured"
                .to_string(),
        );
    }
    let after: Vec<Scrape> = if scrapes.len() < 2 {
        Vec::new()
    } else {
        let warmup = warmup_for(scrapes.last().expect("scrapes").t - scrapes[0].t);
        scrapes.iter().copied().filter(|s| s.t >= warmup).collect()
    };
    match (after.first(), after.last()) {
        (Some(first), Some(last)) if last.t > first.t => {
            eprintln!(
                "\nmetrics: {} series ({} unmatched-sender) at {:.0}s -> {} ({}) at {:.0}s, \
                 across {} post-warm-up scrapes",
                first.series,
                first.unmatched,
                first.t,
                last.series,
                last.unmatched,
                last.t,
                after.len()
            );
            let grew = last.unmatched.saturating_sub(first.unmatched);
            let result = if grew > ALLOWED_SERIES_GROWTH {
                Err(format!(
                    "metric series grew by {grew} (unmatched-sender {} -> {}) over \
                     {:.0} minutes; the domain label is client-controlled",
                    first.unmatched,
                    last.unmatched,
                    (last.t - first.t) / 60.0
                ))
            } else {
                Ok(())
            };
            verdict(&mut failures, "soak/V3/metric-series", result);
        }
        _ => eprintln!("\nmetrics: too few post-warm-up scrapes for a series verdict"),
    }

    analyze_v4(&mut failures);
    if baseline_judged {
        verdict(&mut failures, "soak/rest/baseline", joined(baseline));
    }
    report_slow();

    assert!(judged > 0, "no samples at all; run soak_run first");
    assert!(
        failures.is_empty(),
        "{} checks failed:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

/// W = clamp(0.15 x D, 10 min, 90 min), overridable with `SOAK_WARMUP` so a short
/// run can still drive the whole analysis rather than being all warm-up.
fn warmup_for(span: f64) -> f64 {
    match std::env::var("SOAK_WARMUP") {
        Ok(v) => duration_str(&v).as_secs_f64(),
        Err(_) => (0.15 * span).clamp(600.0, 5400.0),
    }
}

/// Judge one check against `test/known-findings.json`, collecting a real failure
/// or an XPASS rather than panicking, so every check is reported before any fails.
fn verdict(failures: &mut Vec<String>, check: &str, result: Result<(), String>) {
    if let Err(e) = compose::findings::assess(check, result) {
        failures.push(e);
    }
}

fn joined(problems: Vec<String>) -> Result<(), String> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// V4 (F2): what the session timeout's cancellations cost, judged from what
/// `soak_run` copied to the host once the load and the drain were over.
fn analyze_v4(failures: &mut Vec<String>) {
    let sent: Vec<(&str, Vec<Sent>)> = INSTANCES
        .iter()
        .filter_map(|i| read_sample_jsonl(&format!("v4-{i}.jsonl")).map(|s| (*i, s)))
        .collect();
    if sent.is_empty() {
        eprintln!("\nV4: no evidence in {SAMPLE_DIR}/ (a run from before V4, or SOAK_V4=off)");
        return;
    }
    let received: Vec<Received> = read_sample_jsonl("v4-sink.jsonl").unwrap_or_default();
    let ledger = read_v4_ledger();
    let finals: Vec<(&str, Option<String>)> = INSTANCES
        .iter()
        .map(|i| (*i, fs::read_to_string(final_path(i)).ok()))
        .collect();

    let (mut driven, mut delivery, mut accounting, mut registry) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut registry_judged = false;
    let mut stored = 0;
    for (instance, sent) in &sent {
        let prefix = format!("soak-v4-{instance}-");
        let mine: Vec<Received> = received
            .iter()
            .filter(|r| r.id.starts_with(&prefix))
            .cloned()
            .collect();
        // Cut by the session timeout: `421 … session timeout`, at whatever stage
        // it caught the client. At the dot, it caught a relay.
        let cut: BTreeSet<&str> = sent
            .iter()
            .filter(|s| s.code == 421 && s.text.contains("session timeout"))
            .map(|s| s.id.as_str())
            .collect();
        let cut_at_dot = sent
            .iter()
            .filter(|s| cut.contains(s.id.as_str()) && s.stage == "dot")
            .count();
        // One client, so each session is the next run of consecutive ids.
        let sessions: BTreeSet<u64> = sent
            .iter()
            .filter_map(|s| s.id.rsplit('-').next()?.parse::<u64>().ok())
            .map(|n| n / u64::from(V4_PER_SESSION))
            .collect();

        let report = reconcile::reconcile(sent, &mine, None);
        // F2's signature among reconcile's violations: a relay the timeout cut,
        // answered 421, that the sink went on to store. Anything else is not F2.
        let (lies, other): (Vec<&String>, Vec<&String>) = report.violations.iter().partition(|v| {
            v.contains("but the message was delivered")
                && v.split_once(':').is_some_and(|(id, _)| cut.contains(id))
        });
        stored += report.stored;
        eprintln!(
            "\nV4 {instance}: {} messages in {} sessions — {} accepted, {} cut by the \
             session timeout ({cut_at_dot} at the dot, {} of them stored by the sink), \
             {} ended with the connection",
            sent.len(),
            sessions.len(),
            report.accepted,
            cut.len(),
            lies.len(),
            report.transport
        );

        if cut_at_dot == 0 || cut_at_dot * 2 < sessions.len() {
            driven.push(format!(
                "{instance}: {cut_at_dot} relays cut at the dot in {} sessions; the session \
                 timeout is not cancelling relays, so F2 is not being driven",
                sessions.len()
            ));
        }
        // Accepted, cut by the timeout, or never sent because the timeout closed
        // the connection under it (code 0). Nothing else is V4's doing.
        for s in sent {
            if !(matches!(s.code, 0 | 250) || cut.contains(s.id.as_str())) {
                delivery.push(format!(
                    "{instance}: {} {} at {}: {}",
                    s.id, s.code, s.stage, s.text
                ));
            }
        }
        delivery.extend(other.iter().map(|v| format!("{instance}: {v}")));
        if !lies.is_empty() {
            accounting.push(format!(
                "{instance}: {} of the {} relays the session timeout cut were stored without \
                 a reply the client could trust — told 421, so its retry delivers each twice",
                lies.len(),
                cut.len()
            ));
        }

        match finals
            .iter()
            .find(|(i, _)| i == instance)
            .and_then(|(_, body)| body.as_deref())
        {
            Some(body) => {
                registry_judged = true;
                let held = metric(body, "simmer_reservations_in_flight").unwrap_or(0.0);
                eprintln!(
                    "  reservations_in_flight {held} after the drain, against {} cut",
                    cut.len()
                );
                if held != 0.0 {
                    registry.push(format!(
                        "{instance}: reservations_in_flight {held} after the drain, against {} \
                         relays the session timeout cut; only commit, release or shutdown \
                         removes an entry from the registry",
                        cut.len()
                    ));
                }
            }
            None => eprintln!("  no final scrape of {instance}; its registry is not judged"),
        }
    }

    // `app`'s registry while the load ran — the growth curve, where `app2` has
    // only its final value.
    let curve: Vec<(f64, f64)> = read_metrics()
        .iter()
        .filter_map(|s| Some((s.t, s.in_flight?)))
        .collect();
    if let (Some(a), Some(b)) = (curve.first(), curve.last()) {
        if b.0 > a.0 {
            eprintln!(
                "\nV4 app: reservations_in_flight {} at {:.0}s -> {} at {:.0}s while the load \
                 ran ({:+.1}/h)",
                a.1,
                a.0,
                b.1,
                b.0,
                (b.1 - a.1) / (b.0 - a.0) * 3600.0
            );
        }
    }

    // The route's quota row, shared by both instances: its ledger against what
    // the sink stored, and whether the sweeper kept the stranded rows bounded
    // and cleared them once the load stopped.
    let mut sweeper = Vec::new();
    match ledger.last() {
        Some((_, last)) => {
            let peak = ledger.iter().map(|(_, l)| l.rows).max().unwrap_or(0);
            let expired: f64 = finals
                .iter()
                .filter_map(|(_, body)| {
                    metric(
                        body.as_deref()?,
                        &format!("simmer_reservation_expired_total{{route=\"{V4_ROUTE}\"}}"),
                    )
                })
                .sum();
            eprintln!(
                "\nV4 {V4_ROUTE}: at most {peak} reservation rows across {} samples; at the \
                 end {} rows, {} reserved, {} committed; {expired} reservations expired by the \
                 sweeper",
                ledger.len(),
                last.rows,
                last.reserved,
                last.committed
            );
            if last.committed != stored as u64 {
                accounting.push(format!(
                    "the {V4_ROUTE} ledger committed {} of the {stored} V4 messages the sink \
                     stored",
                    last.committed
                ));
            }
            if peak > V4_MAX_ROWS {
                sweeper.push(format!(
                    "{peak} {V4_ROUTE} reservation rows at once, against a bound of \
                     {V4_MAX_ROWS}: stranded rows are accumulating faster than they are swept"
                ));
            }
            if last.rows != 0 || last.reserved != 0 {
                sweeper.push(format!(
                    "{} {V4_ROUTE} reservation rows and {} reserved were still held once the \
                     load had stopped and the drain had timed out",
                    last.rows, last.reserved
                ));
            }
        }
        None => {
            eprintln!("\nV4: no ledger samples; the route's ledger and the sweeper are not judged")
        }
    }

    verdict(failures, "soak/V4/driven", joined(driven.clone()));
    verdict(failures, "soak/V4/delivery", joined(delivery));
    // With nothing cancelled, the F2 checks would pass for want of a cancellation
    // and report an XPASS that means nothing; `driven` has already failed.
    if driven.is_empty() {
        verdict(failures, "soak/V4/accounting", joined(accounting));
        if registry_judged {
            verdict(failures, "soak/V4/registry", joined(registry));
        }
    }
    if !ledger.is_empty() {
        verdict(failures, "soak/V4/sweeper", joined(sweeper));
    }
}

// ---------------------------------------------------------------------------
// sampling
// ---------------------------------------------------------------------------

/// One instance at one moment. Everything here comes from a single `docker exec`,
/// because the runtime image has no `ps` and no `ss` and because one exec per tick
/// per instance is already the harness's largest cost.
#[derive(Debug, Default, Clone, Copy)]
struct Sample {
    t: f64,
    vmrss_kb: u64,
    threads: u64,
    fds: u64,
    anon: u64,
    shmem: u64,
    file: u64,
    cpu_usec: u64,
    tmp_used_kb: u64,
    established: u64,
    close_wait: u64,
}

const SAMPLE_CMD: &str = "grep -E '^(VmRSS|Threads)' /proc/1/status; \
     ls /proc/1/fd | wc -l; \
     grep -E '^(anon|shmem|file) ' /sys/fs/cgroup/memory.stat; \
     grep usage_usec /sys/fs/cgroup/cpu.stat; \
     df -k /tmp | tail -1; \
     awk 'NR>1{print $4}' /proc/1/net/tcp";

fn sample_instance(instance: &str) -> Option<Sample> {
    let out = SOAK
        .compose()
        .args(["exec", "-T", instance, "sh", "-c", SAMPLE_CMD])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut s = Sample::default();
    let mut seen_fd_count = false;
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            ["VmRSS:", v, ..] => s.vmrss_kb = v.parse().unwrap_or(0),
            ["Threads:", v] => s.threads = v.parse().unwrap_or(0),
            ["anon", v] => s.anon = v.parse().unwrap_or(0),
            ["shmem", v] => s.shmem = v.parse().unwrap_or(0),
            ["file", v] => s.file = v.parse().unwrap_or(0),
            ["usage_usec", v] => s.cpu_usec = v.parse().unwrap_or(0),
            // `df -k` on the tmpfs: filesystem, 1K-blocks, used, available, ...
            [_fs, _blocks, used, _avail, _pct, mount] if *mount == "/tmp" => {
                s.tmp_used_kb = used.parse().unwrap_or(0)
            }
            // A bare hexadecimal state from /proc/1/net/tcp: 01 established,
            // 08 close-wait. Everything else (listening, time-wait) is ignored.
            ["01"] => s.established += 1,
            ["08"] => s.close_wait += 1,
            [only] if !seen_fd_count => {
                // The first bare number is `ls /proc/1/fd | wc -l`.
                if let Ok(n) = only.parse() {
                    s.fds = n;
                    seen_fd_count = true;
                }
            }
            _ => {}
        }
    }
    Some(s)
}

/// One scrape of `app`, reduced to what the analyser judges.
#[derive(Debug, Clone, Copy)]
struct Scrape {
    t: f64,
    /// Every series `/metrics` is exporting.
    series: usize,
    /// The `simmer_unmatched_sender_total` ones (F7).
    unmatched: usize,
    /// `simmer_reservations_in_flight` (V4). `None` in a file from before V4.
    in_flight: Option<f64>,
}

/// Scrape `app`, from inside its container like the final scrapes.
///
/// Not `curl` to the published port: that is the Docker host's loopback, which a
/// harness running anywhere else — a jail, a CI container — cannot reach, and a
/// failed scrape here is simply a sample not written. That cost a run: `app` went
/// unscraped for twenty minutes, V2's asymmetry silently vanished, and F7 was
/// "too few scrapes" rather than a verdict.
///
/// Every non-comment, non-blank line is one series, labels included. The total is
/// context; the judgement is made on the unmatched-sender count alone, because the
/// total moves for reasons that are nobody's defect — a new route label, a new
/// outcome — and gating on it would quietly turn this into a different check.
fn scrape_app(t: f64) -> Option<Scrape> {
    let body = scrape_in_container("app")?;
    let live = || {
        body.lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    };
    Some(Scrape {
        t,
        series: live().count(),
        unmatched: live()
            .filter(|l| l.starts_with("simmer_unmatched_sender_total{"))
            .count(),
        in_flight: metric(&body, "simmer_reservations_in_flight"),
    })
}

/// One unlabelled series — or one exactly as labelled — from a scrape.
fn metric(body: &str, series: &str) -> Option<f64> {
    body.lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
        .and_then(|v| v.trim().parse().ok())
}

/// §9.1's own series move for innocent reasons — a route added, an outcome first
/// seen — so a handful of new ones is not the defect. F7 is unbounded growth, and
/// 5% of an hour's messages is ~1,700 fresh domains against this.
const ALLOWED_SERIES_GROWTH: usize = 50;

fn append_metrics(s: &Scrape) {
    let path = sample_path("metrics.csv");
    let fresh = !path.exists();
    let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    if fresh {
        let _ = writeln!(f, "t,series,unmatched,reservations_in_flight");
    }
    let in_flight = s.in_flight.map(|v| v.to_string()).unwrap_or_default();
    let _ = writeln!(f, "{:.1},{},{},{in_flight}", s.t, s.series, s.unmatched);
}

/// `metrics.csv`, including one from before V4, which has no fourth column.
fn read_metrics() -> Vec<Scrape> {
    let Ok(text) = fs::read_to_string(sample_path("metrics.csv")) else {
        return Vec::new();
    };
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let mut f = line.split(',');
            Some(Scrape {
                t: f.next()?.trim().parse().ok()?,
                series: f.next()?.trim().parse().ok()?,
                unmatched: f.next()?.trim().parse().ok()?,
                in_flight: f.next().and_then(|v| v.trim().parse().ok()),
            })
        })
        .collect()
}

fn sample_path(file: &str) -> PathBuf {
    PathBuf::from(SAMPLE_DIR).join(file)
}

fn csv_path(instance: &str) -> PathBuf {
    sample_path(&format!("{instance}.csv"))
}

/// Each instance's `/metrics` once the run is over.
fn final_path(instance: &str) -> PathBuf {
    sample_path(&format!("final-{instance}.prom"))
}

fn read_sample_jsonl<T: serde::de::DeserializeOwned>(file: &str) -> Option<Vec<T>> {
    fs::read_to_string(sample_path(file))
        .ok()
        .map(|text| reconcile::read_jsonl(&text))
}

fn append_sample(instance: &str, t: f64, s: &Sample) {
    append_sample_to(&csv_path(instance), t, s);
}

fn append_sample_to(path: &Path, t: f64, s: &Sample) {
    let fresh = !path.exists();
    let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    if fresh {
        let _ = writeln!(
            f,
            "t,vmrss_kb,threads,fds,anon,shmem,file,cpu_usec,tmp_used_kb,established,close_wait"
        );
    }
    let _ = writeln!(
        f,
        "{t:.1},{},{},{},{},{},{},{},{},{},{}",
        s.vmrss_kb,
        s.threads,
        s.fds,
        s.anon,
        s.shmem,
        s.file,
        s.cpu_usec,
        s.tmp_used_kb,
        s.established,
        s.close_wait
    );
}

fn read_samples(instance: &str) -> Vec<Sample> {
    read_samples_from(&csv_path(instance))
}

fn read_samples_from(path: &Path) -> Vec<Sample> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            if f.len() < 11 {
                return None;
            }
            let n = |i: usize| f[i].parse().unwrap_or(0);
            Some(Sample {
                t: f[0].parse().unwrap_or(0.0),
                vmrss_kb: n(1),
                threads: n(2),
                fds: n(3),
                anon: n(4),
                shmem: n(5),
                file: n(6),
                cpu_usec: n(7),
                tmp_used_kb: n(8),
                established: n(9),
                close_wait: n(10),
            })
        })
        .collect()
}

fn series(samples: &[Sample], pick: impl Fn(&Sample) -> f64) -> Vec<(f64, f64)> {
    samples.iter().map(|s| (s.t, pick(s))).collect()
}

// ---------------------------------------------------------------------------
// at rest — the return to baseline (step 5c)
// ---------------------------------------------------------------------------

/// Each instance's `/metrics` before its first message.
fn base_prom_path(instance: &str) -> PathBuf {
    sample_path(&format!("base-{instance}.prom"))
}

/// One sample of each instance at rest, in the samples' own CSV format.
fn rest_path(instance: &str) -> PathBuf {
    sample_path(&format!("rest-{instance}.csv"))
}

/// `ls -l /proc/1/fd` before the first message (`base`) or at rest (`rest`).
fn fds_path(instance: &str, when: &str) -> PathBuf {
    sample_path(&format!("fds-{when}-{instance}.txt"))
}

/// What each descriptor is, not only how many, so a return-to-baseline failure
/// names what was left open.
fn list_fds(instance: &str) -> Option<String> {
    let out = SOAK
        .compose()
        .args(["exec", "-T", instance, "ls", "-l", "/proc/1/fd"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// What an instance held before its first message and once the run was over.
/// Every part is optional: a run from before step 5c has only the first sample
/// and the final scrape, and is judged on what it has.
struct Rest {
    /// Threads before and after: the first sample's, and the rest sample's or,
    /// failing that, the final scrape's `process_threads`.
    threads: Option<(f64, f64)>,
    /// `simmer_tasks_alive` before and after.
    tasks: Option<(f64, f64)>,
    /// Descriptors open at rest that nothing accounts for
    /// ([`leak::unaccounted_fds`]).
    unaccounted_fds: Option<Vec<String>>,
    /// The final scrape, for the gauges that must simply be zero.
    final_prom: Option<String>,
}

fn read_rest(instance: &str, first: &Sample) -> Rest {
    let base_prom = fs::read_to_string(base_prom_path(instance)).ok();
    let final_prom = fs::read_to_string(final_path(instance)).ok();
    let threads_at_rest = read_samples_from(&rest_path(instance))
        .pop()
        .map(|s| s.threads as f64)
        .or_else(|| metric(final_prom.as_deref()?, "process_threads"));
    Rest {
        threads: threads_at_rest.map(|after| (first.threads as f64, after)),
        tasks: before_after(
            base_prom.as_deref(),
            final_prom.as_deref(),
            "simmer_tasks_alive",
        ),
        unaccounted_fds: unaccounted_fds(instance, base_prom.as_deref(), final_prom.as_deref()),
        final_prom,
    }
}

fn before_after(before: Option<&str>, after: Option<&str>, series: &str) -> Option<(f64, f64)> {
    Some((metric(before?, series)?, metric(after?, series)?))
}

fn unaccounted_fds(
    instance: &str,
    base_prom: Option<&str>,
    final_prom: Option<&str>,
) -> Option<Vec<String>> {
    let base = leak::fd_kinds(&fs::read_to_string(fds_path(instance, "base")).ok()?);
    let rest = leak::fd_kinds(&fs::read_to_string(fds_path(instance, "rest")).ok()?);
    Some(leak::unaccounted_fds(
        &base,
        &rest,
        pooled(base_prom?),
        pooled(final_prom?),
    ))
}

/// Every connection the downstream and database pools report holding; each is
/// one socket.
fn pooled(body: &str) -> f64 {
    series_matching(body, "simmer_pool_connections{")
        .chain(series_matching(body, "simmer_db_pool_connections{"))
        .map(|(_, v)| v)
        .sum()
}

/// Every series whose name, labels included, starts with `prefix`, and its value.
fn series_matching<'a>(
    body: &'a str,
    prefix: &'a str,
) -> impl Iterator<Item = (&'a str, f64)> + 'a {
    body.lines()
        .filter(move |l| l.starts_with(prefix))
        .filter_map(|l| {
            let (name, value) = l.rsplit_once(' ')?;
            Some((name, value.trim().parse().ok()?))
        })
}

/// The return to baseline, checked by hand after every run until step 5c: once
/// the load has stopped, nothing is still held. `reservations_in_flight` is not
/// here — under F2 it is not zero, and `soak/V4/registry` is where that is
/// judged, so it is not counted twice.
fn baseline_problems(instance: &str, rest: &Rest) -> Vec<String> {
    let body = rest.final_prom.as_deref().unwrap_or_default();
    let mut problems = Vec::new();
    let mut held: Vec<(&str, f64)> = Vec::new();
    for series in [
        "simmer_sessions_active",
        "simmer_db_pool_connections{state=\"in_use\"}",
    ] {
        match metric(body, series) {
            Some(v) => held.push((series, v)),
            None => problems.push(format!("{instance}: no {series} in the final scrape")),
        }
    }
    // A check that finds no series to read passes, so an absent family is a
    // failure rather than a silence.
    for (family, pick) in [
        ("simmer_quota_reserved{", ""),
        ("simmer_pool_connections{", "state=\"active\""),
    ] {
        let found: Vec<(&str, f64)> = series_matching(body, family)
            .filter(|(name, _)| name.contains(pick))
            .collect();
        if found.is_empty() {
            problems.push(format!(
                "{instance}: no {family}…}} series in the final scrape"
            ));
        }
        held.extend(found);
    }
    for (series, v) in held {
        if v != 0.0 {
            problems.push(format!("{instance}: {series} is {v} at rest"));
        }
    }
    if let Some((before, after)) = rest.threads {
        if after > before {
            problems.push(format!(
                "{instance}: {after} threads at rest, against {before} before the first message"
            ));
        }
    }
    if let Some((before, after)) = rest.tasks {
        if after > before {
            problems.push(format!(
                "{instance}: {after} tasks alive at rest, against {before} before the first \
                 message"
            ));
        }
    }
    if let Some(extra) = &rest.unaccounted_fds {
        if !extra.is_empty() {
            problems.push(format!(
                "{instance}: {} descriptors open at rest that were not open before the first \
                 message and that no pool holds: {}",
                extra.len(),
                extra.join(", ")
            ));
        }
    }
    problems
}

// ---------------------------------------------------------------------------
// tracing slow messages (step 5c)
// ---------------------------------------------------------------------------

/// Every V2/V3 message a client waited more than [`SLOW_MS`] for, and the sink's
/// records of the slowest [`SLOW_TRACED`] per instance — filtered inside the
/// results volume, because the whole sink file is ~70,000 lines an hour. V4's
/// messages are held 15 s at the dot by design and are not among them.
fn copy_slow_evidence() {
    let mut ids = Vec::new();
    for instance in INSTANCES {
        let text = from_results(&format!(
            "awk -F'\"latency_ms\":' 'NF > 1 {{ split($2, v, /[,}}]/); if (v[1] + 0 > {SLOW_MS}) print }}' \
             /results/soak-{instance}.jsonl"
        ));
        let mut slow: Vec<Sent> = reconcile::read_jsonl(&text);
        slow.sort_by(|a, b| b.latency_ms.total_cmp(&a.latency_ms));
        ids.extend(slow.into_iter().take(SLOW_TRACED).map(|s| s.id));
        let _ = fs::write(sample_path(&format!("slow-{instance}.jsonl")), text);
    }
    if ids.is_empty() {
        return;
    }
    let patterns: String = ids
        .iter()
        .map(|id| format!(" -e '\"id\":\"{id}\"'"))
        .collect();
    let text = from_results(&format!("grep -F{patterns} /results/sink.jsonl || true"));
    let _ = fs::write(sample_path("slow-sink.jsonl"), text);
}

/// The slowest messages, each with the correlation id Simmer logged it under —
/// its `downstream accepted the message` line carries the same id, the route
/// and Simmer's own `latency_ms` — so a tail is followed one message at a time.
/// Reported, never judged.
fn report_slow() {
    let sink: Vec<Received> = read_sample_jsonl("slow-sink.jsonl").unwrap_or_default();
    let mut shown = false;
    for instance in INSTANCES {
        let Some(mut slow) = read_sample_jsonl::<Sent>(&format!("slow-{instance}.jsonl")) else {
            continue;
        };
        slow.sort_by(|a, b| b.latency_ms.total_cmp(&a.latency_ms));
        eprintln!(
            "\n{instance}: {} messages took more than {SLOW_MS} ms{}",
            slow.len(),
            if slow.is_empty() {
                ""
            } else {
                "; the slowest:"
            }
        );
        for s in slow.iter().take(10) {
            let correlation = sink
                .iter()
                .find(|r| r.id == s.id)
                .map_or("no sink record", |r| {
                    r.correlation
                        .as_deref()
                        .unwrap_or("no X-Simmer-Correlation")
                });
            eprintln!(
                "  {:>8.1} ms  {}  {} at {}  correlation {correlation}",
                s.latency_ms, s.id, s.code, s.stage
            );
            shown = true;
        }
    }
    if shown {
        eprintln!("  (the server's side of one: docker compose logs app | grep <correlation>)");
    }
}

// ---------------------------------------------------------------------------
// V4 — relays cancelled by the session timeout (F2)
// ---------------------------------------------------------------------------

fn v4_enabled() -> bool {
    std::env::var("SOAK_V4").map_or(true, |v| v != "off")
}

/// One client, back to back, sessions of [`V4_PER_SESSION`] small messages, each
/// held [`V4_SLOW`] at the dot by the sink: a session is almost entirely relays
/// in flight, so the session timeout almost always lands inside one.
fn v4_args(instance: &str, run_for: Duration) -> Vec<String> {
    [
        "--host",
        instance,
        "--port",
        "25",
        "--username",
        "soakapp",
        "--from",
        V4_SENDER,
        "--from-header",
        &format!("Jane Smith <{V4_SENDER}>"),
        "--tag",
        &format!("soak-v4-{instance}"),
        "--duration",
        &format!("{}s", run_for.as_secs()),
        "--concurrency",
        "1",
        "--per-session",
        &V4_PER_SESSION.to_string(),
        "--size",
        "4k",
        "--sink-script",
        &format!("slow@dot:{}s", V4_SLOW.as_secs()),
        "--jsonl",
        &format!("/results/soak-v4-{instance}.jsonl"),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// V4's quota row and reservations, from the database both instances share.
#[derive(Debug, Clone, Copy)]
struct Ledger {
    rows: u64,
    reserved: u64,
    committed: u64,
}

fn sample_v4_ledger() -> Option<Ledger> {
    let sql = format!(
        "select (select count(*) from quota_reservation where route = '{V4_ROUTE}'), \
         (select coalesce(sum(reserved), 0) from quota_usage where route = '{V4_ROUTE}'), \
         (select coalesce(sum(committed), 0) from quota_usage where route = '{V4_ROUTE}')"
    );
    // Not `Stack::psql`, which panics: a sampler that died on one slow query
    // would leave the rest of the run unsampled.
    let out = SOAK
        .compose()
        .args([
            "exec",
            "-T",
            "simmer-db",
            "psql",
            "-U",
            "simmer",
            "-d",
            "simmer",
            "-q",
            "-At",
            "-c",
            &sql,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut f = text.trim().split('|').map(|v| v.parse::<u64>().ok());
    Some(Ledger {
        rows: f.next()??,
        reserved: f.next()??,
        committed: f.next()??,
    })
}

fn append_v4_ledger(t: f64, l: &Ledger) {
    let path = sample_path("v4.csv");
    let fresh = !path.exists();
    let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    if fresh {
        let _ = writeln!(f, "t,rows,reserved,committed");
    }
    let _ = writeln!(f, "{t:.1},{},{},{}", l.rows, l.reserved, l.committed);
}

fn read_v4_ledger() -> Vec<(f64, Ledger)> {
    let Ok(text) = fs::read_to_string(sample_path("v4.csv")) else {
        return Vec::new();
    };
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let mut f = line.split(',').map(str::trim);
            Some((
                f.next()?.parse().ok()?,
                Ledger {
                    rows: f.next()?.parse().ok()?,
                    reserved: f.next()?.parse().ok()?,
                    committed: f.next()?.parse().ok()?,
                },
            ))
        })
        .collect()
}

/// Wait for the sweeper to clear what the last cancellations stranded: a
/// reservation expires 205 s after it was taken and the sweeper runs every
/// minute, so ten minutes is generous. Samples as it goes, so the drain is on
/// the ledger's curve too.
fn drain_v4(started: Instant) -> bool {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        if let Some(l) = sample_v4_ledger() {
            append_v4_ledger(started.elapsed().as_secs_f64(), &l);
            if l.rows == 0 && l.reserved == 0 {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_secs(10));
    }
}

/// `/metrics` from inside the instance's own network namespace, so `app2` —
/// whose admin port is not published — can be read too.
fn scrape_in_container(instance: &str) -> Option<String> {
    let out = SOAK
        .compose()
        .args([
            "exec",
            "-T",
            instance,
            "bash",
            "-c",
            "exec 3<>/dev/tcp/127.0.0.1/8080; \
             printf 'GET /metrics HTTP/1.0\\r\\n\\r\\n' >&3; cat <&3",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
}

/// V4's records, copied out of the results volume so `soak_analyze` needs nothing
/// but the host: both loadgens' JSONL, and the sink's lines for V4's ids alone —
/// the whole file is ~70,000 lines an hour of V2's traffic.
fn copy_v4_evidence() {
    for instance in INSTANCES {
        let text = from_results(&format!("cat /results/soak-v4-{instance}.jsonl"));
        let _ = fs::write(sample_path(&format!("v4-{instance}.jsonl")), text);
    }
    let text = from_results(r#"grep '"id":"soak-v4-' /results/sink.jsonl || true"#);
    let _ = fs::write(sample_path("v4-sink.jsonl"), text);
}

fn from_results(script: &str) -> String {
    SOAK.compose()
        .args([
            "run",
            "--rm",
            "--no-deps",
            "--no-TTY",
            "--entrypoint",
            "sh",
            "loadgen",
            "-c",
            script,
        ])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// driving the stack
// ---------------------------------------------------------------------------

fn recreate_instances() {
    let out = SOAK
        .compose()
        .args(["up", "-d", "--no-deps", "--force-recreate", "--wait"])
        .args(INSTANCES)
        .output()
        .expect("docker compose up");
    assert!(
        out.status.success(),
        "the soak instances would not start: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fresh_sink() {
    let _ = SOAK.run(&[
        "run",
        "--rm",
        "--no-deps",
        "--no-TTY",
        "--entrypoint",
        "sh",
        "loadgen",
        "-c",
        "rm -f /results/soak-*.jsonl",
    ]);
    let out = SOAK
        .compose()
        .env("SINK_ARGS", "--idle-close-secs 45")
        .args([
            "up",
            "-d",
            "--no-deps",
            "--force-recreate",
            "--wait",
            "sink",
        ])
        .output()
        .expect("docker compose up sink");
    assert!(out.status.success(), "the sink would not start");
}

fn run_loadgen(instance: &str, args: &[String]) -> String {
    let out = SOAK
        .compose()
        .args(["run", "--rm", "--no-deps", "--no-TTY", "loadgen"])
        .args(args)
        .output()
        .expect("docker compose run loadgen");
    if !out.status.success() {
        return format!(
            "{instance}: loadgen failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find(|l| l.starts_with("{\"summary\""))
        .map(|l| format!("{instance}: {l}"))
        .unwrap_or_else(|| format!("{instance}: no summary"))
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn duration_from_env(key: &str, default: &str) -> Duration {
    duration_str(&std::env::var(key).unwrap_or_else(|_| default.to_string()))
}

/// `90s`, `10m`, `2h`, or bare seconds.
fn duration_str(s: &str) -> Duration {
    let s = s.trim();
    let (n, unit) = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map(|i| s.split_at(i))
        .unwrap_or((s, "s"));
    let n: f64 = n.parse().unwrap_or_else(|_| panic!("duration {s}"));
    Duration::from_secs_f64(match unit {
        "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        other => panic!("duration unit {other}"),
    })
}
