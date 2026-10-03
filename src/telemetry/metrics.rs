//! D-126 — the §9.1 metrics over OTLP.
//!
//! Every metric in this crate is recorded through the `metrics` facade, behind
//! the named functions in `crate::metrics`, so exporting them over OTLP is a
//! second [`Recorder`] beside the Prometheus one rather than a second set of
//! call sites. `crate::metrics::install` fans the facade out to whichever of the
//! two are enabled.
//!
//! Written here rather than taken from `metrics-exporter-opentelemetry`, which
//! pins OpenTelemetry 0.31 and would put a second OpenTelemetry in the graph
//! beside the 0.33 everything else uses (D-126).
//!
//! The mapping:
//!
//! - a counter is an OTel `u64` counter. `absolute` has no OTel equivalent
//!   without per-series state, and nothing in this crate calls it, so it is a
//!   no-op rather than a map that would grow with every client-chosen label
//!   (F7 — `simmer_unmatched_sender_total{domain}`);
//! - a gauge is an OTel `f64` gauge behind one cell per series, because the
//!   facade's `increment`/`decrement` are relative and an OTel gauge records
//!   only absolute values. F17's `simmer_capture_disk_bytes` is written both
//!   ways on purpose, so this matters. Gauge label sets are bounded by the
//!   configuration (ramps, routes, groups), so the cells are too;
//! - a histogram is an OTel `f64` histogram with the same explicit buckets the
//!   Prometheus exporter is given.
//!
//! Names are the §9.1 names, unchanged. A metric has one spelling (see the
//! `crate::metrics` module doc), and that holds across both exporters.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use metrics::{
    Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit,
};
use opentelemetry::metrics::Meter;
use opentelemetry::KeyValue;

/// A `metrics::Recorder` that records into an OpenTelemetry [`Meter`].
pub struct OtelRecorder {
    meter: Meter,
    /// `describe_*` arrives before the first registration of a metric (it is
    /// called at install), and OTel fixes an instrument's description and unit
    /// at creation, so they are held here until then.
    descriptions: Mutex<HashMap<String, (Option<Unit>, SharedString)>>,
    counters: Mutex<HashMap<String, opentelemetry::metrics::Counter<u64>>>,
    gauges: Mutex<HashMap<String, opentelemetry::metrics::Gauge<f64>>>,
    histograms: Mutex<HashMap<String, opentelemetry::metrics::Histogram<f64>>>,
    /// One cell per gauge series — see the module doc for why gauges, alone,
    /// need state.
    gauge_cells: Mutex<HashMap<Key, Arc<GaugeCell>>>,
    /// Explicit bucket boundaries, by metric name.
    buckets: HashMap<&'static str, &'static [f64]>,
}

impl OtelRecorder {
    /// `buckets` gives histogram boundaries by metric name; a histogram not
    /// named there gets the SDK's defaults.
    pub fn new(meter: Meter, buckets: &[(&'static str, &'static [f64])]) -> Self {
        Self {
            meter,
            descriptions: Mutex::default(),
            counters: Mutex::default(),
            gauges: Mutex::default(),
            histograms: Mutex::default(),
            gauge_cells: Mutex::default(),
            buckets: buckets.iter().copied().collect(),
        }
    }

    fn describe(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        lock(&self.descriptions).insert(key.as_str().to_string(), (unit, description));
    }

    fn description(&self, name: &str) -> (Option<&'static str>, Option<String>) {
        match lock(&self.descriptions).get(name) {
            Some((unit, d)) => (unit.as_ref().map(otel_unit), Some(d.to_string())),
            None => (None, None),
        }
    }
}

/// A poisoned lock here would mean a panic while inserting into a map; the map
/// is still consistent, so carry on rather than turning metrics into a crash.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// UCUM, which is what OTLP's `unit` field means.
fn otel_unit(unit: &Unit) -> &'static str {
    match unit {
        Unit::Count => "1",
        Unit::Percent => "%",
        Unit::Seconds => "s",
        Unit::Milliseconds => "ms",
        Unit::Microseconds => "us",
        Unit::Nanoseconds => "ns",
        Unit::Tebibytes => "TiBy",
        Unit::Gibibytes => "GiBy",
        Unit::Mebibytes => "MiBy",
        Unit::Kibibytes => "KiBy",
        Unit::Bytes => "By",
        Unit::TerabitsPerSecond => "Tbit/s",
        Unit::GigabitsPerSecond => "Gbit/s",
        Unit::MegabitsPerSecond => "Mbit/s",
        Unit::KilobitsPerSecond => "kbit/s",
        Unit::BitsPerSecond => "bit/s",
        Unit::CountPerSecond => "1/s",
    }
}

fn attributes(key: &Key) -> Vec<KeyValue> {
    key.labels()
        .map(|l| KeyValue::new(l.key().to_string(), l.value().to_string()))
        .collect()
}

impl Recorder for OtelRecorder {
    fn describe_counter(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.describe(key, unit, description);
    }

    fn describe_gauge(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.describe(key, unit, description);
    }

