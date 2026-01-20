use crate::WhitenoiseError;
use crate::integration_tests::core::{ScenarioContext, ScenarioResult};
use async_trait::async_trait;
use std::time::Instant;

#[async_trait]
pub trait TestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError>;

    async fn execute(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let result = self.run(context).await;
        context.record_test(result.is_ok());
        result
    }
}

#[async_trait]
pub trait Scenario {
    /// Get the name of this scenario for logging and reporting
    fn scenario_name(&self) -> &'static str {
        std::any::type_name::<Self>()
            .rsplit("::")
            .next()
            .unwrap_or(std::any::type_name::<Self>())
    }

    /// Get immutable access to the scenario's context
    fn context(&self) -> &ScenarioContext;

    /// Run the actual scenario logic - implement this in each scenario
    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError>;

    /// Execute the scenario with consistent timing, logging and error handling
    /// Always returns a ScenarioResult to ensure consistent reporting
    async fn execute(mut self) -> (ScenarioResult, Option<WhitenoiseError>)
    where
        Self: Sized,
    {
        let start_time = Instant::now();
        let scenario_name = self.scenario_name();

        tracing::info!("=== Running Scenario: {} ===", scenario_name);

        let run_result = self.run_scenario().await;
        let duration = start_time.elapsed();

        let context = self.context();
        let tests_run = context.tests_count;
        let tests_passed = context.tests_passed;

        // Always run cleanup, regardless of scenario success/failure
        // This ensures database state is reset for the next scenario
        let cleanup_result = self.cleanup().await;

        match run_result {
            Ok(()) => {
                // Scenario passed - check if cleanup succeeded
                if let Err(cleanup_err) = cleanup_result {
                    tracing::error!(
                        "✗ {} Scenario cleanup failed: {}",
                        scenario_name,
                        cleanup_err
                    );
                    // Fail the scenario if cleanup fails to prevent corrupted state
                    // from affecting subsequent scenarios
                    (
                        ScenarioResult::failed(scenario_name, tests_run, tests_passed, duration),
                        Some(cleanup_err),
                    )
                } else {
                    tracing::info!(
                        "✓ {} Scenario completed ({}/{}) in {:?}",
                        scenario_name,
                        tests_passed,
                        tests_run,
                        duration
                    );
                    (
                        ScenarioResult::new(scenario_name, tests_run, tests_passed, duration),
                        None,
                    )
                }
            }
            Err(e) => {
                tracing::error!(
                    "✗ {} Scenario failed after {} completed tests in {:?}: {}",
                    scenario_name,
                    tests_passed,
                    duration,
                    e
                );

                // Log cleanup failure but return the original scenario error
                if let Err(cleanup_err) = cleanup_result {
                    tracing::error!(
                        "✗ {} Scenario cleanup also failed: {}",
                        scenario_name,
                        cleanup_err
                    );
                }

                (
                    ScenarioResult::failed(scenario_name, tests_run, tests_passed, duration),
                    Some(e),
                )
            }
        }
    }

    async fn cleanup(&mut self) -> Result<(), WhitenoiseError> {
        let context = self.context();

        // First, logout all accounts to stop their event processing
        for account in context.accounts.values() {
            if let Err(e) = context.whitenoise.logout(&account.pubkey).await {
                match e {
                    WhitenoiseError::AccountNotFound => {} // Account already logged out
                    _ => return Err(e),
                }
            }
        }

        // Give background tasks time to complete and release database connections
        // This helps prevent "database is locked" errors during cleanup
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Reset nostr client first to stop any pending subscriptions
        context.whitenoise.reset_nostr_client().await?;

        // Now wipe the database - this has retry logic for lock contention
        context.whitenoise.wipe_database().await?;

        // Final delay to ensure everything is settled before next scenario
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        Ok(())
    }
}
