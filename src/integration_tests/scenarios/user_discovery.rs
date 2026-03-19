use crate::integration_tests::{core::*, test_cases::user_discovery::*};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

const LOG_TARGET: &str = "integration_tests::scenarios::user_discovery";

pub struct UserDiscoveryScenario {
    context: ScenarioContext,
}

impl UserDiscoveryScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for UserDiscoveryScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!(target: LOG_TARGET, "Testing: No metadata and no relays");
        FindOrCreateUserTestCase::basic()
            .execute(&mut self.context)
            .await?;

        tracing::info!(target: LOG_TARGET, "Testing: With metadata");
        FindOrCreateUserTestCase::basic()
            .with_metadata()
            .execute(&mut self.context)
            .await?;

        tracing::info!(target: LOG_TARGET, "Testing: With relays");
        FindOrCreateUserTestCase::basic()
            .with_relays()
            .execute(&mut self.context)
            .await?;

        tracing::info!(target: LOG_TARGET, "Testing: With metadata and relays");
        FindOrCreateUserTestCase::basic()
            .with_metadata()
            .with_relays()
            .execute(&mut self.context)
            .await?;

        tracing::info!(target: LOG_TARGET, "Testing: Background mode for new user");
        FindOrCreateUserBackgroundModeTestCase::new()
            .execute(&mut self.context)
            .await?;

        tracing::info!(
            target: LOG_TARGET,
            "Testing: Older relay metadata cannot overwrite newer processed metadata"
        );
        FindOrCreateUserPreservesNewerProcessedMetadataTestCase::new()
            .execute(&mut self.context)
            .await?;

        tracing::info!(
            target: LOG_TARGET,
            "Testing: Unknown metadata remains unknown when discovery finds nothing"
        );
        FindOrCreateUserUnknownMetadataNoResultTestCase::new()
            .execute(&mut self.context)
            .await?;

        Ok(())
    }
}