    fn describe_histogram(&self, key: KeyName, unit: Option<Unit>, description: SharedString) {
        self.describe(key, unit, description);
    }

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        let name = key.name();
        let instrument = lock(&self.counters)
            .entry(name.to_string())
            .or_insert_with(|| {
                let (unit, description) = self.description(name);
                let mut b = self.meter.u64_counter(name.to_string());
                if let Some(d) = description {
                    b = b.with_description(d);
                }
                if let Some(u) = unit {
                    b = b.with_unit(u);
                }
                b.build()
            })
            .clone();
        Counter::from_arc(Arc::new(OtelCounter {
            instrument,
            attributes: attributes(key),
        }))
    }

    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        let name = key.name();
        let instrument = lock(&self.gauges)
            .entry(name.to_string())
            .or_insert_with(|| {
                let (unit, description) = self.description(name);
                let mut b = self.meter.f64_gauge(name.to_string());
                if let Some(d) = description {
                    b = b.with_description(d);
                }
                if let Some(u) = unit {
                    b = b.with_unit(u);
                }
                b.build()
            })
            .clone();
        let cell = lock(&self.gauge_cells)
            .entry(key.clone())
            .or_insert_with(|| {
                Arc::new(GaugeCell {
                    bits: AtomicU64::new(0f64.to_bits()),
                    instrument,
                    attributes: attributes(key),
                })
            })
            .clone();
        Gauge::from_arc(cell)
    }

    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        let name = key.name();
        let instrument = lock(&self.histograms)
            .entry(name.to_string())
            .or_insert_with(|| {
                let (unit, description) = self.description(name);
                let mut b = self.meter.f64_histogram(name.to_string());
                if let Some(d) = description {
                    b = b.with_description(d);
                }
                if let Some(u) = unit {
                    b = b.with_unit(u);
                }
                if let Some(bounds) = self.buckets.get(name) {
                    b = b.with_boundaries(bounds.to_vec());
                }
                b.build()
            })
            .clone();
        Histogram::from_arc(Arc::new(OtelHistogram {
            instrument,
            attributes: attributes(key),
        }))
    }
}

struct OtelCounter {
    instrument: opentelemetry::metrics::Counter<u64>,
    attributes: Vec<KeyValue>,
}

impl CounterFn for OtelCounter {
    fn increment(&self, value: u64) {
        self.instrument.add(value, &self.attributes);
    }

    /// Not called anywhere in this crate; see the module doc.
    fn absolute(&self, _value: u64) {}
}

struct GaugeCell {
    bits: AtomicU64,
    instrument: opentelemetry::metrics::Gauge<f64>,
    attributes: Vec<KeyValue>,
}

impl GaugeCell {
    fn update(&self, f: impl Fn(f64) -> f64) {
        let mut current = self.bits.load(Ordering::Relaxed);
        loop {
            let next = f(f64::from_bits(current));
            match self.bits.compare_exchange_weak(
                current,
                next.to_bits(),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.instrument.record(next, &self.attributes);
                    return;
                }
                Err(actual) => current = actual,
            }
        }
    }
}

impl GaugeFn for GaugeCell {
    fn increment(&self, value: f64) {
        self.update(|v| v + value);
    }

    fn decrement(&self, value: f64) {
        self.update(|v| v - value);
    }

    fn set(&self, value: f64) {
        self.bits.store(value.to_bits(), Ordering::Release);
        self.instrument.record(value, &self.attributes);
    }
}

struct OtelHistogram {
    instrument: opentelemetry::metrics::Histogram<f64>,
    attributes: Vec<KeyValue>,
}

impl HistogramFn for OtelHistogram {
    fn record(&self, value: f64) {
        self.instrument.record(value, &self.attributes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::with_local_recorder;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
    use opentelemetry_sdk::metrics::{
        InMemoryMetricExporter, InMemoryMetricExporterBuilder, PeriodicReader, SdkMeterProvider,
    };

    fn harness() -> (SdkMeterProvider, InMemoryMetricExporter, OtelRecorder) {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let recorder = OtelRecorder::new(
            provider.meter("test"),
            &[("test_latency_seconds", &[0.1, 1.0, 10.0])],
        );
        (provider, exporter, recorder)
    }

    fn exported(
        provider: &SdkMeterProvider,
        exporter: &InMemoryMetricExporter,
    ) -> Vec<ResourceMetrics> {
        provider.force_flush().expect("flush");
        exporter.get_finished_metrics().expect("metrics")
    }

    fn find<'a>(
        batches: &'a [ResourceMetrics],
        name: &str,
    ) -> &'a opentelemetry_sdk::metrics::data::Metric {
        batches
            .iter()
            .rev()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .find(|m| m.name() == name)
            .unwrap_or_else(|| panic!("{name} was not exported"))
    }

