//! §9.6 (D-126) against a real collector: the acceptance stack's dummy
//! `otel-collector`, which writes what it receives to the `otel-out` volume.
//!
//! `tests/telemetry.rs` proves what the pipeline *would* export, in memory.
//! This proves it leaves the container: over real gRPC, through the batch
//! processors, to a real OTLP receiver, as the protobuf an actual backend
//! would get. Like `acceptance.rs`, it needs the acceptance profile:
//!
//! ```sh
//! docker compose -f docker-compose.yml -f test/compose/acceptance.yml \
//!   --profile acceptance up -d --build
//! cargo test --test telemetry_compose -- --ignored --test-threads=1
//! ```

mod compose;

use std::time::{Duration, Instant};

use compose::stack::ACCEPTANCE;
use serde_json::Value;

/// How many of a file's last lines are read. One line is one export request,
/// and this test's exports are seconds old when it reads them; the files
/// themselves outlive every run (`otel-init` cannot safely empty them under a
/// running collector, docker-compose.yml says why) and can be hundreds of MiB
/// after a soak.
const TAIL_LINES: &str = "300";

/// The last [`TAIL_LINES`] OTLP JSON export requests in a file, as the
/// collector's file exporter writes them. Read through the loadgen, which mounts
/// the volume read-only: the collector image is distroless and has no `tail`.
fn collector_file(name: &str) -> Vec<Value> {
    // Not `ACCEPTANCE.run`, which fails the test on a non-zero exit: on a fresh
    // `otel-out` the file does not exist until the first export, and that is a
    // reason to poll again rather than to fail.
    let out = ACCEPTANCE
        .compose()
        .args([
            "run",
            "--rm",
            "--no-deps",
            "--no-TTY",
            "--entrypoint",
            "tail",
            "loadgen",
            "-n",
            TAIL_LINES,
            &format!("/otel/{name}"),
        ])
        .output()
        .expect("docker compose run");
    if !out.status.success() {
        // Not written yet: nothing has been exported to that pipeline.
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("a line of OTLP JSON"))
        .collect()
}

