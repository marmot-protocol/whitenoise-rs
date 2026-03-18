use std::time::Duration;

use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::core::json_output::ScenarioThresholds;
use crate::integration_tests::benchmarks::test_cases::CreateGroupBenchmark;
use crate::integration_tests::benchmarks::{BenchmarkConfig, BenchmarkScenario, BenchmarkTestCase};
use crate::integration_tests::core::ScenarioContext;

/// Default number of members per group during benchmark iterations.
const DEFAULT_MEMBERS_PER_GROUP: usize = 2;

/// Benchmark scenario for measuring `create_group` performance.
///
/// This scenario benchmarks the MLS group creation flow, which includes:
/// - Fetching key packages for each member from relays
/// - Creating the MLS group via MDK
/// - Publishing welcome messages to each member's inbox relays
/// - Creating group information and account-group records
/// - Refreshing account subscriptions
///
/// **Setup phase** (not timed):
/// - Creates a single creator account that persists across all iterations
/// - Creates N×members_per_group fresh member accounts (one set per iteration)
///   Each member account has key packages published to the test relays via
///   `create_identity()`, which handles relay lists and key package publishing.
///
/// **Benchmark iteration** (timed):
/// - Calls `create_group` with the creator and a fresh set of members
///
/// MLS key packages are single-use, so each iteration requires unique members.
pub struct GroupCreationBenchmark {
    members_per_group: usize,
    test_case: Option<CreateGroupBenchmark>,
}

impl GroupCreationBenchmark {
    pub fn new(members_per_group: usize) -> Self {
        Self {
            members_per_group,
            test_case: None,
        }
    }
}

impl Default for GroupCreationBenchmark {
    fn default() -> Self {
        Self::new(DEFAULT_MEMBERS_PER_GROUP)
    }
}

#[async_trait]
impl BenchmarkScenario for GroupCreationBenchmark {
    fn name(&self) -> &str {
        "Group Creation Performance"
    }

    fn config(&self) -> BenchmarkConfig {
        BenchmarkConfig {
            // Group creation is network-heavy (key package fetches, welcome publishes);
            // keep iterations modest for reasonable benchmark duration.
            iterations: 10,
            warmup_iterations: 1,
            cooldown_between_iterations: Duration::from_millis(500),
        }
    }

    fn thresholds(&self) -> ScenarioThresholds {
        ScenarioThresholds {
            warn_pct: 10,
            regress_pct: 25,
            break_pct: 40,
            ci_tier: "relay",
        }
    }

    async fn setup(&mut self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let config = self.config();
        let total_iterations = (config.iterations + config.warmup_iterations) as usize;
        let total_members = total_iterations * self.members_per_group;

        tracing::info!(
            "Setting up group creation benchmark: {} iterations, {} members per group, {} total member accounts to create",
            total_iterations,
            self.members_per_group,
            total_members,
        );

        // Create the creator account (reused across all iterations)
        let creator = context.whitenoise.create_identity().await?;
        tracing::info!("Created creator account: {}", creator.pubkey.to_hex());
        context.add_account("creator", creator);

        // Create member accounts — each gets key packages published via create_identity()
        let mut member_sets: Vec<Vec<String>> = Vec::with_capacity(total_iterations);

        for i in 0..total_iterations {
            let mut members_for_iteration = Vec::with_capacity(self.members_per_group);

            for j in 0..self.members_per_group {
                let member_name = format!("member_{}_{}", i, j);
                let member = context.whitenoise.create_identity().await?;
                context.add_account(&member_name, member);
                members_for_iteration.push(member_name);
            }

            member_sets.push(members_for_iteration);

            if (i + 1) % 5 == 0 || i + 1 == total_iterations {
                tracing::info!(
                    "Prepared member accounts for {}/{} iterations",
                    i + 1,
                    total_iterations
                );
            }
        }

        // Brief pause to let relays index published key packages
        tokio::time::sleep(Duration::from_secs(2)).await;

        tracing::info!(
            "Setup complete: 1 creator + {} member accounts ({} groups × {} members)",
            total_members,
            total_iterations,
            self.members_per_group,
        );

        self.test_case = Some(CreateGroupBenchmark::new("creator", member_sets));

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
