//! §9.6 (D-101) — optional OTLP export of traces, metrics and logs.
//!
//! Off unless `telemetry:` is configured with a non-empty endpoint
//! ([`crate::config::Config::telemetry`]). When off, nothing here is built: no
//! provider, no exporter, no background thread, and the subscriber has only its
//! stdout layer.
//!
//! Four properties keep this from ever being more than an observer:
//!
//! 1. **It cannot change what Simmer emits.** No trace context is propagated
//!    into relayed mail or link-proxy requests — a `traceparent` header on an
//!    outbound message is output no application-side configuration produces
//!    (§1.1).
//! 2. **It cannot delay mail.** The exporters' gRPC channel connects lazily and
//!    every export runs on the SDK's batch workers, so a dead or slow collector
//!    costs dropped telemetry, never relay latency. A collector that is down at
//!    startup is not a startup failure.
//! 3. **It exports no more than stdout may log.** §9.5's rules — no bodies, no
//!    recipient addresses above `DEBUG`, no link-proxy query strings — hold for
//!    the export because the export is built from the same events. Its own
//!    `level` is not reachable by `RUST_LOG`, so a container turned up to
//!    `debug` for diagnosis does not start shipping debug lines to a vendor.
//! 4. **The YAML is the configuration.** The resource is built empty, so
//!    `OTEL_SERVICE_NAME`/`OTEL_RESOURCE_ATTRIBUTES` cannot relabel an instance
//!    behind its config, and the endpoint and timeout are set explicitly, which
//!    the SDK ranks above its environment variables. (It still *adds*
//!    `OTEL_EXPORTER_OTLP_HEADERS` and honours `…_COMPRESSION`/`…_INSECURE` if
//!    set; D-101 records that.)

pub mod metrics;

use std::time::Duration;

