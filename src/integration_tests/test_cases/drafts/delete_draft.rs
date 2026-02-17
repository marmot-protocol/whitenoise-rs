use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use mdk_core::prelude::GroupId;

pub struct DeleteDraftTestCase {
    account_name: String,
    group_name: String,
    expect_precondition: bool,
}

impl DeleteDraftTestCase {
    pub fn new(account_name: &str, group_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
            expect_precondition: true,
        }
    }

    pub fn expect_no_draft(mut self) -> Self {
        self.expect_precondition = false;
        self
    }
}

#[async_trait]
impl TestCase for DeleteDraftTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Deleting draft for account '{}' in group '{}'...",
            self.account_name,
            self.group_name
        );

        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;
        let group_id = GroupId::from_slice(group.mls_group_id.as_slice());

        if self.expect_precondition {
            let loaded = context.whitenoise.load_draft(account, &group_id).await?;
            assert!(loaded.is_some(), "Draft should exist before deletion");
        }

        context.whitenoise.delete_draft(account, &group_id).await?;

        let loaded = context.whitenoise.load_draft(account, &group_id).await?;
        assert!(loaded.is_none(), "Draft should not exist after deletion");

        tracing::info!("✓ Draft deleted successfully");
        Ok(())
    }
}
