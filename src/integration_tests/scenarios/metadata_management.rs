use std::sync::Arc;

use crate::integration_tests::{
    core::*,
    test_cases::{metadata_management::*, shared::*},
};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

pub struct MetadataManagementScenario {
    context: ScenarioContext,
}

impl MetadataManagementScenario {
    pub fn new(whitenoise: Arc<Whitenoise>) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for MetadataManagementScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        // Create a test account for metadata operations
        CreateAccountsTestCase::with_names(vec!["metadata_user"])
            .execute(&mut self.context)
            .await?;

        // Test fetching default metadata (should have a generated petname)
        FetchMetadataTestCase::for_account("metadata_user")
            .execute(&mut self.context)
            .await?;

        // Test updating metadata
        UpdateMetadataTestCase::for_account("metadata_user")
            .with_name("Test User")
            .with_about("A test user for metadata testing")
            .with_website("https://example.com")
            .with_nip05("test@example.com")
            .with_picture("https://example.com/avatar.jpg")
            .execute(&mut self.context)
            .await?;

        Ok(())
    }
}
