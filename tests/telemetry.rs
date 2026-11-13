//! §9.6 (D-111) — what the OTLP export would carry, captured in memory.
//!
//! Its own binary because it installs the process's global subscriber and
//! metrics recorder — exactly the layers `main` would, via
//! `logging::layers` and `metrics::install_with`, over in-memory exporters
//! instead of a collector. The sessions run on spawned tasks, so a thread-local
//! default subscriber would not see them.
//!
//! Tests share the one pipeline and so run against their own fake downstream
//! and pick their own spans out by `correlation_id`.

mod support;

use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::trace::{SpanKind, Status};
use opentelemetry::Value;
use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use support::{config_for, Act, FakeDownstream, Script, Simmer};
use tracing_subscriber::layer::SubscriberExt;

const RECIPIENT: &str = "telemetry-victim@gmail.com";

/// A route that stamps the correlation id on the outbound message, so a test
/// can match what was exported against what was sent.
const CORRELATION_HEADER: &str = r#"
      set_headers:
        X-Simmer-Correlation-Id: "{{correlation_id}}"
"#;

struct Exported {
    spans: InMemorySpanExporter,
    logs: InMemoryLogExporter,
    metrics: InMemoryMetricExporter,
    logger: SdkLoggerProvider,
    meter: SdkMeterProvider,
}

fn exported() -> &'static Exported {
    static ONCE: OnceLock<Exported> = OnceLock::new();
    ONCE.get_or_init(|| {
        let spans = InMemorySpanExporter::default();
        let logs = InMemoryLogExporter::default();
        let metrics = InMemoryMetricExporter::default();
        let tracer = SdkTracerProvider::builder()
            .with_simple_exporter(spans.clone())
            .build();
        let logger = SdkLoggerProvider::builder()
            .with_simple_exporter(logs.clone())
            .build();
        let meter = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(metrics.clone()).build())
            .build();

        let pipeline = simmer::telemetry::Pipeline::from_providers(
            Some(tracer),
            Some(meter.clone()),
            Some(logger.clone()),
            "info",
            Duration::from_secs(60),
        );
        // stdout as quiet as it goes: this binary is about the other layers.
        let layers =
            simmer::logging::layers("off", simmer::config::LogFormat::Text, Some(&pipeline));
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layers))
            .expect("the only subscriber in this binary");
        simmer::metrics::install_with(None, pipeline.recorder())
            .expect("the only recorder in this binary");

        Exported {
            spans,
            logs,
            metrics,
            logger,
            meter,
        }
    })
}

fn attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| &kv.value)
}

fn attr_str(span: &SpanData, key: &str) -> Option<String> {
    attr(span, key).map(|v| v.as_str().into_owned())
}

/// Every finished span of the trace whose `smtp.transaction` carries `id`.
fn trace_for(correlation_id: &str) -> Vec<SpanData> {
    let all = exported().spans.get_finished_spans().expect("spans");
    let tx = all
        .iter()
        .find(|s| {
            s.name == "smtp.transaction"
                && attr_str(s, "correlation_id").as_deref() == Some(correlation_id)
        })
        .unwrap_or_else(|| panic!("no smtp.transaction span for {correlation_id}"));
    let trace = tx.span_context.trace_id();
    all.into_iter()
        .filter(|s| s.span_context.trace_id() == trace)
        .collect()
}

fn named<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    spans
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no {name} span in the trace"))
}

fn child_of(child: &SpanData, parent: &SpanData) -> bool {
    child.parent_span_id == parent.span_context.span_id()
}