fn raw_file(name: &str) -> String {
    collector_file(name)
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Poll until `pred` holds over the file's records, or fail after `within`.
fn wait_for(name: &str, within: Duration, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + within;
    loop {
        let records = collector_file(name);
        if pred(&records) {
            return records;
        }
        assert!(
            Instant::now() < deadline,
            "{name} never satisfied the condition; collector logs:\n{}",
            String::from_utf8_lossy(&ACCEPTANCE.run(&["logs", "otel-collector"]).stdout)
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Now, as OTLP writes a time: nanoseconds since the epoch.
fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_nanos()
}

/// An OTLP JSON time, which the encoding writes as a string.
fn nanos(v: &Value) -> u128 {
    v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// Every span in the records that started at or after `since`, flattened,
/// with its resource attributes alongside. Older spans are an earlier run's.
fn spans(records: &[Value], since: u128) -> Vec<(Value, Value)> {
    let mut out = Vec::new();
    for r in records {
        for rs in r["resourceSpans"].as_array().into_iter().flatten() {
            for ss in rs["scopeSpans"].as_array().into_iter().flatten() {
                for span in ss["spans"].as_array().into_iter().flatten() {
                    if nanos(&span["startTimeUnixNano"]) >= since {
                        out.push((span.clone(), rs["resource"].clone()));
                    }
                }
            }
        }
    }
    out
}

/// Does this metrics export request carry a data point taken at or after
/// `since`? Each request is one collection, so all its points share a time.
fn collected_since(v: &Value, since: u128) -> bool {
    match v {
        Value::Object(m) => m
            .iter()
            .any(|(k, v)| (k == "timeUnixNano" && nanos(v) >= since) || collected_since(v, since)),
        Value::Array(a) => a.iter().any(|v| collected_since(v, since)),
        _ => false,
    }
}

fn attr(obj: &Value, key: &str) -> Option<String> {
    obj["attributes"].as_array()?.iter().find_map(|kv| {
        (kv["key"] == key).then(|| {
            let v = &kv["value"];
            v["stringValue"]
                .as_str()
                .map(str::to_string)
                .or_else(|| v["intValue"].as_str().map(str::to_string))
                .or_else(|| v["boolValue"].as_bool().map(|b| b.to_string()))
                .unwrap_or_else(|| v.to_string())
        })
    })
}

/// Every span one relayed message produces.
const TREE: [&str; 7] = [
    "smtp.session",
    "smtp.transaction",
    "simmer.relay",
    "simmer.route",
    "simmer.rewrite",
    "smtp.downstream",
    "simmer.quota.resolve",
];

#[test]
#[ignore = "needs the acceptance compose profile"]
fn a_relayed_message_reaches_the_collector_as_traces_metrics_and_logs() {
    let _logs = ACCEPTANCE.logs_on_failure();
    // Re-creates `app` on `simmer.acceptance.yaml` — `up` alone starts it on the
    // shipped config, which exports nothing — exactly as `acceptance.rs` does.
    ACCEPTANCE.restart_app_at_day(0);
    let since = now_nanos();
    let tag = format!("otel{}", std::process::id());
    let replies = compose::loadgen::run(&ACCEPTANCE, &["--count", "2", "--tag", &tag]);
    assert!(replies.iter().all(|r| r.code == 250), "{replies:?}");

    // -- traces: the whole tree, in one trace, from this run ---------------
    // Waits for the whole tree, not just the relay span: `smtp.session` ends
    // when the connection closes, after everything under it, and can arrive in
    // a later batch than the rest of its trace.
    let records = wait_for("traces.jsonl", Duration::from_secs(30), |r| {
        let all = spans(r, since);
        all.iter().any(|(relay, _)| {
            relay["name"] == "simmer.relay"
                && attr(relay, "result").as_deref() == Some("delivered")
                && TREE.iter().all(|name| {
                    all.iter()
                        .any(|(s, _)| s["traceId"] == relay["traceId"] && s["name"] == *name)
                })
        })
    });
    let all = spans(&records, since);

    let (relay, resource) = all
        .iter()
        .rev()
        .find(|(s, _)| {
            s["name"] == "simmer.relay" && attr(s, "result").as_deref() == Some("delivered")
        })
        .expect("a delivered simmer.relay span");
    assert_eq!(
        attr(resource, "service.name").as_deref(),
        Some("simmer-acceptance")
    );
    assert_eq!(
        attr(resource, "deployment.environment").as_deref(),
        Some("acceptance")
    );
    assert!(attr(resource, "service.instance.id").is_some());
    assert_eq!(
        attr(resource, "simmer.backend").as_deref(),
        Some("postgres")
    );

    let trace_id = relay["traceId"].as_str().expect("traceId").to_string();
    let in_trace: Vec<&Value> = all
        .iter()
        .map(|(s, _)| s)
        .filter(|s| s["traceId"] == trace_id.as_str())
        .collect();
    let by_name = |name: &str| {
        *in_trace
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("no {name} in trace {trace_id}"))
    };
    let session = by_name("smtp.session");
    let tx = by_name("smtp.transaction");
    let down = by_name("smtp.downstream");
    for name in ["simmer.route", "simmer.rewrite", "simmer.quota.resolve"] {
        assert_eq!(
            by_name(name)["parentSpanId"],
            relay["spanId"],
            "{name} under simmer.relay"
        );
    }
    assert_eq!(tx["parentSpanId"], session["spanId"]);
    assert_eq!(relay["parentSpanId"], tx["spanId"]);
    assert_eq!(down["parentSpanId"], relay["spanId"]);
    assert_eq!(attr(tx, "smtp.reply.code").as_deref(), Some("250"));
    assert_eq!(attr(down, "smtp.response.code").as_deref(), Some("250"));
    let route = attr(relay, "route").expect("route");
    assert!(
        ["warming-newbrand", "overflow-established"].contains(&route.as_str()),
        "{route}"
    );
    assert_eq!(attr(relay, "correlation_id"), attr(tx, "correlation_id"));

    // -- logs: correlated with that trace ---------------------------------
    let logs = wait_for("logs.jsonl", Duration::from_secs(30), |r| {
        r.iter().any(|req| req.to_string().contains(&trace_id))
    });
    let accepted = logs.iter().any(|req| {
        req["resourceLogs"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|rl| {
                rl["scopeLogs"].as_array().into_iter().flatten().any(|sl| {
                    sl["logRecords"].as_array().into_iter().flatten().any(|lr| {
                        lr["traceId"] == trace_id.as_str()
                            && lr["body"]["stringValue"] == "downstream accepted the message"
                    })
                })
            })
    });
    assert!(
        accepted,
        "no 'downstream accepted the message' record in trace {trace_id}"
    );

    // -- metrics: §9.1, unchanged names -----------------------------------
    // Periodic every 5s in this stack; allow two intervals and the batch.
    let metrics = wait_for("metrics.jsonl", Duration::from_secs(30), |r| {
        r.iter().any(|req| {
            let s = req.to_string();
            collected_since(req, since)
                && s.contains("\"simmer_messages_total\"")
                && s.contains("\"delivered\"")
        })
    });
    let text: String = metrics
        .iter()
        .filter(|req| collected_since(req, since))
        .map(Value::to_string)
        .collect();
    // A scrape-time gauge, recomputed by the export's own task (D-126): this
    // stack has admin.metrics on too, but nothing scrapes it during the test.
    assert!(
        text.contains("\"simmer_quota_allowance\""),
        "the quota gauges were not exported"
    );

    // -- §9.5: no recipient left the container ----------------------------
    let everything = format!(
        "{}\n{}\n{}",
        raw_file("traces.jsonl"),
        raw_file("logs.jsonl"),
        raw_file("metrics.jsonl")
    );
    for r in &replies {
        let local = r.recipient.split('@').next().expect("local part");
        assert!(
            !everything.contains(local),
            "the recipient {} reached the collector",
            r.recipient
        );
    }
}
