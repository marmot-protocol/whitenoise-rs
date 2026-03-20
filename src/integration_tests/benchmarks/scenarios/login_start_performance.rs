use std::time::Duration;

use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::test_cases::LoginStartBenchmark;
use crate::integration_tests::benchmarks::{BenchmarkConfig, BenchmarkScenario, BenchmarkTestCase};
use crate::integration_tests::core::ScenarioContext;
use crate::integration_tests::core::test_clients::{
    create_test_client, publish_relay_lists, publish_test_metadata,
};

/// Benchmark scenario for measuring `login_start` performance (happy path).
///
/// This scenario tests the performance of the multi-step login API when all
/// three relay lists (NIP-65, Inbox, KeyPackage) are already published to the
/// network. In this case `login_start` discovers the relay lists, activates
/// the account, and returns `LoginStatus::Complete` in a single call.
///
/// The setup phase pre-generates unique keypairs and publishes relay lists +
/// metadata to the local test relays. Each benchmark iteration then calls
/// `login_start` with one of these keypairs and measures the elapsed time.
pub struct LoginStartPerformanceBenchmark {
    test_case: Option<LoginStartBenchmark>,
}

impl LoginStartPerformanceBenchmark {
    pub fn new() -> Self {
        Self { test_case: None }
    }
}

impl Default for LoginStartPerformanceBenchmark {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BenchmarkScenario for LoginStartPerformanceBenchmark {
    fn name(&self) -> &str {
        "Login Start Performance (Happy Path)"
    }

    fn config(&self) -> BenchmarkConfig {
        BenchmarkConfig {
            iterations: 10,
            warmup_iterations: 1,
            cooldown_between_iterations: Duration::from_millis(500),
        }
    }

    async fn setup(&mut self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let config = self.config();
        let total_iterations = (config.iterations + config.warmup_iterations) as usize;
        let relay_urls: Vec<String> = context.dev_relays.iter().map(|s| s.to_string()).collect();

        tracing::info!(
            "Setting up login-start benchmark: {} iterations",
            total_iterations
        );

        let mut prepared_keys = Vec::with_capacity(total_iterations);

        for i in 0..total_iterations {
            let keys = Keys::generate();
            let test_client = create_test_client(&context.dev_relays, keys.clone()).await?;

            publish_test_metadata(
                &test_client,
                &format!("Login Start User {}", i),
                "Benchmark user for login-start happy path",
            )
            .await?;

            // Publish all 3 relay lists so login_start finds them
            publish_relay_lists(&test_client, relay_urls.clone()).await?;

            test_client.disconnect().await;
            prepared_keys.push(keys);

            if (i + 1) % 5 == 0 || i + 1 == total_iterations {
                tracing::info!("Prepared {}/{} test accounts", i + 1, total_iterations);
            }
        }

        // Brief pause to let relays index the published events
        tokio::time::sleep(Duration::from_secs(2)).await;

        tracing::info!(
            "Setup complete: {} accounts with relay lists published",
            total_iterations
        );

        self.test_case = Some(LoginStartBenchmark::new(prepared_keys));

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
