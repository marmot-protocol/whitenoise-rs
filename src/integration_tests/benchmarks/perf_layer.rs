//! PerfTracingLayer — a `tracing_subscriber` layer that captures performance
//! events from two sources and records their durations:
//!
//! 1. **`whitenoise::perf`** — manual `perf_span!` markers emitted by `PerfGuard`
//!    on drop, carrying `name` (str) and `duration_ns` (u64) fields.
//! 2. **`sqlx::query`** — automatic query timing emitted by sqlx's `QueryLogger`
//!    on drop, carrying `summary` (str) and `elapsed_secs` (f64) fields.
//!
//! Both event shapes are normalised into [`PerfSample`] structs and accumulated
//! in a shared `Vec`. After a benchmark loop finishes, call
//! [`PerfTracingLayer::drain`] to consume the accumulated samples and compute
//! per-marker [`PerfBreakdown`] statistics.

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
const SQLX_TARGET: &str = "sqlx::query";

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
    /// Exact total duration (sum of all samples).
    pub total_duration: Duration,
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
        let total_duration: Duration = samples.iter().sum();
        let mean = stats::calculate_mean(&samples);
        // calculate_median sorts samples in place; p95/p99 reuse the sorted slice.
        let median = stats::calculate_median(&mut samples);
        let p95 = stats::calculate_percentile(&mut samples, 0.95);
        let p99 = stats::calculate_percentile(&mut samples, 0.99);
        let min = *samples.iter().min().unwrap_or(&Duration::ZERO);
        let max = *samples.iter().max().unwrap_or(&Duration::ZERO);

        Self {
            marker,
            call_count,
            total_duration,
            mean,
            median,
            p95,
            p99,
            min,
            max,
        }
    }
}

// Visitor that extracts fields from both event shapes:
// - perf events:  `name` (str) + `duration_ns` (u64)
// - sqlx events:  `summary` (str) + `elapsed_secs` (f64)
struct PerfEventVisitor {
    name: Option<String>,
    duration_ns: Option<u64>,
    // sqlx fields
    summary: Option<String>,
    elapsed_secs: Option<f64>,
}

impl Visit for PerfEventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "name" => self.name = Some(value.to_string()),
            "summary" => self.summary = Some(value.to_string()),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "duration_ns" {
            self.duration_ns = Some(value);
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if field.name() == "elapsed_secs" {
            self.elapsed_secs = Some(value);
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

        // Hottest markers (most total time) first
        breakdowns.sort_by(|a, b| b.total_duration.cmp(&a.total_duration));
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
        let target = event.metadata().target();

        if target != PERF_TARGET && target != SQLX_TARGET {
            return;
        }

        let mut visitor = PerfEventVisitor {
            name: None,
            duration_ns: None,
            summary: None,
            elapsed_secs: None,
        };
        event.record(&mut visitor);

        let sample = if target == PERF_TARGET {
            // perf_span! events: name + duration_ns
            match (visitor.name, visitor.duration_ns) {
                (Some(name), Some(ns)) => Some(PerfSample {
                    name,
                    duration: Duration::from_nanos(ns),
                }),
                _ => None,
            }
        } else if target == SQLX_TARGET {
            // sqlx query events: summary + elapsed_secs
            match (visitor.summary, visitor.elapsed_secs) {
                (Some(summary), Some(secs)) => Some(PerfSample {
                    name: format!("sqlx::{summary}"),
                    duration: Duration::from_secs_f64(secs),
                }),
                _ => None,
            }
        } else {
            None
        };

        if let Some(sample) = sample {
            self.samples
                .lock()
                .expect("perf layer mutex poisoned")
                .push(sample);
        }
    }
}
