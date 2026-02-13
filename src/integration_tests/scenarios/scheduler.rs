use crate::integration_tests::{core::*, test_cases::scheduler::*};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

/// Integration test scenario for verifying the scheduler and its tasks.
///
/// This scenario tests:
/// 1. Scheduler is running after Whitenoise initialization
/// 2. Key package maintenance task publishes key packages for accounts
/// 3. Full key package lifecycle (publish → consume via Welcome → cleanup)
/// 4. Shutdown completes gracefully
pub struct SchedulerScenario {
    context: ScenarioContext,
}

impl SchedulerScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for SchedulerScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        // Test key package maintenance task
        KeyPackageMaintenanceTestCase::for_account("test_account")
            .execute(&mut self.context)
            .await?;

        // Test full key package lifecycle (publish → consume → cleanup)
        KeyPackageLifecycleTestCase::new("lifecycle_creator", "lifecycle_member")
            .execute(&mut self.context)
            .await?;

        // Verify scheduler lifecycle (running + shutdown)
        SchedulerLifecycleTestCase
            .execute(&mut self.context)
            .await?;

        Ok(())
    }

    // Override cleanup since we already called shutdown
    async fn cleanup(&mut self) -> Result<(), WhitenoiseError> {
        // Skip the normal cleanup since shutdown was already called as part of the test
        // Just wipe the database to reset state
        self.context.whitenoise.wipe_database().await?;
        self.context.whitenoise.reset_nostr_client().await?;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        Ok(())
    }
}
