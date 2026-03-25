use std::time::Duration;

use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::core::json_output::{CiTier, ScenarioThresholds};
use crate::integration_tests::benchmarks::test_cases::SendMessageBenchmark;
use crate::integration_tests::benchmarks::{BenchmarkConfig, BenchmarkScenario, BenchmarkTestCase};
use crate::integration_tests::core::{ScenarioContext, TestCase};
use crate::integration_tests::test_cases::shared::{CreateAccountsTestCase, CreateGroupTestCase};

pub struct MessagingPerformanceBenchmark {
    send_message_benchmark: SendMessageBenchmark,
}

impl MessagingPerformanceBenchmark {
    pub fn new(send_message_benchmark: SendMessageBenchmark) -> Self {
        Self {
            send_message_benchmark,
        }
    }
}

impl Default for MessagingPerformanceBenchmark {
    fn default() -> Self {
        Self::new(SendMessageBenchmark::new("alice", "benchmark_group"))
    }
}

#[async_trait]
impl BenchmarkScenario for MessagingPerformanceBenchmark {
    fn name(&self) -> &str {
        "Message Sending Performance"
    }

    fn config(&self) -> BenchmarkConfig {
        BenchmarkConfig {
            iterations: 100,
            warmup_iterations: 10,
            cooldown_between_iterations: Duration::from_millis(50),
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

    async fn setup(&mut self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        // Setup phase (not timed): Create accounts and group
        CreateAccountsTestCase::with_names(vec!["alice", "bob"])
            .run(context)
            .await?;

        CreateGroupTestCase::basic()
            .with_name("benchmark_group")
            .with_members("alice", vec!["bob"])
            .run(context)
            .await?;

        // Give time for group sync
        tokio::time::sleep(Duration::from_secs(2)).await;

        Ok(())
    }

    async fn single_iteration(
        &self,
        context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        self.send_message_benchmark.run_iteration(context).await
    }
}
