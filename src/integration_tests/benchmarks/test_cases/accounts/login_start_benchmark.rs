use std::time::{Duration, Instant};

use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::BenchmarkTestCase;
use crate::integration_tests::core::ScenarioContext;
use crate::whitenoise::accounts::LoginStatus;

/// Benchmark test case for measuring `login_start` performance (happy path).
///
/// Each iteration uses a pre-generated keypair whose metadata, relay lists
/// (NIP-65, Inbox, KeyPackage), are already published to the test relays.
/// Because all three relay lists exist on the network, `login_start` discovers
/// them and completes the login in a single call.
///
/// Only the `login_start` call is timed — account setup and relay list
/// publishing happen during the scenario's setup phase.
pub struct LoginStartBenchmark {
    /// Pre-generated keypairs, one per iteration. Each has relay lists
    /// already published to the test relays.
    prepared_keys: Vec<Keys>,
}

impl LoginStartBenchmark {
    pub fn new(prepared_keys: Vec<Keys>) -> Self {
        Self { prepared_keys }
    }
}

#[async_trait]
impl BenchmarkTestCase for LoginStartBenchmark {
    async fn run_iteration(
        &self,
        context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        let iteration = context.tests_count as usize;
        if iteration >= self.prepared_keys.len() {
            return Err(WhitenoiseError::Internal(format!(
                "Login start benchmark iteration {} exceeds prepared keys count ({})",
                iteration,
                self.prepared_keys.len()
            )));
        }
        let keys = &self.prepared_keys[iteration];

        // Time the login_start operation
        let start = Instant::now();
        let result = context
            .whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .map_err(|e| WhitenoiseError::Internal(format!("{}", e)))?;
        let duration = start.elapsed();

        if result.status != LoginStatus::Complete {
            return Err(WhitenoiseError::Internal(format!(
                "Expected LoginStatus::Complete but got {:?}",
                result.status
            )));
        }

        // Log out so the next iteration starts clean
        context.whitenoise.logout(&result.account.pubkey).await?;

        context.tests_count += 1;

        Ok(duration)
    }
}
