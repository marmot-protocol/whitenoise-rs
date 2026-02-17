use crate::integration_tests::core::*;
use crate::integration_tests::test_cases::drafts::*;
use crate::integration_tests::test_cases::shared::*;
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

pub struct DraftsScenario {
    context: ScenarioContext,
}

impl DraftsScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for DraftsScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        CreateAccountsTestCase::with_names(vec!["alice"])
            .execute(&mut self.context)
            .await?;

        CreateGroupTestCase::basic()
            .with_members("alice", vec![])
            .execute(&mut self.context)
            .await?;

        SaveDraftTestCase::new("alice", "test_group", "Hello, world!")
            .execute(&mut self.context)
            .await?;

        LoadDraftTestCase::new("alice", "test_group")
            .expect_content("Hello, world!")
            .execute(&mut self.context)
            .await?;

        SaveDraftTestCase::new("alice", "test_group", "Updated content")
            .execute(&mut self.context)
            .await?;

        LoadDraftTestCase::new("alice", "test_group")
            .expect_content("Updated content")
            .execute(&mut self.context)
            .await?;

        DeleteDraftTestCase::new("alice", "test_group")
            .execute(&mut self.context)
            .await?;

        LoadDraftTestCase::new("alice", "test_group")
            .expect_not_found()
            .execute(&mut self.context)
            .await?;

        DeleteDraftTestCase::new("alice", "test_group")
            .expect_no_draft()
            .execute(&mut self.context)
            .await?;

        Ok(())
    }
}
