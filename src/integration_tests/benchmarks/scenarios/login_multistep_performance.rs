use std::time::Duration;

use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::test_cases::LoginMultistepBenchmark;
use crate::integration_tests::benchmarks::{BenchmarkConfig, BenchmarkScenario, BenchmarkTestCase};
use crate::integration_tests::core::ScenarioContext;

/// Benchmark scenario for measuring the multi-step login flow:
/// `login_start` → `login_publish_default_relays`.
///
/// This scenario tests the "new user" login path where no relay lists exist on
/// the network. `login_start` discovers nothing and returns `NeedsRelayLists`,
/// then `login_publish_default_relays` publishes default relay lists and
/// activates the account.
///
/// Each iteration uses a fresh keypair with nothing published to the network.
/// No setup is needed beyond generating the keypairs.
pub struct LoginMultistepPerformanceBenchmark {
    test_case: Option<LoginMultistepBenchmark>,
}

impl LoginMultistepPerformanceBenchmark {
    pub fn new() -> Self {
        Self { test_case: None }
    }
}

impl Default for LoginMultistepPerformanceBenchmark {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BenchmarkScenario for LoginMultistepPerformanceBenchmark {
    fn name(&self) -> &str {
        "Login Multistep Performance (Publish Defaults)"
    }

    fn config(&self) -> BenchmarkConfig {
        BenchmarkConfig {
            iterations: 10,
            warmup_iterations: 1,
            cooldown_between_iterations: Duration::from_millis(500),
        }
    }

    async fn setup(&mut self, _context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let config = self.config();
        let total_iterations = (config.iterations + config.warmup_iterations) as usize;

        tracing::info!(
            "Setting up login-multistep benchmark: {} iterations (no relay lists to publish)",
            total_iterations
        );

        // Generate keypairs only — no relay lists published so login_start
        // returns NeedsRelayLists for each.
        let prepared_keys: Vec<Keys> = (0..total_iterations).map(|_| Keys::generate()).collect();

        tracing::info!(
            "Setup complete: {} fresh keypairs generated",
            total_iterations
        );

        self.test_case = Some(LoginMultistepBenchmark::new(prepared_keys));

        Ok(())
    }

    async fn single_iteration(
        &self,
        context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        self.test_case
            .as_ref()
            .expect("test_case must be initialized in setup()")
            .run_iteration(context)
            .await
    }
}
