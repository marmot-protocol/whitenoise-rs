use std::time::{Duration, Instant};

use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::BenchmarkTestCase;
use crate::integration_tests::core::ScenarioContext;

/// Benchmark test case for measuring login performance
///
/// This benchmark measures the time it takes to log in with an existing Nostr
/// identity that has metadata, relay lists, and a contact list already published
/// to relays. This simulates the real-world scenario of a user logging into
/// White Noise with an established Nostr account.
///
/// The login flow includes:
/// - Parsing the private key and creating a local account
/// - Fetching existing relay lists from the network (NIP-65, Inbox, KeyPackage)
/// - Connecting to discovered relays
/// - Refreshing global subscriptions for the user
/// - Setting up account-specific subscriptions (giftwrap, MLS, groups)
/// - Fetching or publishing MLS key packages
///
/// Each iteration uses a unique pre-generated keypair whose metadata, relay lists,
/// and contact list have been pre-published to the test relays during setup.
pub struct LoginBenchmark {
    /// Pre-generated keypairs, one per iteration. Each has metadata, relay lists,
    /// and a contact list already published to the test relays.
    prepared_keys: Vec<Keys>,
}

impl LoginBenchmark {
    pub fn new(prepared_keys: Vec<Keys>) -> Self {
        Self { prepared_keys }
    }
}

#[async_trait]
impl BenchmarkTestCase for LoginBenchmark {
    async fn run_iteration(
        &self,
        context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        let iteration = context.tests_count as usize;
        if iteration >= self.prepared_keys.len() {
            return Err(WhitenoiseError::Other(anyhow::anyhow!(
                "Login benchmark iteration {} exceeds prepared keys count ({})",
                iteration,
                self.prepared_keys.len()
            )));
        }
        let keys = &self.prepared_keys[iteration];

        // Time the login operation
        let start = Instant::now();
        let account = context
            .whitenoise
            .login(keys.secret_key().to_secret_hex())
            .await?;
        let duration = start.elapsed();

        // Log out so the next iteration starts clean
        context.whitenoise.logout(&account.pubkey).await?;

        context.tests_count += 1;

        Ok(duration)
    }
}
