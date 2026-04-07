use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::BenchmarkTestCase;
use crate::integration_tests::core::ScenarioContext;
use crate::whitenoise::database::aggregated_messages::PaginationOptions;

/// Benchmark test case for measuring fetch_aggregated_messages_for_group performance
pub struct FetchAggregatedMessagesBenchmark {
    account_name: String,
    group_name: String,
}

impl FetchAggregatedMessagesBenchmark {
    pub fn new(account_name: &str, group_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
        }
    }
}

#[async_trait]
impl BenchmarkTestCase for FetchAggregatedMessagesBenchmark {
    async fn run_iteration(
        &self,
        context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;

        // Time only the fetch_aggregated_messages_for_group call
        let start = Instant::now();

        let messages = context
            .whitenoise
            .fetch_aggregated_messages_for_group(
                &account.pubkey,
                &group.mls_group_id,
                &PaginationOptions::default(),
                None,
            )
            .await?;

        let duration = start.elapsed();

        // Basic validation (minimal impact on timing)
        assert!(!messages.is_empty(), "Should have messages after setup");

        // Increment test count for next iteration
        context.tests_count += 1;

        Ok(duration)
    }
}
