//! D-120 — one POST per dead letter. Bounded by the configured timeout, tried
//! twice, and never able to change what became of the message: a webhook that
//! is down is logged and counted, and the dead letter stands.

use http_body_util::Full;
use hyper::body::Bytes;
use serde::Serialize;

use super::http::{self, HttpsClient};
use crate::config::Webhook;
use crate::metrics;

#[derive(Debug, Clone, Serialize)]
pub struct DeadLetterEvent {
    /// The spool id, as `250 queued as <id>` told the client.
    pub id: String,
    pub ramp: String,
    pub domain_group: String,
    pub route: Option<String>,
    pub reason: &'static str,
    pub code: Option<i64>,
    pub text: Option<String>,
    pub attempts: i64,
    pub received_at: chrono::DateTime<chrono::Utc>,
    pub dead_at: chrono::DateTime<chrono::Utc>,
    /// Present only with `include_addresses: true` (the default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mail_from: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rcpt: Option<Vec<String>>,
}

const ATTEMPTS: u32 = 2;

/// Fire and forget: spawned, so a slow webhook never holds a dispatcher slot.
pub fn notify(client: HttpsClient, cfg: Webhook, event: DeadLetterEvent) {
    tokio::spawn(async move {
        let body = serde_json::to_vec(&event).expect("an event always serialises");
        for attempt in 1..=ATTEMPTS {
            let req = hyper::Request::post(cfg.url.as_str())
                .header(hyper::header::CONTENT_TYPE, "application/json")
                .header(hyper::header::USER_AGENT, "simmer-dead-letter/1")
                .body(Full::new(Bytes::from(body.clone())));
            let req = match req {
                Ok(r) => r,
                Err(e) => {
                    // The URL passed §4.2; this would be a bug, and is said so.
                    tracing::error!(error = %e, "building the dead-letter webhook request");
                    metrics::spool_webhook("error");
                    return;
                }
            };
            match http::send(&client, req, cfg.timeout).await {
                Ok(r) if (200..300).contains(&r.status) => {
                    metrics::spool_webhook("ok");
                    return;
                }
                Ok(r) => tracing::warn!(
                    spool_id = %event.id,
                    status = r.status,
                    attempt,
                    "dead-letter webhook refused the event"
                ),
                Err(e) => tracing::warn!(
                    spool_id = %event.id,
                    error = %e,
                    attempt,
                    "dead-letter webhook failed"
                ),
            }
        }
        metrics::spool_webhook("error");
    });
}
