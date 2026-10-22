//! D-114 — `telemetry.recipients`, captured in memory.
//!
//! Its own binary for `tests/telemetry.rs`'s reason, and a second one rather
//! than more tests there: that binary's `no_recipient_address_is_exported`
//! scans every span it holds, and the spans here carry recipients on purpose.
//!
//! The stdout layer is installed too, writing into a buffer, because the one
//! way this feature could break §9.5 is a recipient recorded as a `tracing`
//! field — which stdout prints on every line inside the span.

mod support;

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use opentelemetry::Value;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use support::{config_for, Act, FakeDownstream, Script, Simmer};
use tracing_subscriber::layer::SubscriberExt;

const KEY: &str = "0123456789abcdef";

struct Exported {
    spans: InMemorySpanExporter,
    stdout: Arc<Mutex<Vec<u8>>>,
}

#[derive(Clone)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("not poisoned").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn exported() -> &'static Exported {
    static ONCE: OnceLock<Exported> = OnceLock::new();
    ONCE.get_or_init(|| {
        let spans = InMemorySpanExporter::default();
        let tracer = SdkTracerProvider::builder()
            .with_simple_exporter(spans.clone())
            .build();
        let pipeline = simmer::telemetry::Pipeline::from_providers(
            Some(tracer),
            None,
            None,
            "info",
            Duration::from_secs(60),
        );
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let sink = Buffer(Arc::clone(&stdout));
        // `debug` is where §9.5 does allow addresses; the guard is for INFO.
        let layers = simmer::logging::layers_to(
            "info",
            simmer::config::LogFormat::Json,
            Some(&pipeline),
            move || sink.clone(),
        );
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layers))
            .expect("the only subscriber in this binary");
        Exported { spans, stdout }
    })
}

fn stdout() -> String {
    String::from_utf8_lossy(&exported().stdout.lock().expect("not poisoned")).into_owned()
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

fn named<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    spans
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no {name} span in the trace"))
}

/// The config, with a telemetry block exporting `recipients`. The endpoint is
/// never dialled: the test installs its own pipeline.
fn config(downstream: &FakeDownstream, recipients: &str) -> String {
    let key = if recipients == "hashed" {
        format!("  recipient_hash_key: \"{KEY}\"\n")
    } else {
        String::new()
    };
    format!(
        "{}telemetry:\n  endpoint: \"http://127.0.0.1:4317\"\n  recipients: {recipients}\n{key}",
        config_for(downstream.addr, "")
    )
}

/// Every span of the trace whose `smtp.downstream` went to `downstream`.
async fn deliver(recipients: &str, to: &str, script: Script) -> Vec<SpanData> {
    exported();
    let downstream = FakeDownstream::start(script).await;
    let simmer = Simmer::start(&config(&downstream, recipients)).await;
    let mut client = simmer.connect().await;
    client.hello().await;
    client
        .deliver(
            "jane@oldbrand.com",
            to,
            "From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n",
        )
        .await;
    client.command("QUIT").await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let all = exported().spans.get_finished_spans().expect("spans");
    let port = downstream.addr.port().to_string();
    let trace = all
        .iter()
        .find(|s| s.name == "smtp.downstream" && attr_str(s, "server.port") == Some(port.clone()))
        .expect("a smtp.downstream span for this test's downstream")
        .span_context
        .trace_id();
    all.into_iter()
        .filter(|s| s.span_context.trace_id() == trace)
        .collect()
}

fn rejecting(text: &'static str) -> Script {
    Script::with(|s| s.rcpt_to = Act::Reply(550, text))
}

#[tokio::test]
async fn none_exports_no_recipient_attribute() {
    let trace = deliver("none", "none-mode@example.com", Script::default()).await;
    for s in &trace {
        let text = format!("{:?} {:?}", s.attributes, s.events);
        assert!(!text.contains("simmer.recipient"), "{}: {text}", s.name);
        assert!(!text.contains("none-mode"), "{}: {text}", s.name);
        assert!(attr(s, "smtp.response.text").is_none(), "{}", s.name);
    }
}

