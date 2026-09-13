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

mod compose;

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use compose::leak;
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
                "--jsonl".to_string(),
                format!("/results/soak-{instance}.jsonl"),
            ];
            let name = instance.to_string();
            thread::spawn(move || run_loadgen(&name, &args))
        })
        .collect();

    for load in loads {
        let summary = load.join().expect("a load thread");
        eprintln!("  {summary}");
    }

    stop.store(true, Ordering::Relaxed);
    for sampler in samplers {
        let _ = sampler.join();
    }
    eprintln!(
        "soak: finished after {:.1} minutes; samples in {SAMPLE_DIR}/",
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
    for instance in INSTANCES {
        let samples = read_samples(instance);
        if samples.is_empty() {
            eprintln!("{instance}: no samples; run soak_run first");
            continue;
        }
        judged += 1;

        let span = samples.last().expect("samples").t - samples[0].t;
        // W = clamp(0.15 x D, 10 min, 90 min), overridable so a short run can
        // still drive the whole analysis rather than being all warm-up.
        let warmup = match std::env::var("SOAK_WARMUP") {
            Ok(v) => duration_str(&v).as_secs_f64(),
            Err(_) => (0.15 * span).clamp(600.0, 5400.0),
        };

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
            let v = leak::verdict(
                &series,
                warmup,
                FLOOR_WINDOW,
                leak::Limits {
                    slope_per_hour: limit,
                },
            );
            let scale = if unit == "MiB/h" { 1_048_576.0 } else { 1.0 };
            eprintln!(
                "  {name:<8} slope {:+.2} {unit}, quartile step {:+.2}{}{}",
                v.slope_per_hour / scale,
                v.quartile_step / scale,
                if unit == "MiB/h" { " MiB" } else { "" },
                if v.inconclusive {
                    "  (inconclusive: fewer than eight post-warm-up floors)"
                } else if v.leaking {
                    "  LEAKING"
                } else {
                    ""
                }
            );
            assert!(
                !v.leaking,
                "{instance}: {name} is growing at {:+.2} {unit} with a quartile step of {:+.2}",
                v.slope_per_hour / scale,
                v.quartile_step / scale
            );
        }

        // Hard bounds, at every sample rather than on the trend: these are not
        // allowed to drift even briefly.
        let peak_established = samples.iter().map(|s| s.established).max().unwrap_or(0);
        let peak_close_wait = samples.iter().map(|s| s.close_wait).max().unwrap_or(0);
        eprintln!("  peak established {peak_established}, peak CLOSE_WAIT {peak_close_wait}");
        assert!(
            peak_close_wait <= 64,
            "{instance}: CLOSE_WAIT peaked at {peak_close_wait}, which is more sockets \
             half-closed than every downstream pool put together"
        );
    }
    assert!(judged > 0, "no samples at all; run soak_run first");
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

fn csv_path(instance: &str) -> PathBuf {
    PathBuf::from(SAMPLE_DIR).join(format!("{instance}.csv"))
}

fn append_sample(instance: &str, t: f64, s: &Sample) {
    let path = csv_path(instance);
    let fresh = !path.exists();
    let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
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
    let Ok(text) = fs::read_to_string(csv_path(instance)) else {
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
