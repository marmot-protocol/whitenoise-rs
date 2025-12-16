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
}

impl VerifyChatListItemTestCase {
    pub fn new(account_name: &str, group_context_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_context_name: group_context_name.to_string(),
            expected_name: None,
            expected_has_last_message: false,
            expected_last_message_content: None,
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

        tracing::info!(
            "✓ Chat list item '{}' verification passed",
            self.group_context_name
        );
        Ok(())
    }
}