use anyhow::Context as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
use opentelemetry_otlp::tonic_types::transport::ClientTlsConfig;
use opentelemetry_otlp::{WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use opentelemetry_sdk::Resource;
use tracing_subscriber::EnvFilter;

use crate::config;

/// The instrumentation scope every span, log and metric is reported under.
const SCOPE: &str = "simmer";

/// Targets never exported, whatever `telemetry.level` says. The exporter's own
/// stack logs through `tracing` too, and an export failure exported as a log
/// record is a feedback loop: each failed batch would enqueue another. They
/// still reach stdout, which is where an operator diagnosing a dead collector
/// will look.
const NEVER_EXPORTED: &[&str] = &[
    "opentelemetry",
    "opentelemetry_sdk",
    "opentelemetry_otlp",
    "tonic",
    "h2",
    "hyper",
    "hyper_util",
    "tower",
];

/// The running export pipeline. Held by `main` for the life of the process and
/// shut down last, so the spans of the final drained sessions are flushed.
pub struct Pipeline {
    pub tracer: Option<SdkTracerProvider>,
    pub meter: Option<SdkMeterProvider>,
    pub logger: Option<SdkLoggerProvider>,
    level: String,
    metrics_interval: Duration,
}

/// Build the providers `cfg` asks for.
///
/// Must be called inside the Tokio runtime: tonic spawns its channel's worker
/// there. Fails only on a configuration the exporter refuses, which §4.2 has
/// already checked — so a failure here is the second line of defence, and
/// `main` refuses to start on it rather than run with an export that silently
/// sends nothing (D-085's reasoning, D-101).
pub fn init(cfg: &config::Telemetry, backend: &str) -> anyhow::Result<Pipeline> {
    let resource = resource(cfg, backend);

    let tracer = if cfg.traces {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(&cfg.endpoint)
            .with_timeout(cfg.timeout)
            .with_metadata(metadata(cfg)?)
            .with_tls_config(tls())
            .build()
            .context("building the OTLP span exporter")?;
        Some(
            SdkTracerProvider::builder()
                .with_resource(resource.clone())
                .with_sampler(Sampler::TraceIdRatioBased(cfg.sample_ratio))
                .with_batch_exporter(exporter)
                .build(),
        )
    } else {
        None
    };

    let meter = if cfg.metrics {
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_temporality(temporality(cfg.metrics_temporality))
            .with_tonic()
            .with_endpoint(&cfg.endpoint)
            .with_timeout(cfg.timeout)
            .with_metadata(metadata(cfg)?)
            .with_tls_config(tls())
            .build()
            .context("building the OTLP metric exporter")?;
        Some(
            SdkMeterProvider::builder()
                .with_resource(resource.clone())
                .with_reader(
                    PeriodicReader::builder(exporter)
                        .with_interval(cfg.metrics_interval)
                        .build(),
                )
                .build(),
        )
    } else {
        None
    };

    let logger = if cfg.logs {
        let exporter = opentelemetry_otlp::LogExporter::builder()
            .with_tonic()
            .with_endpoint(&cfg.endpoint)
            .with_timeout(cfg.timeout)
            .with_metadata(metadata(cfg)?)
            .with_tls_config(tls())
            .build()
            .context("building the OTLP log exporter")?;
        Some(
            SdkLoggerProvider::builder()
                .with_resource(resource)
                .with_batch_exporter(exporter)
                .build(),
        )
    } else {
        None
    };

    Ok(Pipeline {
        tracer,
        meter,
        logger,
        level: cfg.level.clone(),
        metrics_interval: cfg.metrics_interval,
    })
}

impl Pipeline {
    /// A pipeline over providers the caller built — `tests/telemetry.rs`, which
    /// gives them in-memory exporters and then installs exactly the layers
    /// [`crate::logging::init`] would.
    #[doc(hidden)]
    pub fn from_providers(
        tracer: Option<SdkTracerProvider>,
        meter: Option<SdkMeterProvider>,
        logger: Option<SdkLoggerProvider>,
        level: &str,
        metrics_interval: Duration,
    ) -> Self {
        Self {
            tracer,
            meter,
            logger,
            level: level.to_string(),
            metrics_interval,
        }
    }

    /// The filter for an exporting layer: `telemetry.level`, with
    /// [`NEVER_EXPORTED`] switched off. Built per layer, since a per-layer
    /// filter is owned by its layer.
    pub fn filter(&self) -> EnvFilter {
        let mut f = EnvFilter::try_new(&self.level).unwrap_or_else(|_| EnvFilter::new("info"));
        for target in NEVER_EXPORTED {
            if let Ok(d) = format!("{target}=off").parse() {
                f = f.add_directive(d);
            }
        }
        f
    }

    /// The tracer spans are recorded into, if traces are exported.
    pub fn tracer(&self) -> Option<opentelemetry_sdk::trace::SdkTracer> {
        self.tracer.as_ref().map(|p| p.tracer(SCOPE))
    }

    /// The §9.1 metrics bridge, if metrics are exported. Handed to
    /// `crate::metrics::install`, which fans the facade out to it.
    pub fn recorder(&self) -> Option<metrics::OtelRecorder> {
        use opentelemetry::metrics::MeterProvider as _;
        self.meter
            .as_ref()
            .map(|p| metrics::OtelRecorder::new(p.meter(SCOPE), crate::metrics::HISTOGRAM_BUCKETS))
    }

    /// How often the scrape-time gauges are recomputed for export, if metrics
    /// are exported.
    pub fn gauge_interval(&self) -> Option<Duration> {
        self.meter.as_ref().map(|_| self.metrics_interval)
    }

    /// Flush and stop every provider. Blocking: each provider waits for its
    /// final export, bounded by the configured timeout.
    pub fn shutdown(self) {
        if let Some(p) = self.tracer {
            if let Err(e) = p.shutdown() {
                tracing::warn!(error = %e, "telemetry: span export did not shut down cleanly");
            }
        }
        if let Some(p) = self.meter {
            if let Err(e) = p.shutdown() {
                tracing::warn!(error = %e, "telemetry: metric export did not shut down cleanly");
            }
        }
        if let Some(p) = self.logger {
            if let Err(e) = p.shutdown() {
                // Not logged through `tracing`: the log exporter is the thing
                // that has just stopped.
                eprintln!("simmer: telemetry: log export did not shut down cleanly: {e}");
            }
        }
    }
}

/// `service.*` from the config and the build, then `telemetry.resource`.
/// `builder_empty`, not `builder`: the latter reads `OTEL_SERVICE_NAME` and
/// `OTEL_RESOURCE_ATTRIBUTES`, and the YAML is the configuration.
fn resource(cfg: &config::Telemetry, backend: &str) -> Resource {
    let mut attrs = vec![
        KeyValue::new("service.name", cfg.service_name.clone()),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        // Instances of one deployment usually share a config and therefore a
        // hostname; this is what tells their series apart.
        KeyValue::new("service.instance.id", uuid::Uuid::new_v4().to_string()),
        KeyValue::new("simmer.backend", backend.to_string()),
    ];
    attrs.extend(
        cfg.resource
            .iter()
            .map(|(k, v)| KeyValue::new(k.clone(), v.clone())),
    );
    Resource::builder_empty().with_attributes(attrs).build()
}

/// `telemetry.metrics_temporality` as the SDK's selector. `Delta` is the SDK's
/// `LowMemory`, not its `Delta`: the latter makes gauges delta too, and a delta
/// gauge is dropped from every export in which it did not change.
fn temporality(t: config::MetricsTemporality) -> opentelemetry_sdk::metrics::Temporality {
    use opentelemetry_sdk::metrics::Temporality;
    match t {
        config::MetricsTemporality::Delta => Temporality::LowMemory,
        config::MetricsTemporality::Cumulative => Temporality::Cumulative,
    }
}

/// `telemetry.headers` as gRPC metadata. §4.2 has checked each one parses, by
/// the same `http` types used here.
fn metadata(cfg: &config::Telemetry) -> anyhow::Result<MetadataMap> {
    let mut headers = http::HeaderMap::new();
    for (name, value) in &cfg.headers {
        let n = http::HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("telemetry.headers.{name} is not a valid metadata key"))?;
        let v = http::HeaderValue::from_str(value)
            .with_context(|| format!("telemetry.headers.{name} has an invalid value"))?;
        headers.insert(n, v);
    }
    Ok(MetadataMap::from_headers(headers))
}

/// §8.2's trust: the ring provider (the crate's only one) and the platform
/// root store. Applied only to an `https://` endpoint; `http://` stays plain.
fn tls() -> ClientTlsConfig {
    ClientTlsConfig::new().with_native_roots()
}
