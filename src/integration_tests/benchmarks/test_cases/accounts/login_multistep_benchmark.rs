use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::BenchmarkTestCase;
use crate::integration_tests::core::ScenarioContext;
use crate::whitenoise::accounts::LoginStatus;

/// Benchmark test case for measuring the multi-step login flow:
/// `login_start` → `login_publish_default_relays`.
///
/// Each iteration uses a fresh keypair with NO relay lists published to the
/// network. `login_start` discovers nothing and returns `NeedsRelayLists`,
/// then `login_publish_default_relays` publishes default relay lists and
/// activates the account.
///
/// The entire two-step flow is timed as one operation. The perf breakdown
/// from `#[perf_instrument]` on each method provides the per-step timings.
pub struct LoginMultistepBenchmark {
    /// Pre-generated keypairs, one per iteration. None have relay lists
    /// published — login_start will return NeedsRelayLists for each.
    prepared_keys: Vec<Keys>,
    /// Monotonic counter that is NOT reset between warmup and benchmark
    /// phases. The benchmark framework resets `context.tests_count` after
    /// warmup, but this test case publishes relay lists during each
    /// iteration — reusing a keypair would cause `login_start` to return
    /// `Complete` instead of `NeedsRelayLists`.
    next_key: AtomicUsize,
}

impl LoginMultistepBenchmark {
    pub fn new(prepared_keys: Vec<Keys>) -> Self {
        Self {
            prepared_keys,
            next_key: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl BenchmarkTestCase for LoginMultistepBenchmark {
    async fn run_iteration(
        &self,
        context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        let iteration = self.next_key.fetch_add(1, Ordering::Relaxed);
        if iteration >= self.prepared_keys.len() {
            return Err(WhitenoiseError::Internal(format!(
                "Login multistep benchmark iteration {} exceeds prepared keys count ({})",
                iteration,
                self.prepared_keys.len()
            )));
        }
        let keys = &self.prepared_keys[iteration];

        // Time the entire multi-step login flow
        let start = Instant::now();

        let result = context
            .whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await?;

        if result.status != LoginStatus::NeedsRelayLists {
            return Err(WhitenoiseError::Internal(format!(
                "Expected LoginStatus::NeedsRelayLists but got {:?}",
                result.status
            )));
        }

        let final_result = context
            .whitenoise
            .login_publish_default_relays(&result.account.pubkey)
            .await?;

        let duration = start.elapsed();

        if final_result.status != LoginStatus::Complete {
            return Err(WhitenoiseError::Internal(format!(
                "Expected LoginStatus::Complete after publish_default_relays but got {:?}",
                final_result.status
            )));
        }

        // Log out so the next iteration starts clean
        context
            .whitenoise
            .logout(&final_result.account.pubkey)
            .await?;

        Ok(duration)
    }
}
