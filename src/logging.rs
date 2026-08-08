//! §9.5 structured logging.
//!
//! > Structured JSON. Every message carries a `correlation_id` propagated through
//! > every log line... Message bodies are never logged; recipient addresses are
//! > logged only at `DEBUG`.
//!
//! This does not use `hs_utils::logging::init`, which emits human-readable text
//! with no JSON option. See `DECISIONS.md` D-006. The `text` format exists for
//! local development, where JSON on a terminal is unreadable.

use tracing_subscriber::EnvFilter;

use crate::config::LogFormat;

/// Initialise the global subscriber. Call once, immediately after config load and
/// before anything that logs — the pool builder and the migrator both emit on
/// their own.
pub fn init(level: &str, format: LogFormat) {
    // As in the house pattern, the level string is a full EnvFilter directive, so
    // `"info,sqlx=warn"` works. RUST_LOG still wins when set, for debugging a
    // container without editing its config.
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let builder = tracing_subscriber::fmt().with_env_filter(filter);

    match format {
        LogFormat::Json => builder.json().with_current_span(true).init(),
        // ANSI off to match the house convention: these logs are shipped to
        // CloudWatch, where escape codes are noise.
        LogFormat::Text => builder.with_ansi(false).with_target(false).init(),
    }
}
