//! D-113 end to end: a ramp with `allow_no_overflow_route` and a message that
//! finds no eligible route, over a real SMTP session.
//!
//! The ramp's only route is a warming route, paused through the in-memory quota,
//! so the chain walk is exhausted and there is no overflow route to fall to.
//! Default: a descriptive `550`, nothing relayed. With `black_hole_allowed`:
//! `250`, nothing relayed, nothing persisted. Both emit the two telemetry events
//! (the reply the sender got; the action taken), asserted here through a
//! thread-local subscriber that records tracing events by field.

mod support;

use std::sync::{Arc, Mutex};

use support::{config_for, FakeDownstream, GrantAllQuota, Script, Simmer};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

const WARMING: &str = "    warmup:\n      started: \"2026-01-01T00:00:00Z\"\n      \
                       schedule: { default: [10] }\n";

fn yaml(downstream: &FakeDownstream, ramp_keys: &str) -> String {
    // `config_for`'s one route is an overflow route; make it a warming one, so
    // the ramp has none.
    config_for(downstream.addr, ramp_keys).replace("    overflow: true\n", WARMING)
}

#[derive(Default, Debug, Clone)]
struct Seen {
    fields: Vec<(String, String)>,
}

impl Seen {
    fn get(&self, k: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }
}

impl Visit for Seen {
    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        self.fields.push((f.name().to_string(), format!("{v:?}")));
    }
    fn record_str(&mut self, f: &Field, v: &str) {
        self.fields.push((f.name().to_string(), v.to_string()));
    }
}

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Seen>>>);

impl<S: tracing::Subscriber> Layer<S> for Recorder {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let mut seen = Seen::default();
        event.record(&mut seen);
        self.0.lock().expect("not poisoned").push(seen);
    }
}

struct Outcome {
    rcpt: u16,
    data: u16,
    data_text: String,
    forwarded: usize,
    events: Vec<Seen>,
}

async fn run(ramp_keys: &str) -> Outcome {
    let recorder = Recorder::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(recorder.clone()));
    let downstream = FakeDownstream::start(Script::default()).await;
    let quota = Arc::new(GrantAllQuota::new());
    quota.pause("only");
    let simmer = Simmer::start_with_quota(&yaml(&downstream, ramp_keys), quota).await;
    let mut c = simmer.connect().await;
    c.hello().await;
    assert_eq!(c.command("MAIL FROM:<jane@oldbrand.com>").await.code, 250);
    let rcpt = c.command("RCPT TO:<bob@example.net>").await.code;
    assert_eq!(c.command("DATA").await.code, 354);
    c.send_raw(b"From: jane@oldbrand.com\r\nSubject: hi\r\n\r\nhello\r\n.\r\n")
        .await;
    let r = c.read_reply().await;
    let events = recorder.0.lock().expect("not poisoned").clone();
    Outcome {
        rcpt,
        data: r.code,
        data_text: r.text(),
        forwarded: downstream.messages().len(),
        events,
    }
}

fn event<'a>(o: &'a Outcome, name: &str) -> &'a Seen {
    o.events
        .iter()
        .find(|e| e.get("event") == Some(name))
        .unwrap_or_else(|| panic!("no {name} event in {:?}", o.events))
}

#[tokio::test]
async fn a_no_overflow_ramp_rejects_by_default_and_says_why() {
    let o = run("  allow_no_overflow_route: true\n").await;
    assert_eq!(o.rcpt, 250, "the decision is made once, at the end of data");
    assert_eq!(o.data, 550);
    assert!(o.data_text.contains("no overflow route"), "{}", o.data_text);
    assert_eq!(o.forwarded, 0, "nothing is relayed");

    let reply = event(&o, "simmer.no_overflow.reply");
    assert_eq!(reply.get("smtp.reply.code"), Some("550"));
    assert_eq!(reply.get("smtp.reply.enhanced_status"), Some("5.7.1"));
    assert_eq!(reply.get("reply_kind"), Some("rejection"));
    let action = event(&o, "simmer.no_overflow.action");
    assert_eq!(action.get("action"), Some("rejected_no_overflow"));
    assert_eq!(action.get("forwarded"), Some("false"));
}

#[tokio::test]
async fn black_hole_allowed_answers_250_and_relays_nothing() {
    let o = run("  allow_no_overflow_route: true\n  black_hole_allowed: true\n").await;
    assert_eq!(o.rcpt, 250);
    assert_eq!(o.data, 250, "the sender is told it succeeded");
    assert_eq!(o.forwarded, 0, "and nothing was relayed");

    let reply = event(&o, "simmer.no_overflow.reply");
    assert_eq!(reply.get("smtp.reply.code"), Some("250"));
    assert_eq!(reply.get("smtp.reply.enhanced_status"), Some("2.0.0"));
    assert_eq!(reply.get("reply_kind"), Some("black_hole_success"));
    let action = event(&o, "simmer.no_overflow.action");
    assert_eq!(action.get("action"), Some("discarded_black_hole"));
    assert_eq!(action.get("persisted"), Some("false"));
}
