use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use nostr_sdk::EventId;

/// Marks a message as read for an account.
pub struct MarkMessageReadTestCase {
    account_name: String,
    message_id_key: String,
}

impl MarkMessageReadTestCase {
    pub fn new(account_name: &str, message_id_key: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            message_id_key: message_id_key.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for MarkMessageReadTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Marking message '{}' as read for account: {}",
            self.message_id_key,
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let message_id_hex = context.get_message_id(&self.message_id_key)?;
        let message_id = EventId::from_hex(message_id_hex).map_err(|e| {
            WhitenoiseError::Configuration(format!(
                "Invalid message ID '{}': {}",
                message_id_hex, e
            ))
        })?;

        let updated_account_group = context
            .whitenoise
            .mark_message_read(account, &message_id)
            .await?;

        // Verify the last_read_message_id was actually updated
        assert_eq!(
            updated_account_group.last_read_message_id,
            Some(message_id),
            "Expected last_read_message_id to be set to {}",
            message_id
        );

        tracing::info!(
            "✓ Marked message '{}' as read for {} (last_read_message_id verified)",
            self.message_id_key,
            self.account_name
        );
        Ok(())
    }
}