    /// D-126's default `metrics_temporality: delta`, as `telemetry::init`
    /// builds it: a counter series that saw nothing in an interval is not
    /// exported — and so not held — while a gauge that did not change still is.
    /// The first half is what bounds F7's client-controlled label (docs/SOAK.md
    /// §18); the second is why it is the SDK's `LowMemory` and not `Delta`.
    #[test]
    fn under_delta_an_idle_counter_series_is_dropped_and_a_gauge_is_kept() {
        let exporter = InMemoryMetricExporterBuilder::new()
            .with_temporality(crate::telemetry::temporality(
                crate::config::MetricsTemporality::Delta,
            ))
            .build();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let recorder = OtelRecorder::new(provider.meter("test"), &[]);
        let domains = |batches: &[ResourceMetrics]| -> Vec<String> {
            let AggregatedMetrics::U64(MetricData::Sum(sum)) =
                find(batches, "test_unmatched_total").data()
            else {
                panic!("a counter must export as a u64 sum");
            };
            let mut d: Vec<String> = sum
                .data_points()
                .flat_map(|p| p.attributes())
                .filter(|kv| kv.key.as_str() == "domain")
                .map(|kv| kv.value.to_string())
                .collect();
            d.sort();
            d
        };

        with_local_recorder(&recorder, || {
            metrics::counter!("test_unmatched_total", "domain" => "a.test").increment(1);
            metrics::counter!("test_unmatched_total", "domain" => "b.test").increment(1);
            metrics::gauge!("test_level").set(7.0);
        });
        assert_eq!(
            domains(&exported(&provider, &exporter)),
            ["a.test", "b.test"]
        );

        exporter.reset();
        with_local_recorder(&recorder, || {
            metrics::counter!("test_unmatched_total", "domain" => "b.test").increment(1);
        });
        let second = exported(&provider, &exporter);
        assert_eq!(domains(&second), ["b.test"], "a.test was idle all interval");
        let AggregatedMetrics::F64(MetricData::Gauge(g)) = find(&second, "test_level").data()
        else {
            panic!("a gauge must export as an f64 gauge");
        };
        assert_eq!(
            g.data_points().map(|p| p.value()).collect::<Vec<_>>(),
            [7.0],
            "an unchanged gauge must still be exported"
        );
    }

    #[test]
    fn a_counter_keeps_its_name_labels_and_description() {
        let (provider, exporter, recorder) = harness();
        with_local_recorder(&recorder, || {
            metrics::describe_counter!("test_total", "what it counts");
            metrics::counter!("test_total", "route" => "warming").increment(2);
            metrics::counter!("test_total", "route" => "warming").increment(3);
            metrics::counter!("test_total", "route" => "overflow").increment(1);
        });
        let batches = exported(&provider, &exporter);
        let m = find(&batches, "test_total");
        assert_eq!(m.description(), "what it counts");
        let AggregatedMetrics::U64(MetricData::Sum(sum)) = m.data() else {
            panic!("a counter must export as a u64 sum");
        };
        let mut points: Vec<(String, u64)> = sum
            .data_points()
            .map(|p| {
                let route = p
                    .attributes()
                    .find(|kv| kv.key.as_str() == "route")
                    .map(|kv| kv.value.to_string())
                    .unwrap_or_default();
                (route, p.value())
            })
            .collect();
        points.sort();
        assert_eq!(
            points,
            vec![("overflow".to_string(), 1), ("warming".to_string(), 5)]
        );
    }

    #[test]
    fn a_gauge_tracks_increments_decrements_and_sets() {
        let (provider, exporter, recorder) = harness();
        with_local_recorder(&recorder, || {
            // F17's two writers: increments from one, a set from the other.
            metrics::gauge!("test_bytes").increment(10.0);
            metrics::gauge!("test_bytes").increment(5.0);
            metrics::gauge!("test_bytes").decrement(3.0);
        });
        let batches = exported(&provider, &exporter);
        let AggregatedMetrics::F64(MetricData::Gauge(g)) = find(&batches, "test_bytes").data()
        else {
            panic!("a gauge must export as an f64 gauge");
        };
        assert_eq!(g.data_points().next().expect("a point").value(), 12.0);

        with_local_recorder(&recorder, || {
            metrics::gauge!("test_bytes").set(100.0);
            metrics::gauge!("test_bytes").increment(1.0);
        });
        let batches = exported(&provider, &exporter);
        let AggregatedMetrics::F64(MetricData::Gauge(g)) = find(&batches, "test_bytes").data()
        else {
            panic!("a gauge must export as an f64 gauge");
        };
        assert_eq!(g.data_points().next().expect("a point").value(), 101.0);
    }

    #[test]
    fn a_histogram_uses_the_configured_buckets() {
        let (provider, exporter, recorder) = harness();
        with_local_recorder(&recorder, || {
            metrics::histogram!("test_latency_seconds").record(0.05);
            metrics::histogram!("test_latency_seconds").record(5.0);
        });
        let batches = exported(&provider, &exporter);
        let AggregatedMetrics::F64(MetricData::Histogram(h)) =
            find(&batches, "test_latency_seconds").data()
        else {
            panic!("a histogram must export as an f64 histogram");
        };
        let p = h.data_points().next().expect("a point");
        assert_eq!(p.bounds().collect::<Vec<_>>(), vec![0.1, 1.0, 10.0]);
        assert_eq!(p.bucket_counts().collect::<Vec<_>>(), vec![1, 0, 1, 0]);
        assert_eq!(p.count(), 2);
    }
}
