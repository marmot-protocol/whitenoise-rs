use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::group_information::GroupType;
use async_trait::async_trait;

/// Verifies the chat list for an account matches expected conditions.
pub struct VerifyChatListTestCase {
    account_name: String,
    expected_group_count: usize,
    expected_dm_count: usize,
    expected_first_chat_group_name: Option<String>,
}

impl VerifyChatListTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            expected_group_count: 0,
            expected_dm_count: 0,
            expected_first_chat_group_name: None,
        }
    }

    pub fn expecting_groups(mut self, count: usize) -> Self {
        self.expected_group_count = count;
        self
    }

    pub fn expecting_dms(mut self, count: usize) -> Self {
        self.expected_dm_count = count;
        self
    }

    pub fn expecting_first_chat(mut self, group_context_name: &str) -> Self {
        self.expected_first_chat_group_name = Some(group_context_name.to_string());
        self
    }
}

#[async_trait]
impl TestCase for VerifyChatListTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Verifying chat list for account: {}", self.account_name);

        let account = context.get_account(&self.account_name)?;
        let chat_list = context.whitenoise.get_chat_list(account).await?;

        // Count groups and DMs
        let group_count = chat_list
            .iter()
            .filter(|item| item.group_type == GroupType::Group)
            .count();
        let dm_count = chat_list
            .iter()
            .filter(|item| item.group_type == GroupType::DirectMessage)
            .count();

        tracing::info!(
            "Chat list contains {} groups and {} DMs (expected {} groups, {} DMs)",
            group_count,
            dm_count,
            self.expected_group_count,
            self.expected_dm_count
        );

        // When expecting empty list, verify it's actually empty
        if self.expected_group_count == 0 && self.expected_dm_count == 0 {
            assert!(
                chat_list.is_empty(),
                "Expected empty chat list but found {} items",
                chat_list.len()
            );
            tracing::info!("✓ Empty chat list verification passed");
            return Ok(());
        }

        assert_eq!(
            group_count, self.expected_group_count,
            "Expected {} groups but found {}",
            self.expected_group_count, group_count
        );
        assert_eq!(
            dm_count, self.expected_dm_count,
            "Expected {} DMs but found {}",
            self.expected_dm_count, dm_count
        );

        // Verify first chat matches expected (most recent activity)
        if let Some(expected_group_name) = &self.expected_first_chat_group_name {
            let expected_group = context.get_group(expected_group_name)?;
            assert!(
                !chat_list.is_empty(),
                "Chat list is empty but expected first chat"
            );
            assert_eq!(
                chat_list[0].mls_group_id, expected_group.mls_group_id,
                "First chat should be '{}' (most recent activity)",
                expected_group_name
            );
        }

        tracing::info!("✓ Chat list verification passed");
        Ok(())
    }
}
