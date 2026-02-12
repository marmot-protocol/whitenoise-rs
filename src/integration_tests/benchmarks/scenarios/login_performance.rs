use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use nostr_sdk::prelude::*;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::test_cases::LoginBenchmark;
use crate::integration_tests::benchmarks::{BenchmarkConfig, BenchmarkScenario, BenchmarkTestCase};
use crate::integration_tests::core::ScenarioContext;
use crate::integration_tests::core::test_clients::{
    create_test_client, publish_follow_list, publish_relay_lists, publish_test_metadata,
};

/// Default number of follows to publish in each test account's contact list.
/// Set to ~1000 to simulate a well-connected Nostr user and stress-test the
/// login flow's follow-list processing path.
const DEFAULT_FOLLOW_COUNT: usize = 1000;

/// How many follow-user publish tasks run concurrently during setup.
/// Each task creates a client connection, publishes 4 events (metadata +
/// 3 relay lists), and disconnects — too much concurrency risks overwhelming
/// the local test relays.
const SETUP_CONCURRENCY: usize = 25;

/// Benchmark scenario for measuring login performance with large follow lists.
///
/// This scenario simulates logging in with an established Nostr account that has:
/// - Published metadata (name, about)
/// - Published relay lists (NIP-65, Inbox, KeyPackage)
/// - A large contact list (~1000 follows)
///
/// Each followed user also has metadata and relay lists published to the local
/// test relays, simulating a realistic Nostr social graph where followed users
/// are established accounts with discoverable relay information.
///
/// The setup phase pre-generates unique keypairs and publishes all prerequisite
/// events to the local test relays. Each benchmark iteration then calls
/// `login()` with one of these keypairs and measures the elapsed time.
///
/// This is designed to reproduce the slow-login issue reported in #480, where
/// accounts with large follow lists experience significantly longer login times.
pub struct LoginPerformanceBenchmark {
    follow_count: usize,
    test_case: Option<LoginBenchmark>,
}

impl LoginPerformanceBenchmark {
    pub fn new(follow_count: usize) -> Self {
        Self {
            follow_count,
            test_case: None,
        }
    }
}

impl Default for LoginPerformanceBenchmark {
    fn default() -> Self {
        Self::new(DEFAULT_FOLLOW_COUNT)
    }
}

#[async_trait]
impl BenchmarkScenario for LoginPerformanceBenchmark {
    fn name(&self) -> &str {
        "Login Performance (Large Follow List)"
    }

    fn config(&self) -> BenchmarkConfig {
        BenchmarkConfig {
            // Login is network-heavy (relay fetches, subscriptions); keep iterations
            // modest to avoid extremely long benchmark runs while still getting
            // statistically useful data.
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
            "Setting up login benchmark: {} iterations, {} follows per account",
            total_iterations,
            self.follow_count
        );

        // Generate keypairs for all followed users and publish their data to the
        // local test relays. This simulates a realistic Nostr graph where followed
        // users have discoverable metadata and relay lists.
        tracing::info!(
            "Publishing metadata and relay lists for {} follow users ({} concurrent)...",
            self.follow_count,
            SETUP_CONCURRENCY
        );

        let follow_keys: Vec<Keys> = (0..self.follow_count).map(|_| Keys::generate()).collect();
        let follow_pubkeys: Vec<PublicKey> = follow_keys.iter().map(|k| k.public_key()).collect();

        // Publish metadata + relay lists for each follow user concurrently
        let dev_relays = context.dev_relays.clone();
        let relay_urls_for_follows = relay_urls.clone();
        let total_follows = self.follow_count;

        let errors: Vec<(usize, WhitenoiseError)> =
            stream::iter(follow_keys.into_iter().enumerate())
                .map(|(i, keys)| {
                    let dev_relays = dev_relays.clone();
                    let relay_urls = relay_urls_for_follows.clone();
                    async move {
                        let result: Result<(), WhitenoiseError> = async {
                            let test_client = create_test_client(&dev_relays, keys).await?;

                            publish_test_metadata(
                                &test_client,
                                &format!("Follow User {}", i),
                                "Followed user for login benchmark",
                            )
                            .await?;

                            publish_relay_lists(&test_client, relay_urls).await?;

                            test_client.disconnect().await;
                            Ok(())
                        }
                        .await;

                        if let Err(e) = &result {
                            (i, Some(e.to_string()))
                        } else {
                            (i, None)
                        }
                    }
                })
                .buffer_unordered(SETUP_CONCURRENCY)
                .filter_map(|(i, err)| async move {
                    err.map(|msg| (i, WhitenoiseError::Other(anyhow::anyhow!(msg))))
                })
                .collect()
                .await;

        if !errors.is_empty() {
            tracing::warn!(
                "Failed to publish data for {} follow users (continuing with partial setup)",
                errors.len()
            );
        }

        tracing::info!(
            "Published data for {}/{} follow users",
            total_follows - errors.len(),
            total_follows
        );

        // Pre-generate keypairs and publish prerequisite events for each
        // login iteration (the accounts being benchmarked)
        let mut prepared_keys = Vec::with_capacity(total_iterations);

        for i in 0..total_iterations {
            let keys = Keys::generate();
            let test_client = create_test_client(&context.dev_relays, keys.clone()).await?;

            // Publish metadata
            publish_test_metadata(
                &test_client,
                &format!("Login Benchmark User {}", i),
                "Benchmark test account with large follow list",
            )
            .await?;

            // Publish relay lists (NIP-65, Inbox, KeyPackage) pointing to test relays
            publish_relay_lists(&test_client, relay_urls.clone()).await?;

            // Publish the large contact list
            publish_follow_list(&test_client, &follow_pubkeys).await?;

            test_client.disconnect().await;

            prepared_keys.push(keys);

            if (i + 1) % 5 == 0 || i + 1 == total_iterations {
                tracing::info!("Prepared {}/{} test accounts", i + 1, total_iterations);
            }
        }

        // Brief pause to let relays index the published events
        tokio::time::sleep(Duration::from_secs(2)).await;

        tracing::info!(
            "Setup complete: {} accounts with {} follows each (all follows have metadata + relay lists)",
            total_iterations,
            self.follow_count
        );

        self.test_case = Some(LoginBenchmark::new(prepared_keys));

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