async fn deliver(script: Script) -> (FakeDownstream, String, u16) {
    exported();
    let downstream = FakeDownstream::start(script).await;
    let simmer = Simmer::start(&config_for(downstream.addr, CORRELATION_HEADER)).await;
    let mut client = simmer.connect().await;
    client.hello().await;
    let reply = client
        .deliver(
            "jane@oldbrand.com",
            RECIPIENT,
            "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n",
        )
        .await;
    client.command("QUIT").await;
    drop(client);
    // The session span ends when the connection's task finishes.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let correlation_id = exported()
        .spans
        .get_finished_spans()
        .expect("spans")
        .iter()
        .filter(|s| s.name == "smtp.downstream")
        .filter(|s| attr_str(s, "server.port") == Some(downstream.addr.port().to_string()))
        .find_map(|d| {
            exported()
                .spans
                .get_finished_spans()
                .expect("spans")
                .into_iter()
                .find(|r| r.name == "simmer.relay" && child_of(d, r))
                .and_then(|r| attr_str(&r, "correlation_id"))
        })
        .expect("a simmer.relay span over this test's downstream");
    (downstream, correlation_id, reply.code)
}

#[tokio::test]
async fn a_delivered_message_is_one_trace_from_session_to_quota() {
    let (downstream, id, code) = deliver(Script::default()).await;
    assert_eq!(code, 250);

    // The id exported is the id that left on the message.
    let sent = String::from_utf8_lossy(&downstream.last().expect("delivered").body).into_owned();
    assert!(
        sent.contains(&format!("X-Simmer-Correlation-Id: {id}")),
        "the exported correlation_id {id} is not the one on the outbound message:\n{sent}"
    );

    let trace = trace_for(&id);
    let session = named(&trace, "smtp.session");
    let tx = named(&trace, "smtp.transaction");
    let relay = named(&trace, "simmer.relay");
    let walk = named(&trace, "simmer.route");
    let rewrite = named(&trace, "simmer.rewrite");
    let down = named(&trace, "smtp.downstream");
    let resolve = named(&trace, "simmer.quota.resolve");
    // The post-commit read for the §9.1 gauges (D-111, docs/SOAK.md §18).
    let usage = named(&trace, "simmer.quota.usage");

    assert!(child_of(tx, session), "transaction under the session");
    assert!(child_of(relay, tx), "relay under the transaction");
    for (name, span) in [
        ("simmer.route", walk),
        ("simmer.rewrite", rewrite),
        ("smtp.downstream", down),
        ("simmer.quota.resolve", resolve),
        ("simmer.quota.usage", usage),
    ] {
        assert!(child_of(span, relay), "{name} under simmer.relay");
    }

    // stdout shows only the innermost span's fields, so every child of the
    // relay carries the id itself — that is what puts it on the outcome lines.
    for span in [walk, rewrite, down, resolve, usage] {
        assert_eq!(
            attr_str(span, "correlation_id").as_deref(),
            Some(id.as_str()),
            "{}",
            span.name
        );
    }

    assert_eq!(session.span_kind, SpanKind::Server);
    assert_eq!(down.span_kind, SpanKind::Client);

    assert_eq!(attr_str(tx, "smtp.reply.code").as_deref(), Some("250"));
    assert_eq!(attr_str(relay, "ramp").as_deref(), Some("main"));
    assert_eq!(attr_str(relay, "route").as_deref(), Some("only"));
    assert_eq!(attr_str(relay, "domain_group").as_deref(), Some("catchall"));
    assert_eq!(attr_str(relay, "result").as_deref(), Some("delivered"));
    assert_eq!(attr_str(walk, "route").as_deref(), Some("only"));
    assert!(attr_str(walk, "chain").is_some(), "the rendered chain");
    assert_eq!(attr_str(down, "smtp.response.code").as_deref(), Some("250"));
    assert_eq!(attr_str(down, "outcome").as_deref(), Some("delivered"));
    assert_eq!(attr_str(resolve, "committed").as_deref(), Some("true"));
    assert!(!matches!(tx.status, Status::Error { .. }));

    // The `relaying` and `downstream accepted` lines are log records in the
    // same trace — the correlation the stdout lines never had for outcomes.
    exported().logger.force_flush().expect("flush logs");
    let logs = exported().logs.get_emitted_logs().expect("logs");
    let accepted = logs
        .iter()
        .find(|l| {
            l.record
                .trace_context()
                .is_some_and(|t| t.trace_id == tx.span_context.trace_id())
                && format!("{:?}", l.record.body()).contains("downstream accepted the message")
        })
        .expect("a 'downstream accepted the message' log record in the transaction's trace");
    assert_eq!(accepted.record.severity_text(), Some("INFO"));

    // §9.1 over OTLP, through the same facade as /metrics.
    exported().meter.force_flush().expect("flush metrics");
    let delivered = exported()
        .metrics
        .get_finished_metrics()
        .expect("metrics")
        .iter()
        .flat_map(|rm| rm.scope_metrics())
        .flat_map(|sm| sm.metrics())
        .filter(|m| m.name() == "simmer_messages_total")
        .any(|m| match m.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => sum.data_points().any(|p| {
                p.attributes()
                    .any(|kv| kv.key.as_str() == "result" && kv.value.as_str() == "delivered")
            }),
            _ => false,
        });
    assert!(
        delivered,
        "simmer_messages_total{{result=\"delivered\"}} was exported"
    );
}

