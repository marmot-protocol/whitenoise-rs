use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::WhitenoiseError;
use crate::integration_tests::benchmarks::BenchmarkTestCase;
use crate::integration_tests::core::ScenarioContext;

/// Benchmark test case for measuring `add_members_to_group` performance.
///
/// Each iteration adds a fresh set of members (whose key packages are already
/// published) to a pre-created group. MLS key packages are single-use, so
/// each iteration needs unique members.
///
/// Only the `add_members_to_group` call is timed — account creation, group
/// creation, and key package publishing all happen during the scenario's
/// setup phase.
pub struct AddMembersBenchmark {
    /// Name of the admin account stored in ScenarioContext.
    admin_account: String,
    /// Name of the group stored in ScenarioContext.
    group_name: String,
    /// Pre-generated member accounts, one set per iteration.
    member_sets: Vec<Vec<String>>,
    /// Monotonic counter (not reset between warmup and benchmark phases).
    next_set: AtomicUsize,
}

impl AddMembersBenchmark {
    pub fn new(admin_account: &str, group_name: &str, member_sets: Vec<Vec<String>>) -> Self {
        Self {
            admin_account: admin_account.to_string(),
            group_name: group_name.to_string(),
            member_sets,
            next_set: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl BenchmarkTestCase for AddMembersBenchmark {
    async fn run_iteration(
        &self,
        context: &mut ScenarioContext,
    ) -> Result<Duration, WhitenoiseError> {
        let iteration = self.next_set.fetch_add(1, Ordering::Relaxed);
        if iteration >= self.member_sets.len() {
            return Err(WhitenoiseError::Other(anyhow::anyhow!(
                "Add members benchmark iteration {} exceeds prepared member sets ({})",
                iteration,
                self.member_sets.len()
            )));
        }

        let admin = context.get_account(&self.admin_account)?;
        let group = context.get_group(&self.group_name)?;
        let group_id = group.mls_group_id.clone();

        let member_pubkeys: Vec<PublicKey> = self.member_sets[iteration]
            .iter()
            .map(|name| context.get_account(name).map(|acc| acc.pubkey))
            .collect::<Result<Vec<_>, _>>()?;

        let start = Instant::now();

        context
            .whitenoise
            .add_members_to_group(admin, &group_id, member_pubkeys)
            .await?;

        Ok(start.elapsed())
    }
}
