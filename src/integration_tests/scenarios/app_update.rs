use crate::integration_tests::{core::*, test_cases::app_update::*};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

/// Scenario that tests the app update checking functionality.
///
/// This scenario connects to the Zapstore relay to verify that
/// the app can successfully query for available updates.
pub struct AppUpdateScenario {
    context: ScenarioContext,
}

impl AppUpdateScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for AppUpdateScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        // Test checking for updates with an old version (should find update)
        CheckForUpdateTestCase::with_version("0.0.1")
            .expect_update_available()
            .execute(&mut self.context)
            .await?;

        // Test checking for updates with a very high version (should not find update)
        CheckForUpdateTestCase::with_version("999.999.999")
            .expect_no_update_available()
            .execute(&mut self.context)
            .await?;

        Ok(())
    }
}