#[tokio::test]
async fn a_downstream_deferral_marks_the_trace_as_failed() {
    let (_downstream, id, code) = deliver(Script::with(|s| {
        s.final_dot = Act::Reply(452, "4.2.2 mailbox full");
    }))
    .await;
    assert_eq!(code, 451);

    let trace = trace_for(&id);
    let tx = named(&trace, "smtp.transaction");
    let relay = named(&trace, "simmer.relay");
    let down = named(&trace, "smtp.downstream");
    let resolve = named(&trace, "simmer.quota.resolve");

    for (name, span) in [
        ("smtp.transaction", tx),
        ("simmer.relay", relay),
        ("smtp.downstream", down),
    ] {
        assert!(
            matches!(span.status, Status::Error { .. }),
            "{name} should be an error, is {:?}",
            span.status
        );
    }
    assert_eq!(attr_str(tx, "smtp.reply.code").as_deref(), Some("451"));
    assert_eq!(attr_str(down, "outcome").as_deref(), Some("deferred"));
    assert_eq!(attr_str(down, "smtp.response.code").as_deref(), Some("452"));
    assert_eq!(attr_str(down, "smtp.stage").as_deref(), Some("final_dot"));
    assert_eq!(attr_str(relay, "result").as_deref(), Some("deferred"));
    assert_eq!(attr_str(resolve, "committed").as_deref(), Some("false"));
}

/// §9.5 applied to the export: no recipient address leaves in any span
/// attribute, span event or log record — not even from a downstream that quotes
/// it back in its reply text.
#[tokio::test]
async fn no_recipient_address_is_exported() {
    let (_d, id, _) = deliver(Script::with(|s| {
        s.rcpt_to = Act::Reply(550, "5.1.1 <telemetry-victim@gmail.com> unknown user");
    }))
    .await;
    let (_d, _, _) = deliver(Script::default()).await;

    exported().logger.force_flush().expect("flush logs");
    let spans = exported().spans.get_finished_spans().expect("spans");
    let logs = exported().logs.get_emitted_logs().expect("logs");
    assert!(!spans.is_empty() && !logs.is_empty());

    let local = RECIPIENT.split('@').next().expect("local part");
    for s in &spans {
        let text = format!("{:?} {:?}", s.attributes, s.events);
        assert!(
            !text.contains(local),
            "span {} carries the recipient: {text}",
            s.name
        );
    }
    for l in &logs {
        // The downstream's reply text above quotes the address back; outcome.rs
        // redacts its local part before logging it (D-111).
        let text = format!(
            "{:?} {:?}",
            l.record.body(),
            l.record.attributes_iter().collect::<Vec<_>>()
        );
        assert!(
            !text.contains(local),
            "log record carries the recipient: {text}"
        );
    }

    // And the rejected one still produced its trace.
    let trace = trace_for(&id);
    let down = named(&trace, "smtp.downstream");
    assert_eq!(attr_str(down, "smtp.response.code").as_deref(), Some("550"));
    assert_eq!(attr_str(down, "smtp.stage").as_deref(), Some("rcpt_to"));
}

// -- D-115: the schedule step, as a trace sees it ---------------------------

