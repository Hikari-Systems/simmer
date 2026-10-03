//! §9.5 structured logging, and §9.6's export layers (D-101).
//!
//! > Structured JSON. Every message carries a `correlation_id` propagated through
//! > every log line... Message bodies are never logged; recipient addresses are
//! > logged only at `DEBUG`.
//!
//! The house helper is `hs_utils::logging::init`, which emits human-readable
//! text with no JSON option, so this calls `tracing_subscriber` directly — see
//! `DECISIONS.md` D-006. Since D-060 the crate is not a dependency at all, so
//! this is now simply how logging is done here rather than a divergence from a
//! shared implementation. The `text` format exists for local development, where
//! JSON on a terminal is unreadable.
//!
//! Since D-101 the message path runs inside spans (`smtp.session`,
//! `smtp.transaction` and their children), and a JSON line carries the fields
//! of the span it was emitted in — which is what puts `correlation_id` on the
//! downstream outcome lines that never had it. Only the innermost span is
//! included (`with_span_list(false)`): the chain of parents repeats fields the
//! transaction span already has, on every line.

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use crate::config::LogFormat;
use crate::telemetry::Pipeline;

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// Initialise the global subscriber. Call once, immediately after config load
/// and before anything that logs — the pool builder and the migrator both emit
/// on their own.
///
/// `telemetry` adds the OTLP span and log layers when §9.6's export is on. Each
/// layer has its own filter: stdout's is `logging.level` (or `RUST_LOG`), the
/// exporters' is `telemetry.level`, and neither can widen the other.
pub fn init(level: &str, format: LogFormat, telemetry: Option<&Pipeline>) {
    tracing_subscriber::registry()
        .with(layers(level, format, telemetry))
        .init();
}

/// The layers [`init`] installs, unassembled — so a test can build the same
/// stack against its own exporters with `tracing::subscriber::set_default`.
pub fn layers(level: &str, format: LogFormat, telemetry: Option<&Pipeline>) -> Vec<BoxedLayer> {
    // As in the house pattern, the level string is a full EnvFilter directive, so
    // `"info,sqlx=warn"` works. RUST_LOG still wins when set, for debugging a
    // container without editing its config.
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let stdout: BoxedLayer = match format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(false)
            .with_filter(filter)
            .boxed(),
        // ANSI off to match the house convention: these logs are shipped to
        // CloudWatch, where escape codes are noise.
        LogFormat::Text => tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(false)
            .with_filter(filter)
            .boxed(),
    };

    let mut out = vec![stdout];
    if let Some(t) = telemetry {
        if let Some(tracer) = t.tracer() {
            out.push(
                tracing_opentelemetry::layer()
                    .with_tracer(tracer)
                    // Thread ids and names describe the runtime, not the message.
                    .with_threads(false)
                    .with_filter(t.filter())
                    .boxed(),
            );
        }
        if let Some(logger) = &t.logger {
            out.push(
                opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(logger)
                    .with_filter(t.filter())
                    .boxed(),
            );
        }
    }
    out
}
