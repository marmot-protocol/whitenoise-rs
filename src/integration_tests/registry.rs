use crate::integration_tests::core::*;
use crate::integration_tests::scenarios::*;
use crate::{Whitenoise, WhitenoiseError};
use std::time::{Duration, Instant};

/// Macro to register integration test scenarios in a single place.
/// Add a new scenario by adding one line with: "cli-name" => ScenarioType
macro_rules! scenario_registry {
    ($($name:literal => $scenario_type:ty),* $(,)?) => {
        /// Get all registered scenario names (kebab-case)
        fn get_all_scenario_names() -> Vec<&'static str> {
            vec![$($name),*]
        }

        /// Parse scenario name and return the scenario type name for display
        fn parse_scenario_name(name: &str) -> Result<&'static str, String> {
            match name.to_lowercase().as_str() {
                $(
                    $name => Ok($name),
                )*
                _ => {
                    let available = get_all_scenario_names().join("\n  - ");
                    Err(format!(
                        "Unknown scenario '{}'. Available scenarios:\n  - {}",
                        name, available
                    ))
                }
            }
        }

        /// Run a single scenario by name
        async fn run_single_scenario(
            name: &str,
            whitenoise: &'static Whitenoise,
        ) -> Result<(ScenarioResult, Option<WhitenoiseError>), String> {
            match name.to_lowercase().as_str() {
                $(
                    $name => Ok(<$scenario_type>::new(whitenoise).execute().await),
                )*
                _ => {
                    let available = get_all_scenario_names().join("\n  - ");
                    Err(format!(
                        "Unknown scenario '{}'. Available scenarios:\n  - {}",
                        name, available
                    ))
                }
            }
        }

        /// Run all registered scenarios
        async fn run_all_registered(
            whitenoise: &'static Whitenoise,
            results: &mut Vec<ScenarioResult>,
            first_error: &mut Option<WhitenoiseError>,
        ) {
            $(
                let (result, error) = <$scenario_type>::new(whitenoise).execute().await;
                results.push(result);
                if error.is_some() && first_error.is_none() {
                    *first_error = error;
                }
                // Give some breathing room between scenarios
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            )*
        }
    };
}

// ============================================================================
// SCENARIO REGISTRY - Add new scenarios here (one line each)
// ============================================================================
scenario_registry! {
    "account-management" => AccountManagementScenario,
    "app-settings" => AppSettingsScenario,
    "metadata-management" => MetadataManagementScenario,
    "basic-messaging" => BasicMessagingScenario,
    "follow-management" => FollowManagementScenario,
    "subscription-processing" => SubscriptionProcessingScenario,
    "advanced-messaging" => AdvancedMessagingScenario,
    "group-membership" => GroupMembershipScenario,
    "chat-media-upload" => ChatMediaUploadScenario,
    "user-discovery" => UserDiscoveryScenario,
    "scheduler" => SchedulerScenario,
    "message-streaming" => MessageStreamingScenario,
    "chat-list" => ChatListScenario,
}
// ============================================================================

pub struct ScenarioRegistry;

impl ScenarioRegistry {
    /// Run a single scenario by name
    pub async fn run_scenario(
        scenario_name: &str,
        whitenoise: &'static Whitenoise,
    ) -> Result<(), WhitenoiseError> {
        let overall_start = Instant::now();

        // Validate scenario name
        parse_scenario_name(scenario_name).map_err(WhitenoiseError::InvalidInput)?;

        tracing::info!("=== Running Scenario: {} ===", scenario_name);

        // Run the single scenario
        let (result, error) = run_single_scenario(scenario_name, whitenoise)
            .await
            .map_err(WhitenoiseError::InvalidInput)?;

        // Print summary for this single scenario
        Self::print_summary(&[result], overall_start.elapsed()).await;

        // Return error if scenario failed
        match error {
            Some(e) => {
                tracing::error!("=== Scenario Failed ===");
                Err(e)
            }
            None => {
                tracing::info!("=== Scenario Completed Successfully ===");
                Ok(())
            }
        }
    }

