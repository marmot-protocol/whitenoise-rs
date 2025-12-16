use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::group_information::GroupType;
use async_trait::async_trait;

/// Verifies DM-specific properties in the chat list.
pub struct VerifyDmChatListItemTestCase {
    account_name: String,
    dm_group_context_name: String,
    other_user_account_name: String,
}

impl VerifyDmChatListItemTestCase {
    pub fn new(
        account_name: &str,
        dm_group_context_name: &str,
        other_user_account_name: &str,
    ) -> Self {
        Self {
            account_name: account_name.to_string(),
            dm_group_context_name: dm_group_context_name.to_string(),
            other_user_account_name: other_user_account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for VerifyDmChatListItemTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Verifying DM chat list item '{}' for account: {}",
            self.dm_group_context_name,
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let expected_group = context.get_group(&self.dm_group_context_name)?;
        let other_user_account = context.get_account(&self.other_user_account_name)?;
        let chat_list = context.whitenoise.get_chat_list(account).await?;

        let item = chat_list
            .iter()
            .find(|item| item.mls_group_id == expected_group.mls_group_id)
            .ok_or_else(|| {
                WhitenoiseError::Configuration(format!(
                    "DM group '{}' not found in chat list",
                    self.dm_group_context_name
                ))
            })?;

        // Verify it's a DM
        assert_eq!(
            item.group_type,
            GroupType::DirectMessage,
            "Expected DirectMessage type"
        );

        // DMs should not have group_image_path (they use profile picture URLs)
        assert!(
            item.group_image_path.is_none(),
            "DM should not have group_image_path"
        );

        // Verify the DM name comes from the other user's metadata (if set)
        let other_user_metadata = other_user_account.metadata(context.whitenoise).await?;
        let expected_name = other_user_metadata
            .display_name
            .filter(|s| !s.is_empty())
            .or(other_user_metadata.name.filter(|s| !s.is_empty()));

        assert_eq!(
            item.name, expected_name,
            "DM name should be other user's display name"
        );

        // Verify profile picture URL matches
        assert_eq!(
            item.group_image_url, other_user_metadata.picture,
            "DM profile picture URL should match other user's picture"
        );

        tracing::info!(
            "✓ DM chat list item '{}' verification passed",
            self.dm_group_context_name
        );
        Ok(())
    }
}
