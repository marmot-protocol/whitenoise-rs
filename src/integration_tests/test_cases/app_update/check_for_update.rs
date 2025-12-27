use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::app_update::check_for_app_update;

/// Test case that checks for app updates from the Zapstore relay
pub struct CheckForUpdateTestCase {
    current_version: String,
    expect_update_available: Option<bool>,
}

impl CheckForUpdateTestCase {
    /// Create a new test case with a specified current version
    pub fn with_version(version: &str) -> Self {
        Self {
            current_version: version.to_string(),
            expect_update_available: None,
        }
    }

    /// Assert that an update should be available
    pub fn expect_update_available(mut self) -> Self {
        self.expect_update_available = Some(true);
        self
    }

    /// Assert that no update should be available
    pub fn expect_no_update_available(mut self) -> Self {
        self.expect_update_available = Some(false);
        self
    }
}

#[async_trait]
impl TestCase for CheckForUpdateTestCase {
    async fn run(&self, _context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Checking for app update with current version: {}",
            self.current_version
        );

        // Call the actual function that connects to the Zapstore relay
        let update_info = check_for_app_update(&self.current_version).await?;

        tracing::info!(
            "✓ Received update info - Latest version: {}, Update available: {}",
            update_info.version,
            update_info.update_available
        );

        // Verify version format (should be x.y.z)
        let parts: Vec<&str> = update_info.version.split('.').collect();
        if parts.len() != 3 {
            return Err(WhitenoiseError::Other(anyhow::anyhow!(
                "Invalid version format: {}",
                update_info.version
            )));
        }

        // Verify all parts are valid numbers
        for part in &parts {
            if part.parse::<u32>().is_err() {
                return Err(WhitenoiseError::Other(anyhow::anyhow!(
                    "Invalid version component: {}",
                    part
                )));
            }
        }

        tracing::info!("✓ Version format validated: {}", update_info.version);

        // Check update availability expectation if specified
        if let Some(expected) = self.expect_update_available {
            if update_info.update_available != expected {
                return Err(WhitenoiseError::Other(anyhow::anyhow!(
                    "Update availability mismatch: expected {}, got {}",
                    expected,
                    update_info.update_available
                )));
            }
            tracing::info!(
                "✓ Update availability verified: {}",
                update_info.update_available
            );
        }

        tracing::info!("✓ App update check completed successfully");
        Ok(())
    }
}
