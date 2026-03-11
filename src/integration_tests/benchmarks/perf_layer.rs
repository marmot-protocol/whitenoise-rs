//! PerfTracingLayer — a `tracing_subscriber` layer that captures `whitenoise::perf`
//! events and records their durations.
//!
//! # How it works
//!
//! The layer is registered **once** at benchmark binary startup. Every time a
//! `tracing::info!` event whose target is `"whitenoise::perf"` is recorded, the
//! layer extracts the `name` and `duration_ns` fields and pushes a `PerfSample`
//! into a shared `Vec`. After a benchmark loop finishes, call
//! [`PerfTracingLayer::drain`] to consume the accumulated samples and compute
//! per-marker [`PerfBreakdown`] statistics.
//!
//! The `PerfGuard` type in `src/perf.rs` emits these events on drop, which means
//! the layer is notified synchronously at the moment the guard goes out of scope.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use super::stats;

const PERF_TARGET: &str = "whitenoise::perf";

/// A single timing observation for a named perf marker.
#[derive(Debug, Clone)]
pub struct PerfSample {
    /// The marker name (e.g. `"messages::send_message_to_group"`).
    pub name: String,
    /// Wall-clock duration of the operation.
    pub duration: Duration,
}

/// Aggregated statistics for a single perf marker across many observations.
#[derive(Debug, Clone)]
pub struct PerfBreakdown {
    /// Marker name.
    pub marker: String,
    /// Number of observations.
    pub call_count: u64,
    /// Mean duration.
    pub mean: Duration,
    /// Median duration.
    pub median: Duration,
    /// 95th-percentile duration.
    pub p95: Duration,
    /// 99th-percentile duration.
    pub p99: Duration,
    /// Minimum observed duration.
    pub min: Duration,
    /// Maximum observed duration.
    pub max: Duration,
}

impl PerfBreakdown {
    fn from_samples(marker: String, mut samples: Vec<Duration>) -> Self {
        let call_count = samples.len() as u64;
        let mean = stats::calculate_mean(&samples);
        let median = stats::calculate_median(&mut samples);
        let p95 = stats::calculate_percentile(&mut samples.clone(), 0.95);
        let p99 = stats::calculate_percentile(&mut samples.clone(), 0.99);
        let min = *samples.iter().min().unwrap_or(&Duration::ZERO);
        let max = *samples.iter().max().unwrap_or(&Duration::ZERO);

        Self {
            marker,
            call_count,
            mean,
            median,
            p95,
            p99,
            min,
            max,
        }
    }
}

// Visitor that extracts `name` (str) and `duration_ns` (u64) from event fields.
struct PerfEventVisitor {
    name: Option<String>,
    duration_ns: Option<u64>,
}

impl Visit for PerfEventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "name" {
            self.name = Some(value.to_string());
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "duration_ns" {
            self.duration_ns = Some(value);
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

/// A `tracing_subscriber::Layer` that captures `whitenoise::perf` timing events.
///
/// Wrap this with a level filter (`LevelFilter::INFO`) and add it to your
/// subscriber stack. Only events whose target is exactly `"whitenoise::perf"`
/// are captured; everything else passes through unmodified.
#[derive(Clone, Default)]
pub struct PerfTracingLayer {
    samples: Arc<Mutex<Vec<PerfSample>>>,
}

impl PerfTracingLayer {
    /// Create a new layer. Hold onto the returned value so you can call
    /// [`PerfTracingLayer::drain`] after benchmark loops.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drain all accumulated samples and return per-marker breakdowns, sorted
    /// by call count descending (hottest markers first).
    pub fn drain(&self) -> Vec<PerfBreakdown> {
        let mut guard = self.samples.lock().expect("perf layer mutex poisoned");
        let taken: Vec<PerfSample> = std::mem::take(&mut *guard);
        drop(guard);

        // Group by marker name
        let mut by_name: HashMap<String, Vec<Duration>> = HashMap::new();
        for sample in taken {
            by_name
                .entry(sample.name)
                .or_default()
                .push(sample.duration);
        }

        let mut breakdowns: Vec<PerfBreakdown> = by_name
            .into_iter()
            .map(|(name, durations)| PerfBreakdown::from_samples(name, durations))
            .collect();

        // Hottest markers (most calls) first
        breakdowns.sort_by(|a, b| b.call_count.cmp(&a.call_count));
        breakdowns
    }

    /// Clear all accumulated samples without computing statistics.
    /// Call this between benchmark warmup and the actual timed loop.
    pub fn clear(&self) {
        self.samples
            .lock()
            .expect("perf layer mutex poisoned")
            .clear();
    }
}

impl<S> Layer<S> for PerfTracingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Only process events from the perf target
        if event.metadata().target() != PERF_TARGET {
            return;
        }

        let mut visitor = PerfEventVisitor {
            name: None,
            duration_ns: None,
        };
        event.record(&mut visitor);

        if let (Some(name), Some(ns)) = (visitor.name, visitor.duration_ns) {
            self.samples
                .lock()
                .expect("perf layer mutex poisoned")
                .push(PerfSample {
                    name,
                    duration: Duration::from_nanos(ns),
                });
        }
    }
}
