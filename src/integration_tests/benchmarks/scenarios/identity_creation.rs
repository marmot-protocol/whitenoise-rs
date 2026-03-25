use std::time::Duration;

use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::core::json_output::{CiTier, ScenarioThresholds};
use crate::integration_tests::benchmarks::test_cases::CreateIdentityBenchmark;
use crate::integration_tests::benchmarks::{BenchmarkConfig, BenchmarkScenario, BenchmarkTestCase};
use crate::integration_tests::core::ScenarioContext;

/// Benchmark scenario for measuring identity creation performance
///
/// This scenario tests the performance of the `create_identity()` method, which is
/// the primary entry point for creating new users in Whitenoise. The operation includes:
///
/// - Generating a new keypair
/// - Creating a user record in the database
/// - Setting up default relay lists (NIP-65, Inbox, Key Package relays)
/// - Publishing relay lists to the network
/// - Generating and publishing metadata with a petname
/// - Publishing key packages for MLS group messaging
/// - Setting up initial subscriptions
///
/// This benchmark helps identify performance bottlenecks in the account creation
/// flow and ensures the onboarding experience remains fast as the codebase evolves.
pub struct IdentityCreationBenchmark {
    test_case: CreateIdentityBenchmark,
}

impl IdentityCreationBenchmark {
    pub fn new() -> Self {
        Self {
            test_case: CreateIdentityBenchmark::new(),
        }
    }
}

impl Default for IdentityCreationBenchmark {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BenchmarkScenario for IdentityCreationBenchmark {
    fn name(&self) -> &str {
        "Identity Creation Performance"
    }

    fn config(&self) -> BenchmarkConfig {
        BenchmarkConfig {
            iterations: 25,
            warmup_iterations: 0,
            cooldown_between_iterations: Duration::from_millis(100),
        }
    }

    fn thresholds(&self) -> ScenarioThresholds {
        ScenarioThresholds {
            warn_pct: 5,
            regress_pct: 10,
            break_pct: 20,
            ci_tier: CiTier::Stable,
        }
    }

    async fn setup(&mut self, _context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        // No setup needed - each iteration creates a fresh identity
        tracing::info!("Ready to benchmark identity creation");
        Ok(())
    }

    async fn single_iteration(
        &self,
        context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        self.test_case.run_iteration(context).await
    }
}
