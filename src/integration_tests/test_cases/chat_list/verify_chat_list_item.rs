use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;

/// Verifies chat list item details for a specific group.
pub struct VerifyChatListItemTestCase {
    account_name: String,
    group_context_name: String,
    expected_name: Option<String>,
    expected_has_last_message: bool,
    expected_last_message_content: Option<String>,
    expected_pending_confirmation: Option<bool>,
    expected_welcomer_account: Option<String>,
    assert_no_welcomer: bool,
}

impl VerifyChatListItemTestCase {
    pub fn new(account_name: &str, group_context_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_context_name: group_context_name.to_string(),
            expected_name: None,
            expected_has_last_message: false,
            expected_last_message_content: None,
            expected_pending_confirmation: None,
            expected_welcomer_account: None,
            assert_no_welcomer: false,
        }
    }

    pub fn expecting_name(mut self, name: &str) -> Self {
        self.expected_name = Some(name.to_string());
        self
    }

    pub fn expecting_last_message(mut self, content: &str) -> Self {
        self.expected_has_last_message = true;
        self.expected_last_message_content = Some(content.to_string());
        self
    }

    pub fn expecting_no_last_message(mut self) -> Self {
        self.expected_has_last_message = false;
        self.expected_last_message_content = None;
        self
    }

    /// Verifies the pending_confirmation field matches the expected value.
    /// Use `expecting_not_pending()` for creator's groups (auto-accepted).
    pub fn expecting_pending_confirmation(mut self, pending: bool) -> Self {
        self.expected_pending_confirmation = Some(pending);
        self
    }

    /// Convenience method: verifies the group is NOT pending (i.e., accepted).
    /// Use this for groups created by the account being tested.
    pub fn expecting_not_pending(self) -> Self {
        self.expecting_pending_confirmation(false)
    }

    /// Verifies the welcomer_pubkey matches the pubkey of the specified account.
    /// Use this for invited members to verify who invited them.
    pub fn expecting_welcomer(mut self, welcomer_account_name: &str) -> Self {
        self.expected_welcomer_account = Some(welcomer_account_name.to_string());
        self.assert_no_welcomer = false;
        self
    }

    /// Verifies there is no welcomer (i.e., creator's own group).
    /// Use this for groups created by the account being tested.
    pub fn expecting_no_welcomer(mut self) -> Self {
        self.assert_no_welcomer = true;
        self.expected_welcomer_account = None;
        self
    }
}

#[async_trait]
impl TestCase for VerifyChatListItemTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Verifying chat list item '{}' for account: {}",
            self.group_context_name,
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let expected_group = context.get_group(&self.group_context_name)?;
        let chat_list = context.whitenoise.get_chat_list(account).await?;

        let item = chat_list
            .iter()
            .find(|item| item.mls_group_id == expected_group.mls_group_id)
            .ok_or_else(|| {
                WhitenoiseError::Configuration(format!(
                    "Group '{}' not found in chat list",
                    self.group_context_name
                ))
            })?;

        // Verify name
        if let Some(expected_name) = &self.expected_name {
            assert_eq!(
                item.name.as_ref(),
                Some(expected_name),
                "Expected name '{}' but got {:?}",
                expected_name,
                item.name
            );
        }

        // Verify last message
        if self.expected_has_last_message {
            assert!(
                item.last_message.is_some(),
                "Expected last message but found none"
            );
            if let Some(expected_content) = &self.expected_last_message_content {
                let actual_content = &item.last_message.as_ref().unwrap().content;
                assert_eq!(
                    actual_content, expected_content,
                    "Expected last message content '{}' but got '{}'",
                    expected_content, actual_content
                );
            }
        } else {
            assert!(
                item.last_message.is_none(),
                "Expected no last message but found: {:?}",
                item.last_message
            );
        }

        // Verify pending_confirmation
        if let Some(expected_pending) = self.expected_pending_confirmation {
            assert_eq!(
                item.pending_confirmation, expected_pending,
                "Expected pending_confirmation={} but got {}",
                expected_pending, item.pending_confirmation
            );
        }

        // Verify welcomer_pubkey
        if self.assert_no_welcomer {
            assert!(
                item.welcomer_pubkey.is_none(),
                "Expected no welcomer but got {:?}",
                item.welcomer_pubkey
            );
        } else if let Some(welcomer_account_name) = &self.expected_welcomer_account {
            let welcomer_account = context.get_account(welcomer_account_name)?;
            assert_eq!(
                item.welcomer_pubkey,
                Some(welcomer_account.pubkey),
                "Expected welcomer_pubkey to be {}'s pubkey but got {:?}",
                welcomer_account_name,
                item.welcomer_pubkey
            );
        }

        tracing::info!(
            "✓ Chat list item '{}' verification passed",
            self.group_context_name
        );
        Ok(())
    }
}