/// `config_for`'s one route made a warming route, alone in a ramp that may
/// have no overflow (D-113), so the walk selects it and the relay span says
/// where it stood on its schedule.
fn warming_config(addr: std::net::SocketAddr) -> String {
    config_for(
        addr,
        "    warmup:\n      started: \"2020-01-01T00:00:00Z\"\n      schedule: { default: [50, 100] }\n",
    )
    .replace("    overflow: true\n", "")
    .replace(
        "  default_chain: [only]\n",
        "  default_chain: [only]\n  allow_no_overflow_route: true\n",
    )
}

#[tokio::test]
async fn the_relay_span_says_where_the_route_stood_on_its_schedule() {
    exported();
    let downstream = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&warming_config(downstream.addr)).await;
    let mut client = simmer.connect().await;
    client.hello().await;
    let reply = client
        .deliver(
            "jane@oldbrand.com",
            RECIPIENT,
            "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n",
        )
        .await;
    assert_eq!(reply.code, 250);
    client.command("QUIT").await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let relay = exported()
        .spans
        .get_finished_spans()
        .expect("spans")
        .into_iter()
        .filter(|s| s.name == "simmer.relay")
        .find(|r| attr_str(r, "warmup.advance_mode").is_some())
        .expect("a relay span for the warming route");
    // The test store grants everything and keeps no history, so a threshold
    // route is on its first step — 2000-odd days behind its calendar.
    assert_eq!(
        attr_str(&relay, "warmup.advance_mode").as_deref(),
        Some("threshold")
    );
    assert_eq!(attr_str(&relay, "warmup.step").as_deref(), Some("0"));
    let held: i64 = attr_str(&relay, "warmup.held_days")
        .unwrap()
        .parse()
        .unwrap();
    let day: i64 = attr_str(&relay, "day_index").unwrap().parse().unwrap();
    assert_eq!(held, day);
}

#[tokio::test]
async fn the_daily_decision_is_an_exported_span_event_and_a_counter() {
    use simmer::quota::advance::Row;
    use simmer::quota::store::RouteState;
    exported();
    let cfg = simmer::config::from_str(&warming_config("127.0.0.1:1".parse().unwrap()), "test")
        .expect("valid");
    let route = cfg.default_ramp().route("only").expect("route");
    // Yesterday sent 40 of 50 at step 0: held, with a bar of 45.
    let position = simmer::quota::position_from(
        route,
        "catchall",
        1,
        RouteState::default(),
        simmer::quota::AdvanceRows {
            today: None,
            previous: Some(Row {
                day_index: 0,
                step: Some(0),
                allowance: Some(50),
                committed: 40,
            }),
        },
    );

    let span = tracing::info_span!("test.d115", otel.name = "test.d115");
    span.in_scope(|| simmer::quota::report_row_created(route, "catchall", &position));
    drop(span);

    let span = exported()
        .spans
        .get_finished_spans()
        .expect("spans")
        .into_iter()
        .find(|s| s.name == "test.d115")
        .expect("the span");
    let event = span
        .events
        .iter()
        .find(|e| {
            e.attributes.iter().any(|kv| {
                kv.key.as_str() == "event" && kv.value.as_str() == "simmer.warmup.advance"
            })
        })
        .expect("a simmer.warmup.advance event");
    let get = |k: &str| {
        event
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == k)
            .map(|kv| kv.value.as_str().into_owned())
    };
    assert_eq!(get("outcome").as_deref(), Some("held"));
    assert_eq!(get("step").as_deref(), Some("0"));
    assert_eq!(get("required").as_deref(), Some("45"));
    assert_eq!(get("previous_committed").as_deref(), Some("40"));
    assert_eq!(get("previous_allowance").as_deref(), Some("50"));
    assert_eq!(get("advance_mode").as_deref(), Some("threshold"));
    let text = format!("{:?}", event.attributes);
    assert!(
        !text.contains('@'),
        "no recipient in a schedule event: {text}"
    );

    // ...and the counter reaches OTLP through the same facade as /metrics.
    exported().meter.force_flush().expect("flush metrics");
    let counted = exported()
        .metrics
        .get_finished_metrics()
        .expect("metrics")
        .iter()
        .flat_map(|rm| rm.scope_metrics())
        .flat_map(|sm| sm.metrics())
        .filter(|m| m.name() == "simmer_warmup_advance_total")
        .any(|m| match m.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => sum.data_points().any(|p| {
                p.attributes()
                    .any(|kv| kv.key.as_str() == "outcome" && kv.value.as_str() == "held")
            }),
            _ => false,
        });
    assert!(
        counted,
        "simmer_warmup_advance_total{{outcome=\"held\"}} was exported"
    );
}

