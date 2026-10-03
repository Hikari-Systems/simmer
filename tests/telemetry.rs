//! §9.6 (D-126) — what the OTLP export would carry, captured in memory.
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
    // The post-commit read for the §9.1 gauges (D-126, docs/SOAK.md §18).
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
        // redacts its local part before logging it (D-126).
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

// ---------------------------------------------------------------------------
// D-127 — the spool's spans
// ---------------------------------------------------------------------------

/// A spooled message is two traces joined by `correlation_id`: the client's
/// (`smtp.transaction` → `simmer.spool.accept`, with the admission decision
/// and the body's `put`), and the dispatcher's (`simmer.spool.attempt` →
/// `simmer.relay` → the usual tree, with the body's `get` and `delete`).
#[cfg(feature = "postgres")]
#[sqlx::test]
async fn a_spooled_message_is_an_accept_trace_and_an_attempt_trace(pool: sqlx::PgPool) {
    exported();
    const SPOOLED_RECIPIENT: &str = "spool-telemetry-victim@example.net";
    let downstream = FakeDownstream::start(Script::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let yaml = config_for(
        downstream.addr,
        &format!(
            "{CORRELATION_HEADER}  delivery: spool\nspool:\n  body_store: {{ kind: volume, path: \"{}\" }}\n  \
             dispatch: {{ poll_interval: 50ms, batch: 8 }}\n",
            dir.path().display()
        ),
    );
    let store = std::sync::Arc::new(simmer::quota::PgQuotaStore::new(pool));
    let simmer = Simmer::start_spooled(&yaml, store).await;
    let mut client = simmer.connect().await;
    client.hello().await;
    let reply = client
        .deliver(
            "jane@oldbrand.com",
            SPOOLED_RECIPIENT,
            "From: jane@oldbrand.com\r\nSubject: spooled\r\n\r\nhello\r\n",
        )
        .await;
    assert_eq!(reply.code, 250, "{reply:?}");
    let spool_id = reply.text().rsplit(' ').next().unwrap().trim().to_string();
    client.command("QUIT").await;
    drop(client);

    // Delivered, then the attempt span closes.
    for _ in 0..100 {
        if exported()
            .spans
            .get_finished_spans()
            .unwrap()
            .iter()
            .any(|s| {
                s.name == "simmer.spool.attempt"
                    && attr_str(s, "spool_id").as_deref() == Some(&spool_id)
            })
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let all = exported().spans.get_finished_spans().unwrap();

    // The accept.
    let accept = all
        .iter()
        .find(|s| {
            s.name == "simmer.spool.accept" && attr_str(s, "spool_id").as_deref() == Some(&spool_id)
        })
        .expect("a simmer.spool.accept span with the id the client was given");
    assert_eq!(attr_str(accept, "admission").as_deref(), Some("admit"));
    assert_eq!(attr_str(accept, "body_store").as_deref(), Some("volume"));
    assert_eq!(attr_str(accept, "smtp.reply.code").as_deref(), Some("250"));
    let correlation_id = attr_str(accept, "correlation_id").expect("correlation_id");
    let client_trace = trace_for(&correlation_id);
    let tx = named(&client_trace, "smtp.transaction");
    assert!(child_of(accept, tx));
    assert_eq!(attr_str(tx, "delivery").as_deref(), Some("spool"));
    assert_eq!(attr_str(tx, "spool_id").as_deref(), Some(spool_id.as_str()));
    let put = client_trace
        .iter()
        .find(|s| s.name == "simmer.spool.body.put" && attr_str(s, "op").as_deref() == Some("put"))
        .expect("the body's put");
    assert!(child_of(put, accept));
    assert_eq!(attr_str(put, "outcome").as_deref(), Some("ok"));
    assert!(client_trace
        .iter()
        .any(|s| s.name == "simmer.spool.admission"));
    assert!(client_trace
        .iter()
        .any(|s| s.name == "simmer.spool.enqueue"));
    assert!(
        !client_trace.iter().any(|s| s.name == "simmer.relay"),
        "nothing is relayed on the client's time"
    );

    // The attempt.
    let attempt = all
        .iter()
        .find(|s| {
            s.name == "simmer.spool.attempt"
                && attr_str(s, "spool_id").as_deref() == Some(&spool_id)
        })
        .expect("a simmer.spool.attempt span");
    assert_eq!(attr_str(attempt, "outcome").as_deref(), Some("delivered"));
    assert_eq!(attr_str(attempt, "attempt").as_deref(), Some("1"));
    assert_eq!(attr_str(attempt, "route").as_deref(), Some("only"));
    assert_eq!(
        attr_str(attempt, "correlation_id").as_deref(),
        Some(correlation_id.as_str()),
        "the attempt joins the accept through the client's correlation id"
    );
    let attempt_trace: Vec<_> = all
        .iter()
        .filter(|s| s.span_context.trace_id() == attempt.span_context.trace_id())
        .cloned()
        .collect();
    let relay = named(&attempt_trace, "simmer.relay");
    assert!(child_of(relay, attempt));
    assert_eq!(attr_str(relay, "spooled").as_deref(), Some("true"));
    assert_eq!(
        attr_str(relay, "spool_id").as_deref(),
        Some(spool_id.as_str())
    );
    let resolve = named(&attempt_trace, "simmer.quota.resolve");
    assert_eq!(
        attr_str(resolve, "spool_lease_held").as_deref(),
        Some("true")
    );
    for op in ["get", "delete"] {
        assert!(
            attempt_trace
                .iter()
                .any(|s| s.name == format!("simmer.spool.body.{op}")
                    && attr_str(s, "body_store").as_deref() == Some("volume")),
            "the body's {op} is in the attempt's trace"
        );
    }

    // And neither trace names the recipient.
    let local = SPOOLED_RECIPIENT.split('@').next().unwrap();
    for s in client_trace.iter().chain(attempt_trace.iter()) {
        let text = format!("{:?} {:?}", s.attributes, s.events);
        assert!(
            !text.contains(local),
            "span {} carries the recipient: {text}",
            s.name
        );
    }
}
