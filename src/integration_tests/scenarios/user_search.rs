use crate::integration_tests::{
    core::*,
    test_cases::{shared::*, user_search::*},
};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

pub struct UserSearchScenario {
    context: ScenarioContext,
}

impl UserSearchScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for UserSearchScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        CreateAccountsTestCase::with_names(vec!["searcher"])
            .execute(&mut self.context)
            .await?;

        tracing::info!("Testing: Search finds directly followed users at radius 1");
        SearchDirectFollowsTestCase::new("searcher")
            .execute(&mut self.context)
            .await?;

        tracing::info!("Testing: Search discovers follows-of-follows at radius 2");
        SearchFollowsOfFollowsTestCase::new("searcher")
            .execute(&mut self.context)
            .await?;

        tracing::info!("Testing: Search behavior with empty metadata in User table");
        SearchEmptyMetadataTestCase::new("searcher")
            .execute(&mut self.context)
            .await?;

        tracing::info!("Testing: Incremental single-radius search (0,1) then (2,2)");
        SearchIncrementalRadiusTestCase::new("searcher")
            .execute(&mut self.context)
            .await?;

        // Use a fresh account with no follows for the fallback test
        CreateAccountsTestCase::with_names(vec!["isolated"])
            .execute(&mut self.context)
            .await?;

        tracing::info!("Testing: Fallback seed injection when social graph is empty");
        SearchFallbackSeedTestCase::new("isolated")
            .execute(&mut self.context)
            .await?;

        Ok(())
    }
}
