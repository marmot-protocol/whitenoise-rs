use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use mdk_core::prelude::GroupId;

pub struct LoadDraftTestCase {
    account_name: String,
    group_name: String,
    expect_exists: bool,
    expected_content: Option<String>,
}

impl LoadDraftTestCase {
    pub fn new(account_name: &str, group_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
            expect_exists: true,
            expected_content: None,
        }
    }

    pub fn expect_not_found(mut self) -> Self {
        self.expect_exists = false;
        self
    }

    pub fn expect_content(mut self, content: &str) -> Self {
        self.expected_content = Some(content.to_string());
        self
    }
}

#[async_trait]
impl TestCase for LoadDraftTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Loading draft for account '{}' in group '{}'...",
            self.account_name,
            self.group_name
        );

        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;
        let group_id = GroupId::from_slice(group.mls_group_id.as_slice());

        let loaded = context.whitenoise.load_draft(account, &group_id).await?;

        if self.expect_exists {
            let draft = loaded.expect("Draft should exist");
            assert!(draft.id.is_some());
            assert_eq!(draft.account_pubkey, account.pubkey);
            assert_eq!(draft.mls_group_id, group_id);

            if let Some(expected) = &self.expected_content {
                assert_eq!(&draft.content, expected, "Content mismatch");
            }

            tracing::info!("✓ Draft loaded successfully: {:?}", draft.content);
        } else {
            assert!(loaded.is_none(), "Draft should not exist");
            tracing::info!("✓ No draft found (as expected)");
        }

        Ok(())
    }
}
