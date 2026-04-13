use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use nostr_sdk::prelude::*;

pub struct DeleteMessageTestCase {
    deleter_account_name: String,
    group_name: String,
    target_message_id_key: String,
}

impl DeleteMessageTestCase {
    pub fn new(deleter_account_name: &str, group_name: &str, target_message_id_key: &str) -> Self {
        Self {
            deleter_account_name: deleter_account_name.to_string(),
            group_name: group_name.to_string(),
            target_message_id_key: target_message_id_key.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for DeleteMessageTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Deleting message {} from group {} using account: {}",
            self.target_message_id_key,
            self.group_name,
            self.deleter_account_name
        );

        let deleter_account = context.get_account(&self.deleter_account_name)?;
        let group = context.get_group(&self.group_name)?;
        let target_message_id = context.get_message_id(&self.target_message_id_key)?;

        // Create delete tags targeting the specific message
        let delete_tags =
            vec![Tag::parse(vec!["e", target_message_id]).map_err(|e| {
                WhitenoiseError::Internal(format!("Failed to create e-tag: {}", e))
            })?];

        // Send delete message (kind 5 with empty content)
        let delete_result = context
            .whitenoise
            .send_message_to_group(
                deleter_account,
                &group.mls_group_id,
                "".to_string(), // Empty content for deletion event
                5,              // Kind 5 for deletion
                Some(delete_tags),
            )
            .await?;

        // Store the delete message ID for reference
        context.add_message_id("delete_message", delete_result.message.id.to_string());

        tracing::info!(
            "✓ Delete message sent successfully for target message: {}",
            self.target_message_id_key
        );

        Ok(())
    }
}
