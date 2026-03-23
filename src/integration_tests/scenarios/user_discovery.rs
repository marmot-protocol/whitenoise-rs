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
        ResolveUserBlockingTestCase::basic()
            .execute(&mut self.context)
            .await?;

        tracing::info!(target: LOG_TARGET, "Testing: With metadata");
        ResolveUserBlockingTestCase::basic()
            .with_metadata()
            .execute(&mut self.context)
            .await?;

        tracing::info!(target: LOG_TARGET, "Testing: With relays");
        ResolveUserBlockingTestCase::basic()
            .with_relays()
            .execute(&mut self.context)
            .await?;

        tracing::info!(target: LOG_TARGET, "Testing: With metadata and relays");
        ResolveUserBlockingTestCase::basic()
            .with_metadata()
            .with_relays()
            .execute(&mut self.context)
            .await?;

        tracing::info!(target: LOG_TARGET, "Testing: resolve_user for a new unknown user");
        ResolveUserTestCase::new()
            .execute(&mut self.context)
            .await?;

        tracing::info!(
            target: LOG_TARGET,
            "Testing: known metadata is returned locally without a forced refresh"
        );
        ResolveUserKnownMetadataNoRefreshTestCase::new()
            .execute(&mut self.context)
            .await?;

        tracing::info!(
            target: LOG_TARGET,
            "Testing: Older relay metadata cannot overwrite newer processed metadata"
        );
        ResolveUserPreservesNewerProcessedMetadataTestCase::new()
            .execute(&mut self.context)
            .await?;

        tracing::info!(
            target: LOG_TARGET,
            "Testing: Unknown metadata remains unknown when discovery finds nothing"
        );
        ResolveUserUnknownMetadataNoResultTestCase::new()
            .execute(&mut self.context)
            .await?;

        tracing::info!(
            target: LOG_TARGET,
            "Testing: subscribe_to_user and resolve_user share the same local-state contract"
        );
        SubscribeToUserResolveUserSharedStateTestCase::new()
            .execute(&mut self.context)
            .await?;

        Ok(())
    }
}