    pub async fn run_all_scenarios(whitenoise: &'static Whitenoise) -> Result<(), WhitenoiseError> {
        let overall_start = Instant::now();
        let mut results = Vec::new();
        let mut first_error = None;

        // Run all registered scenarios
        run_all_registered(whitenoise, &mut results, &mut first_error).await;

        Self::print_summary(&results, overall_start.elapsed()).await;

        // Return the first error encountered, if any
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn print_summary(results: &[ScenarioResult], overall_duration: Duration) {
        tokio::time::sleep(Duration::from_secs(1)).await; // Wait for the logs to be flushed
        tracing::info!("=== Integration Test Summary ===");

        tracing::info!("Detailed Results:");
        for result in results {
            let status = if result.success { "✓" } else { "✗" };
            tracing::info!(
                "  {} {} - {}/{} tests passed in {:?}",
                status,
                result.scenario_name,
                result.tests_passed,
                result.tests_run,
                result.duration
            );
        }

        tracing::info!("Total duration: {:?}", overall_duration);

        let total_passed = results.iter().map(|r| r.tests_passed).sum::<u32>();
        let total_failed = results.iter().map(|r| r.tests_failed).sum::<u32>();

        let scenarios_passed = results.iter().filter(|r| r.success).count();
        let scenarios_failed = results.iter().filter(|r| !r.success).count();

        tracing::info!(
            "Scenarios: {} passed, {} failed",
            scenarios_passed,
            scenarios_failed
        );
        tracing::info!(
            "Test Cases: {} passed, {} failed",
            total_passed,
            total_failed
        );

        // Give async logging time to flush before program exits
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scenario_registry_exists() {
        // Simple test to verify the registry struct exists
        let _registry = ScenarioRegistry;
    }

    #[test]
    fn test_parse_valid_scenario_names() {
        // Test all valid scenario names can be parsed
        assert!(parse_scenario_name("account-management").is_ok());
        assert!(parse_scenario_name("app-settings").is_ok());
        assert!(parse_scenario_name("metadata-management").is_ok());
        assert!(parse_scenario_name("basic-messaging").is_ok());
        assert!(parse_scenario_name("follow-management").is_ok());
        assert!(parse_scenario_name("subscription-processing").is_ok());
        assert!(parse_scenario_name("advanced-messaging").is_ok());
        assert!(parse_scenario_name("group-membership").is_ok());
        assert!(parse_scenario_name("chat-media-upload").is_ok());
        assert!(parse_scenario_name("user-discovery").is_ok());
        assert!(parse_scenario_name("scheduler").is_ok());
        assert!(parse_scenario_name("message-streaming").is_ok());
        assert!(parse_scenario_name("chat-list").is_ok());
    }

    #[test]
    fn test_parse_case_insensitive() {
        // Test case insensitivity
        assert!(parse_scenario_name("ACCOUNT-MANAGEMENT").is_ok());
        assert!(parse_scenario_name("Basic-Messaging").is_ok());
        assert!(parse_scenario_name("USER-DISCOVERY").is_ok());
    }

    #[test]
    fn test_parse_invalid_scenario_name() {
        // Test invalid scenario name
        let result = parse_scenario_name("invalid-scenario");
        assert!(result.is_err());
        if let Err(error_msg) = result {
            assert!(error_msg.contains("Unknown scenario 'invalid-scenario'"));
            assert!(error_msg.contains("Available scenarios:"));
            assert!(error_msg.contains("account-management"));
        }
    }

    #[test]
    fn test_get_all_scenario_names() {
        // Test that all scenario names are returned
        let names = get_all_scenario_names();
        assert_eq!(names.len(), 13);
        assert!(names.contains(&"account-management"));
        assert!(names.contains(&"basic-messaging"));
        assert!(names.contains(&"user-discovery"));
        assert!(names.contains(&"scheduler"));
        assert!(names.contains(&"message-streaming"));
        assert!(names.contains(&"chat-list"));
    }
}
