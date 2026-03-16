use std::time::{Duration, Instant};

use async_trait::async_trait;
use mdk_core::prelude::*;
use nostr_sdk::prelude::*;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::BenchmarkTestCase;
use crate::integration_tests::core::ScenarioContext;

/// Benchmark test case for measuring `create_group` performance.
///
/// Each iteration uses a pre-created creator account and a fresh set of member
/// accounts (whose key packages are already published to the test relays).
/// MLS key packages are single-use, so each iteration needs unique members.
///
/// Only the `create_group` call is timed — account creation, key package
/// publishing, and relay list setup all happen during the scenario's setup phase.
pub struct CreateGroupBenchmark {
    /// Name of the creator account stored in ScenarioContext.
    creator_account: String,
    /// Pre-generated member accounts, one set per iteration.
    /// Each inner Vec contains the account names for that iteration's group members.
    member_sets: Vec<Vec<String>>,
}

impl CreateGroupBenchmark {
    pub fn new(creator_account: &str, member_sets: Vec<Vec<String>>) -> Self {
        Self {
            creator_account: creator_account.to_string(),
            member_sets,
        }
    }
}

#[async_trait]
impl BenchmarkTestCase for CreateGroupBenchmark {
    async fn run_iteration(
        &self,
        context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        let iteration = context.tests_count as usize;
        if iteration >= self.member_sets.len() {
            return Err(WhitenoiseError::Other(anyhow::anyhow!(
                "Create group benchmark iteration {} exceeds prepared member sets ({})",
                iteration,
                self.member_sets.len()
            )));
        }

        let creator = context.get_account(&self.creator_account)?;
        let member_names = &self.member_sets[iteration];
        let member_pubkeys: Vec<PublicKey> = member_names
            .iter()
            .map(|name| context.get_account(name).map(|acc| acc.pubkey))
            .collect::<Result<Vec<_>, _>>()?;
        let admin_pubkeys = vec![creator.pubkey];

        let config = NostrGroupConfigData::new(
            format!("Benchmark Group {}", iteration),
            "Benchmark group for create_group performance".to_string(),
            None,
            None,
            None,
            context.test_relays(),
            admin_pubkeys,
        );

        // Time only the create_group call
        let start = Instant::now();

        context
            .whitenoise
            .create_group(creator, member_pubkeys, config, None)
            .await?;

        let duration = start.elapsed();

        context.tests_count += 1;

        Ok(duration)
    }
}
