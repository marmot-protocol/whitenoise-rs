use std::time::Duration;

use serde::Serialize;

use super::benchmark_config::BenchmarkConfig;
use crate::integration_tests::benchmarks::perf_layer::{IterationDetail, PerfBreakdown};
use crate::integration_tests::benchmarks::stats;

/// Results from running a benchmark scenario
#[derive(Debug, Clone, Serialize)]
pub struct BenchmarkResult {
    pub name: String,
    pub iterations: u32,
    #[serde(
        rename = "total_duration_ns",
        serialize_with = "crate::integration_tests::benchmarks::serde_duration::as_nanos"
    )]
    pub total_duration: Duration,
    #[serde(
        rename = "mean_ns",
        serialize_with = "crate::integration_tests::benchmarks::serde_duration::as_nanos"
    )]
    pub mean: Duration,
    #[serde(
        rename = "median_ns",
        serialize_with = "crate::integration_tests::benchmarks::serde_duration::as_nanos"
    )]
    pub median: Duration,
    #[serde(
        rename = "std_dev_ns",
        serialize_with = "crate::integration_tests::benchmarks::serde_duration::as_nanos"
    )]
    pub std_dev: Duration,
    #[serde(
        rename = "min_ns",
        serialize_with = "crate::integration_tests::benchmarks::serde_duration::as_nanos"
    )]
    pub min: Duration,
    #[serde(
        rename = "max_ns",
        serialize_with = "crate::integration_tests::benchmarks::serde_duration::as_nanos"
    )]
    pub max: Duration,
    #[serde(
        rename = "p95_ns",
        serialize_with = "crate::integration_tests::benchmarks::serde_duration::as_nanos"
    )]
    pub p95: Duration,
    #[serde(
        rename = "p99_ns",
        serialize_with = "crate::integration_tests::benchmarks::serde_duration::as_nanos"
    )]
    pub p99: Duration,
    #[serde(rename = "throughput_ops_sec")]
    pub throughput: f64, // operations per second
    /// Per-marker performance breakdown captured via `perf_span!`.
    /// `None` when no `PerfTracingLayer` was active during the benchmark.
    pub perf_breakdown: Option<Vec<PerfBreakdown>>,
    /// Per-iteration span detail. `Some` only when `--detailed` is active.
    /// Each entry holds all spans captured within a single iteration along
    /// with that iteration's wall-clock time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_iteration: Option<Vec<IterationDetail>>,
}

impl BenchmarkResult {
    /// Convenience constructor for benchmarks with custom orchestration.
    /// Calculates all statistics from timing data (Level 2 abstraction).
    ///
    /// Use this when you need custom warmup/benchmark loops but still want
    /// DRY statistics calculation.
    ///
    /// # Arguments
    /// * `name` - Benchmark name
    /// * `config` - Benchmark configuration (for iterations count)
    /// * `timings` - Vector of timing measurements from benchmark iterations
    /// * `total_duration` - Total elapsed time including cooldown between iterations
    /// * `perf_breakdown` - Optional per-marker breakdown from `PerfTracingLayer::drain()`
    pub fn from_timings(
        name: String,
        config: &BenchmarkConfig,
        timings: Vec<Duration>,
        total_duration: Duration,
        perf_breakdown: Option<Vec<PerfBreakdown>>,
        per_iteration: Option<Vec<IterationDetail>>,
    ) -> Self {
        let mean = stats::calculate_mean(&timings);
        let mut timings_for_median = timings.clone();
        let median = stats::calculate_median(&mut timings_for_median);
        let std_dev = stats::calculate_std_dev(&timings, mean);
        let min = *timings.iter().min().unwrap_or(&Duration::ZERO);
        let max = *timings.iter().max().unwrap_or(&Duration::ZERO);
        let mut timings_for_p95 = timings.clone();
        let p95 = stats::calculate_percentile(&mut timings_for_p95, 0.95);
        let mut timings_for_p99 = timings;
        let p99 = stats::calculate_percentile(&mut timings_for_p99, 0.99);
        let throughput = stats::calculate_throughput(config.iterations, total_duration);

        Self {
            name,
            iterations: config.iterations,
            total_duration,
            mean,
            median,
            std_dev,
            min,
            max,
            p95,
            p99,
            throughput,
            perf_breakdown,
            per_iteration,
        }
    }
}
