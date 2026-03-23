use std::time::{Duration, Instant};

use async_trait::async_trait;
use nostr_sdk::PublicKey;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::BenchmarkTestCase;
use crate::integration_tests::core::ScenarioContext;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserResolutionOperation {
    GetOrCreateLocal,
    Resolve,
    ResolveBlocking,
}

/// Benchmark test case for measuring single-user resolution performance.
pub struct UserResolutionBenchmark {
    operation: UserResolutionOperation,
    pubkeys: Vec<PublicKey>,
}

impl UserResolutionBenchmark {
    pub fn new(operation: UserResolutionOperation, pubkeys: Vec<PublicKey>) -> Self {
        assert!(!pubkeys.is_empty(), "pubkeys cannot be empty");
        Self { operation, pubkeys }
    }

    pub fn for_get_or_create_local(pubkeys: Vec<PublicKey>) -> Self {
        Self::new(UserResolutionOperation::GetOrCreateLocal, pubkeys)
    }

    pub fn for_resolve_user(pubkeys: Vec<PublicKey>) -> Self {
        Self::new(UserResolutionOperation::Resolve, pubkeys)
    }

    pub fn for_resolve_user_blocking(pubkeys: Vec<PublicKey>) -> Self {
        Self::new(UserResolutionOperation::ResolveBlocking, pubkeys)
    }
}

#[async_trait]
impl BenchmarkTestCase for UserResolutionBenchmark {
    async fn run_iteration(
        &self,
        context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        // Get pubkey for this iteration (cycle through the list)
        let pubkey = &self.pubkeys[context.tests_count as usize % self.pubkeys.len()];

        let start = Instant::now();
        match self.operation {
            UserResolutionOperation::GetOrCreateLocal => {
                context.whitenoise.get_or_create_user_local(pubkey).await?;
            }
            UserResolutionOperation::Resolve => {
                context.whitenoise.resolve_user(pubkey).await?;
            }
            UserResolutionOperation::ResolveBlocking => {
                context.whitenoise.resolve_user_blocking(pubkey).await?;
            }
        }
        let duration = start.elapsed();

        // Increment test count for next iteration
        context.tests_count += 1;

        Ok(duration)
    }
}
