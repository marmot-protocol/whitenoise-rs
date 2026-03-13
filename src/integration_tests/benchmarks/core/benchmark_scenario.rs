use std::time::{Duration, Instant};

use async_trait::async_trait;
use indicatif::{ProgressBar, ProgressStyle};

use super::benchmark_config::BenchmarkConfig;
use super::benchmark_result::BenchmarkResult;
use crate::integration_tests::benchmarks::PERF_LAYER;
use crate::integration_tests::core::ScenarioContext;
use crate::{Whitenoise, WhitenoiseError};

/// Trait for benchmark scenarios using the template method pattern.
///
/// Implement `setup()` and `single_iteration()` to create a benchmark.
/// The default `run_benchmark()` implementation handles warmup, timing, and statistics.
#[async_trait]
pub trait BenchmarkScenario {
    /// Name of the benchmark scenario
    fn name(&self) -> &str;

    /// Configuration for this benchmark
    fn config(&self) -> BenchmarkConfig {
        BenchmarkConfig::default()
    }

    /// Setup phase - create accounts, groups, and any test data needed.
    /// This is NOT timed and runs once before warmup iterations.
    async fn setup(&mut self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError>;

    /// Run a single iteration of the benchmark operation.
    /// This should perform the operation being benchmarked and return its duration.
    async fn single_iteration(
        &self,
        context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError>;

    /// Run the benchmark with standard warmup, timing, and statistics collection.
    ///
    /// Override this method only if you need custom orchestration (e.g. multi-phase
    /// timing or a custom iteration loop). If you do override it, you are responsible
    /// for calling `PERF_LAYER.get().map(|l| l.clear())` before the timed phase and
    /// `PERF_LAYER.get().map(|l| l.drain())` at the end to collect perf breakdowns —
    /// the default implementation does both; overrides do not inherit that logic.
    async fn run_benchmark(
        &mut self,
        whitenoise: &'static Whitenoise,
    ) -> Result<BenchmarkResult, WhitenoiseError> {
        let config = self.config();
        let mut context = ScenarioContext::new(whitenoise);

        tracing::info!("Setting up benchmark: {}", self.name());
        self.setup(&mut context).await?;

        // Warmup phase (skip if warmup_iterations == 0)
        if config.warmup_iterations > 0 {
            tracing::info!("Running {} warmup iterations...", config.warmup_iterations);

            let pb = ProgressBar::new(config.warmup_iterations as u64);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("{msg} [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
                    .unwrap()
                    .progress_chars("=>-"),
            );
            pb.set_message("Warmup");

            for _ in 0..config.warmup_iterations {
                self.single_iteration(&mut context).await?;
                pb.inc(1);
                tokio::time::sleep(config.cooldown_between_iterations).await;
            }

            pb.finish_with_message("Warmup complete");

            // Reset counter for actual benchmark
            context.tests_count = 0;
        }

        // Clear perf samples before the timed phase.
        // This covers both cases: warmup ran (clears warmup spans) and
        // warmup was skipped (clears setup spans).
        if let Some(layer) = PERF_LAYER.get() {
            layer.clear();
        }

        // Benchmark phase
        tracing::info!("Running {} benchmark iterations...", config.iterations);
        let mut timings = Vec::with_capacity(config.iterations as usize);
        let overall_start = Instant::now();

        let pb = ProgressBar::new(config.iterations as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{msg} [{bar:40.green/blue}] {pos}/{len} ({percent}%) - ETA: {eta}")
                .unwrap()
                .progress_chars("##-"),
        );
        pb.set_message("Benchmarking");

        for _ in 0..config.iterations {
            let duration = self.single_iteration(&mut context).await?;
            timings.push(duration);
            pb.inc(1);
            tokio::time::sleep(config.cooldown_between_iterations).await;
        }

        pb.finish_with_message("Benchmark complete");
        let total_duration = overall_start.elapsed();

        // Drain perf layer if available
        let perf_breakdown = PERF_LAYER.get().map(|layer| layer.drain());

        // Calculate and return results
        Ok(BenchmarkResult::from_timings(
            self.name().to_string(),
            &config,
            timings,
            total_duration,
            perf_breakdown,
        ))
    }
}