#[tokio::test]
async fn domain_exports_the_domain_and_never_the_local_part() {
    let trace = deliver(
        "domain",
        "domain-mode@Example.com",
        rejecting("5.1.1 <domain-mode@example.com> unknown user"),
    )
    .await;
    for name in ["smtp.transaction", "simmer.relay"] {
        let span = named(&trace, name);
        assert_eq!(
            attr_str(span, "simmer.recipient.domain").as_deref(),
            Some("example.com"),
            "{name}"
        );
        assert!(attr(span, "simmer.recipient.hash").is_none(), "{name}");
        assert!(attr(span, "simmer.recipient.address").is_none(), "{name}");
    }
    let down = named(&trace, "smtp.downstream");
    assert_eq!(
        attr_str(down, "smtp.response.text").as_deref(),
        Some("5.1.1 <*@example.com> unknown user")
    );
    for s in &trace {
        let text = format!("{:?} {:?}", s.attributes, s.events);
        assert!(!text.contains("domain-mode"), "{}: {text}", s.name);
    }
}

#[tokio::test]
async fn hashed_exports_a_keyed_hash_and_never_the_local_part() {
    let trace = deliver("hashed", "Hashed.Mode+tag@gmail.com", Script::default()).await;
    let tx = named(&trace, "smtp.transaction");
    let relay = named(&trace, "simmer.relay");
    let hash = attr_str(tx, "simmer.recipient.hash").expect("hash on the transaction");
    assert_eq!(hash.len(), 32);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(attr_str(relay, "simmer.recipient.hash"), Some(hash));
    assert!(attr(tx, "simmer.recipient.address").is_none());
    // A 2xx's text carries the provider's queue id, and is exported too.
    assert!(attr(named(&trace, "smtp.downstream"), "smtp.response.text").is_some());
    for s in &trace {
        let text = format!("{:?} {:?}", s.attributes, s.events).to_lowercase();
        assert!(
            !text.contains("hashed.mode") && !text.contains("hashedmode"),
            "{}: {text}",
            s.name
        );
    }
}

#[tokio::test]
async fn address_exports_the_address_and_stdout_still_never_sees_it() {
    let trace = deliver(
        "address",
        "address-mode@example.com",
        rejecting("5.1.1 <address-mode@example.com> unknown user"),
    )
    .await;
    for name in ["smtp.transaction", "simmer.relay"] {
        let span = named(&trace, name);
        assert_eq!(
            attr_str(span, "simmer.recipient.address").as_deref(),
            Some("address-mode@example.com"),
            "{name}"
        );
        // `address` without a key exports no hash: it is `hashed`'s.
        assert!(attr(span, "simmer.recipient.hash").is_none(), "{name}");
    }
    assert_eq!(
        attr_str(named(&trace, "smtp.downstream"), "smtp.response.text").as_deref(),
        Some("5.1.1 <address-mode@example.com> unknown user")
    );

    // The regression this binary exists for: span-only, so §9.5 holds for
    // stdout even here.
    let out = stdout();
    assert!(!out.is_empty(), "the stdout layer should have written");
    assert!(
        !out.contains("address-mode"),
        "stdout carries the recipient"
    );
}

#[tokio::test]
async fn a_refused_rcpt_is_an_event_on_the_transaction() {
    exported();
    let downstream = FakeDownstream::start(Script::default()).await;
    let simmer = Simmer::start(&config(&downstream, "address")).await;
    let mut client = simmer.connect().await;
    client.hello().await;
    assert_eq!(
        client.command("MAIL FROM:<jane@oldbrand.com>").await.code,
        250
    );
    assert_eq!(
        client
            .command("RCPT TO:<first-rcpt@example.com>")
            .await
            .code,
        250
    );
    // D-047: one recipient per transaction.
    assert_eq!(
        client
            .command("RCPT TO:<second-rcpt@example.com>")
            .await
            .code,
        452
    );
    client.command("QUIT").await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let all = exported().spans.get_finished_spans().expect("spans");
    let tx = all
        .iter()
        .find(|s| {
            s.name == "smtp.transaction"
                && attr_str(s, "simmer.recipient.address").as_deref()
                    == Some("first-rcpt@example.com")
        })
        .expect("the transaction, carrying its accepted recipient");
    let event = tx
        .events
        .iter()
        .find(|e| e.name == simmer::telemetry::recipient::RCPT_REFUSED)
        .expect("the refused RCPT as an event");
    let get = |k: &str| {
        event
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == k)
            .map(|kv| kv.value.as_str().into_owned())
    };
    assert_eq!(
        get("simmer.recipient.address").as_deref(),
        Some("second-rcpt@example.com")
    );
    assert_eq!(get("smtp.reply.code").as_deref(), Some("452"));
    assert!(!stdout().contains("second-rcpt"));
}