/// [`warming_config`] with its ramp in a ramp group (D-116).
fn grouped_config(addr: std::net::SocketAddr) -> String {
    format!(
        "{}\nramp_groups:\n  telemetry-group:\n    ramps: [main]\n    started: \"2020-01-01T00:00:00Z\"\n    schedule: [1000]\n",
        warming_config(addr)
    )
}

#[tokio::test]
async fn the_relay_span_names_the_ramp_group_it_counted_against() {
    exported();
    let downstream = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&grouped_config(downstream.addr)).await;
    let mut client = simmer.connect().await;
    client.hello().await;
    let reply = client
        .deliver(
            "jane@oldbrand.com",
            RECIPIENT,
            "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n",
        )
        .await;
    assert_eq!(reply.code, 250);
    client.command("QUIT").await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let spans = exported().spans.get_finished_spans().expect("spans");
    let relay = spans
        .iter()
        .filter(|s| s.name == "simmer.relay")
        .find(|r| attr_str(r, "ramp_group").is_some())
        .expect("a relay span naming the group");
    assert_eq!(
        attr_str(relay, "ramp_group").as_deref(),
        Some("telemetry-group")
    );
    assert_eq!(attr_str(relay, "ramp_group.step").as_deref(), Some("0"));
    let walk = spans
        .iter()
        .filter(|s| s.name == "simmer.route")
        .find(|r| attr_str(r, "ramp_group").is_some())
        .expect("the walk span too");
    assert_eq!(
        attr_str(walk, "ramp_group").as_deref(),
        Some("telemetry-group")
    );
}

#[tokio::test]
async fn the_group_daily_decision_is_an_exported_span_event() {
    use simmer::quota::advance::Row;
    exported();
    let cfg = simmer::config::from_str(&grouped_config("127.0.0.1:1".parse().unwrap()), "test")
        .expect("valid");
    let group = cfg.ramp_groups.get("telemetry-group").expect("group");
    // Yesterday committed 950 of 1000: met at 0.9, so advanced.
    let position = simmer::quota::group_position_from(
        group,
        1,
        simmer::quota::AdvanceRows {
            today: None,
            previous: Some(Row {
                day_index: 0,
                step: Some(0),
                allowance: Some(1000),
                committed: 950,
            }),
        },
    );
    let span = tracing::info_span!("test.d116", otel.name = "test.d116");
    span.in_scope(|| simmer::quota::report_group_row_created(group, &position));
    drop(span);

    let span = exported()
        .spans
        .get_finished_spans()
        .expect("spans")
        .into_iter()
        .find(|s| s.name == "test.d116")
        .expect("the span");
    let event = span
        .events
        .iter()
        .find(|e| {
            e.attributes.iter().any(|kv| {
                kv.key.as_str() == "event" && kv.value.as_str() == "simmer.ramp_group.advance"
            })
        })
        .expect("a simmer.ramp_group.advance event");
    let get = |k: &str| {
        event
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == k)
            .map(|kv| kv.value.as_str().into_owned())
    };
    assert_eq!(get("ramp_group").as_deref(), Some("telemetry-group"));
    assert_eq!(get("outcome").as_deref(), Some("advanced"));
    assert_eq!(get("step").as_deref(), Some("1"));
    assert_eq!(get("required").as_deref(), Some("900"));
    assert_eq!(get("previous_committed").as_deref(), Some("950"));
}
