use crate::integration_tests::benchmarks::scenarios::{
    IdentityCreationBenchmark, LoginPerformanceBenchmark, MessageAggregationBenchmark,
    MessagingPerformanceBenchmark, UserDiscoveryBenchmark,
};
use crate::integration_tests::benchmarks::{BenchmarkResult, BenchmarkScenario};
use crate::{Whitenoise, WhitenoiseError};
use std::time::{Duration, Instant};

/// Macro to register benchmarks in a single place.
/// Add a new benchmark by adding one line with: "cli-name" => Constructor expression
macro_rules! benchmark_registry {
    ($($name:literal => $scenario:expr),* $(,)?) => {
        /// Get all registered benchmark names (kebab-case)
        fn get_all_benchmark_names() -> Vec<&'static str> {
            vec![$($name),*]
        }

        /// Parse scenario name and instantiate the corresponding benchmark
        fn parse_and_instantiate(name: &str) -> Result<Box<dyn BenchmarkScenario + Send>, String> {
            match name.to_lowercase().as_str() {
                $(
                    $name => Ok(Box::new($scenario)),
                )*
                _ => {
                    let available = get_all_benchmark_names().join("\n  - ");
                    Err(format!(
                        "Unknown scenario '{}'. Available scenarios:\n  - {}",
                        name, available
                    ))
                }
            }
        }

        /// Run all registered benchmarks
        async fn run_all_registered(
            whitenoise: &'static Whitenoise,
            results: &mut Vec<BenchmarkResult>,
            first_error: &mut Option<WhitenoiseError>,
        ) {
            $(
                match $scenario.run_benchmark(whitenoise).await {
                    Ok(result) => results.push(result),
                    Err(e) => {
                        tracing::error!("Benchmark '{}' failed: {}", $name, e);
                        if first_error.is_none() {
                            *first_error = Some(e);
                        }
                    }
                }
            )*
        }
    };
}

// ============================================================================
// BENCHMARK REGISTRY - Add new benchmarks here (one line each)
// ============================================================================
benchmark_registry! {
    "identity-creation" => IdentityCreationBenchmark::default(),
    "login-performance" => LoginPerformanceBenchmark::default(),
    "messaging-performance" => MessagingPerformanceBenchmark::default(),
    "message-aggregation" => MessageAggregationBenchmark::default(),
    "user-discovery-blocking" => UserDiscoveryBenchmark::with_blocking_mode(),
    "user-discovery-background" => UserDiscoveryBenchmark::with_background_mode(),
}
// ============================================================================

pub struct BenchmarkRegistry;

impl BenchmarkRegistry {
    /// Run a single benchmark scenario by name
    pub async fn run_scenario(
        scenario_name: &str,
        whitenoise: &'static Whitenoise,
    ) -> Result<(), WhitenoiseError> {
        let overall_start = Instant::now();

        // Parse and instantiate the scenario
        let mut scenario =
            parse_and_instantiate(scenario_name).map_err(WhitenoiseError::InvalidInput)?;

        tracing::info!("=== Running Benchmark: {} ===", scenario.name());

        // Run the benchmark
        let result = scenario.run_benchmark(whitenoise).await?;

        // Print summary for this single benchmark
        Self::print_summary(&[result], overall_start.elapsed()).await;

        tracing::info!("=== Benchmark Completed Successfully ===");

        Ok(())
    }

    pub async fn run_all_benchmarks(
        whitenoise: &'static Whitenoise,
    ) -> Result<(), WhitenoiseError> {
        let overall_start = Instant::now();
        let mut results = Vec::new();
        let mut first_error = None;

        tracing::info!("=== Running Performance Benchmarks ===");

        // Run all registered benchmarks
        run_all_registered(whitenoise, &mut results, &mut first_error).await;

        Self::print_summary(&results, overall_start.elapsed()).await;

        // Return the first error encountered, if any
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn print_summary(results: &[BenchmarkResult], overall_duration: Duration) {
        tokio::time::sleep(Duration::from_millis(500)).await; // Wait for logs to flush

        if results.is_empty() {
            return;
        }

        tracing::info!("=== Benchmark Results Summary ===");
        tracing::info!("");

        for result in results {
            tracing::info!("Benchmark: {}", result.name);
            tracing::info!("  Iterations:  {}", result.iterations);
            tracing::info!("  Total Time:  {:?}", result.total_duration);
            tracing::info!("");
            tracing::info!("  Statistics:");
            tracing::info!("    Mean:      {:?}", result.mean);
            tracing::info!("    Median:    {:?}", result.median);
            tracing::info!("    Std Dev:   {:?}", result.std_dev);
            tracing::info!("    Min:       {:?}", result.min);
            tracing::info!("    Max:       {:?}", result.max);
            tracing::info!("    P95:       {:?}", result.p95);
            tracing::info!("    P99:       {:?}", result.p99);
            tracing::info!("");
            tracing::info!("  Throughput:  {:.2} ops/sec", result.throughput);
            tracing::info!("");
            tracing::info!("---");
        }

        tracing::info!("");
        tracing::info!("Total Benchmarks: {}", results.len());
        tracing::info!("Overall Duration: {:?}", overall_duration);

        // Give async logging time to flush before program exits
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_benchmark_registry_exists() {
        // Simple test to verify the registry struct exists
        let _registry = BenchmarkRegistry;
    }

    #[test]
    fn test_parse_valid_scenario_names() {
        // Test all valid scenario names can be parsed and instantiated
        assert!(parse_and_instantiate("identity-creation").is_ok());
        assert!(parse_and_instantiate("login-performance").is_ok());
        assert!(parse_and_instantiate("messaging-performance").is_ok());
        assert!(parse_and_instantiate("message-aggregation").is_ok());
        assert!(parse_and_instantiate("user-discovery-blocking").is_ok());
        assert!(parse_and_instantiate("user-discovery-background").is_ok());
    }

    #[test]
    fn test_parse_case_insensitive() {
        // Test case insensitivity
        assert!(parse_and_instantiate("IDENTITY-CREATION").is_ok());
        assert!(parse_and_instantiate("LOGIN-PERFORMANCE").is_ok());
        assert!(parse_and_instantiate("MESSAGING-PERFORMANCE").is_ok());
        assert!(parse_and_instantiate("Message-Aggregation").is_ok());
        assert!(parse_and_instantiate("USER-DISCOVERY-BLOCKING").is_ok());
    }

    #[test]
    fn test_parse_invalid_scenario_name() {
        // Test invalid scenario name
        let result = parse_and_instantiate("invalid-scenario");
        assert!(result.is_err());
        if let Err(error_msg) = result {
            assert!(error_msg.contains("Unknown scenario 'invalid-scenario'"));
            assert!(error_msg.contains("Available scenarios:"));
            assert!(error_msg.contains("identity-creation"));
        }
    }

    #[test]
    fn test_get_all_benchmark_names() {
        // Test that all benchmark names are returned
        let names = get_all_benchmark_names();
        assert_eq!(names.len(), 6);
        assert!(names.contains(&"identity-creation"));
        assert!(names.contains(&"login-performance"));
        assert!(names.contains(&"messaging-performance"));
        assert!(names.contains(&"message-aggregation"));
        assert!(names.contains(&"user-discovery-blocking"));
        assert!(names.contains(&"user-discovery-background"));
    }

    #[test]
    fn test_instantiated_scenarios_have_names() {
        // Test that instantiation returns valid trait objects with names
        for name in get_all_benchmark_names() {
            let scenario = parse_and_instantiate(name).unwrap();
            // Verify the scenario has a non-empty name
            assert!(!scenario.name().is_empty());
        }
    }
}
