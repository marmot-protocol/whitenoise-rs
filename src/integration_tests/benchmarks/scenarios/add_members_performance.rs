use std::time::Duration;

use async_trait::async_trait;
use mdk_core::prelude::*;
use nostr_sdk::prelude::*;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::test_cases::AddMembersBenchmark;
use crate::integration_tests::benchmarks::{BenchmarkConfig, BenchmarkScenario, BenchmarkTestCase};
use crate::integration_tests::core::ScenarioContext;

/// Number of members added per iteration.
const MEMBERS_PER_ITERATION: usize = 1;

/// Benchmark scenario for measuring `add_members_to_group` performance.
///
/// This benchmarks the blocking publish path: `add_members_to_group` calls
/// `publish_and_merge_commit`, which synchronously waits for relay acceptance
/// before merging the MLS commit. This is the hot path where quorum-based
/// publishing can reduce latency.
///
/// **Setup phase** (not timed):
/// - Creates an admin account
/// - Creates a group with one initial member
/// - Creates N fresh member accounts (one set per iteration), each with
///   key packages published via `create_identity()`
///
/// **Benchmark iteration** (timed):
/// - Calls `add_members_to_group` with a fresh set of members
#[derive(Default)]
pub struct AddMembersPerformanceBenchmark {
    test_case: Option<AddMembersBenchmark>,
}

#[async_trait]
impl BenchmarkScenario for AddMembersPerformanceBenchmark {
    fn name(&self) -> &str {
        "Add Members Performance"
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
        let total_members = total_iterations * MEMBERS_PER_ITERATION;

        tracing::info!(
            "Setting up add-members benchmark: {} iterations, {} members per iteration",
            total_iterations,
            MEMBERS_PER_ITERATION,
        );

        // Create admin account
        let admin = context.whitenoise.create_identity().await?;
        tracing::info!("Created admin account: {}", admin.pubkey.to_hex());
        context.add_account("admin", admin.clone());

        // Create an initial member so the group isn't empty
        let initial_member = context.whitenoise.create_identity().await?;
        tracing::info!(
            "Created initial group member: {}",
            initial_member.pubkey.to_hex()
        );

        // Brief pause to let relays index key packages
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Create the group
        let group_config = NostrGroupConfigData::new(
            "Add Members Benchmark Group".to_string(),
            "Benchmark group for add_members_to_group performance".to_string(),
            None,
            None,
            None,
            context.test_relays(),
            vec![admin.pubkey],
            None, // disappearing_message_duration_secs
        );

        let group = context
            .whitenoise
            .create_group(&admin, vec![initial_member.pubkey], group_config, None)
            .await?;
        tracing::info!("Created benchmark group");
        context.add_group("benchmark_group", group);

        // Create member accounts for each iteration
        let mut member_sets: Vec<Vec<String>> = Vec::with_capacity(total_iterations);

        for i in 0..total_iterations {
            let mut members_for_iteration = Vec::with_capacity(MEMBERS_PER_ITERATION);

            for j in 0..MEMBERS_PER_ITERATION {
                let member_name = format!("add_member_{}_{}", i, j);
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
            "Setup complete: 1 admin + 1 initial member + {} new members across {} iterations",
            total_members,
            total_iterations,
        );

        self.test_case = Some(AddMembersBenchmark::new(
            "admin",
            "benchmark_group",
            member_sets,
        ));

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
