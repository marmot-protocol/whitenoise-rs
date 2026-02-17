use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use mdk_core::prelude::GroupId;

pub struct SaveDraftTestCase {
    account_name: String,
    group_name: String,
    content: String,
    expected_content: Option<String>,
}

impl SaveDraftTestCase {
    pub fn new(account_name: &str, group_name: &str, content: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
            content: content.to_string(),
            expected_content: None,
        }
    }

    pub fn expect_content(mut self, content: &str) -> Self {
        self.expected_content = Some(content.to_string());
        self
    }
}

#[async_trait]
impl TestCase for SaveDraftTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Saving draft for account '{}' in group '{}'...",
            self.account_name,
            self.group_name
        );

        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;
        let group_id = GroupId::from_slice(group.mls_group_id.as_slice());

        let draft = context
            .whitenoise
            .save_draft(account, &group_id, &self.content, None, &[])
            .await?;

        assert!(draft.id.is_some(), "Draft should have an ID after save");
        assert_eq!(draft.account_pubkey, account.pubkey);
        assert_eq!(draft.mls_group_id, group_id);
        assert_eq!(draft.content, self.content);

        if let Some(expected) = &self.expected_content {
            assert_eq!(&draft.content, expected, "Content mismatch");
        }

        tracing::info!("✓ Draft saved successfully with ID: {:?}", draft.id);
        Ok(())
    }
}
