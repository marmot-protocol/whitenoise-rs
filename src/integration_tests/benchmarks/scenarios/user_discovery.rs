use std::time::Duration;

use async_trait::async_trait;
use nostr_sdk::{EventBuilder, Keys, Metadata};

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::test_cases::{
    UserResolutionBenchmark, UserResolutionOperation,
};
use crate::integration_tests::benchmarks::{BenchmarkConfig, BenchmarkScenario, BenchmarkTestCase};
use crate::integration_tests::core::ScenarioContext;
use crate::integration_tests::core::test_clients::create_test_client;

pub struct UserDiscoveryBenchmark {
    operation: UserResolutionOperation,
    test_case: Option<UserResolutionBenchmark>,
}

impl UserDiscoveryBenchmark {
    pub fn new(operation: UserResolutionOperation) -> Self {
        Self {
            operation,
            test_case: None,
        }
    }

    pub fn for_get_or_create_local() -> Self {
        Self::new(UserResolutionOperation::GetOrCreateLocal)
    }

    pub fn for_resolve_user() -> Self {
        Self::new(UserResolutionOperation::Resolve)
    }

    pub fn for_resolve_user_blocking() -> Self {
        Self::new(UserResolutionOperation::ResolveBlocking)
    }
}

#[async_trait]
impl BenchmarkScenario for UserDiscoveryBenchmark {
    fn name(&self) -> &str {
        match self.operation {
            UserResolutionOperation::GetOrCreateLocal => "User Resolution - Local Only",
            UserResolutionOperation::Resolve => "User Resolution - resolve_user",
            UserResolutionOperation::ResolveBlocking => "User Resolution - resolve_user_blocking",
        }
    }

    fn config(&self) -> BenchmarkConfig {
        BenchmarkConfig {
            iterations: 100,
            warmup_iterations: 0,
            cooldown_between_iterations: Duration::from_millis(50),
        }
    }

    async fn setup(&mut self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let num_users = self.config().iterations as usize;
        tracing::info!("Creating {} test users with metadata...", num_users);

        // Create one account so we can subscribe to events
        context.whitenoise.create_identity().await?;

        // Generate keypairs and publish metadata for each (one per iteration)
        let mut pubkeys = Vec::with_capacity(num_users);

        for i in 0..num_users {
            let keys = Keys::generate();
            let pubkey = keys.public_key();
            pubkeys.push(pubkey);

            // Create test client and publish metadata
            let test_client = create_test_client(&context.dev_relays, keys).await?;
            let metadata = Metadata::new()
                .name(format!("Benchmark User {}", i))
                .about("User for benchmark testing");

            test_client
                .send_event_builder(EventBuilder::metadata(&metadata))
                .await?;

            test_client.disconnect().await;

            if (i + 1) % 10 == 0 {
                tracing::info!("Created {}/{} test users", i + 1, num_users);
            }
        }

        tracing::info!(
            "✓ Setup complete - {} users with published metadata",
            num_users
        );

        self.test_case = Some(UserResolutionBenchmark::new(self.operation, pubkeys));

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
