use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::chat_list_streaming::{ChatListUpdate, ChatListUpdateTrigger};
use async_trait::async_trait;
use tokio::sync::{Mutex, broadcast};

/// Test case that verifies a real-time chat list update is received correctly.
///
/// This test case uses a two-phase approach since we need to:
/// 1. Subscribe to chat list updates FIRST to get a receiver
/// 2. Perform some action (create group, send message, delete message, etc.)
/// 3. Call `run()` (via `execute()`) to verify the update was received
///
/// Usage:
/// ```ignore
/// let verifier = VerifyChatListUpdateTestCase::new("account", ChatListUpdateTrigger::NewLastMessage);
/// verifier.subscribe(&context).await?;
/// // ... perform action that triggers the update ...
/// verifier.execute(&mut context).await?;
/// ```
pub struct VerifyChatListUpdateTestCase {
    account_name: String,
    expected_trigger: ChatListUpdateTrigger,
    expected_group_name: Option<String>,
    expected_last_message_content: Option<String>,
    expected_no_last_message: bool,
    receiver: Mutex<Option<broadcast::Receiver<ChatListUpdate>>>,
}

impl VerifyChatListUpdateTestCase {
    pub fn new(account_name: &str, expected_trigger: ChatListUpdateTrigger) -> Self {
        Self {
            account_name: account_name.to_string(),
            expected_trigger,
            expected_group_name: None,
            expected_last_message_content: None,
            expected_no_last_message: false,
            receiver: Mutex::new(None),
        }
    }

    /// The group name we expect in the update
    pub fn expect_group_name(mut self, name: &str) -> Self {
        self.expected_group_name = Some(name.to_string());
        self
    }

    /// Expect the chat list item to have a last message with this content
    pub fn expect_last_message_content(mut self, content: &str) -> Self {
        self.expected_last_message_content = Some(content.to_string());
        self
    }

    /// Expect the chat list item to have no last message
    pub fn expect_no_last_message(mut self) -> Self {
        self.expected_no_last_message = true;
        self
    }

    /// Subscribe to the chat list. Must be called before `execute()`.
    pub async fn subscribe(&self, context: &ScenarioContext) -> Result<(), WhitenoiseError> {
        let account = context.get_account(&self.account_name)?;
        let subscription = context.whitenoise.subscribe_to_chat_list(account).await?;

        let mut guard = self.receiver.lock().await;
        *guard = Some(subscription.updates);

        tracing::info!(
            "Subscribed to chat list for '{}', waiting for {:?} update",
            self.account_name,
            self.expected_trigger
        );
        Ok(())
    }
}

#[async_trait]
impl TestCase for VerifyChatListUpdateTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let mut guard = self.receiver.lock().await;
        let receiver = guard.as_mut().ok_or_else(|| {
            WhitenoiseError::Other(anyhow::anyhow!(
                "VerifyChatListUpdateTestCase: subscribe() must be called before run(). \
                 The scenario should first call subscribe(), then perform the action \
                 that triggers the update, then call execute()."
            ))
        })?;

        // Wait for the update with timeout
        let update = tokio::time::timeout(tokio::time::Duration::from_secs(10), receiver.recv())
            .await
            .map_err(|_| {
                WhitenoiseError::Other(anyhow::anyhow!(
                    "Timeout waiting for {:?} update",
                    self.expected_trigger
                ))
            })?
            .map_err(|e| {
                WhitenoiseError::Other(anyhow::anyhow!("Failed to receive update: {}", e))
            })?;

        // Verify trigger type
        assert_eq!(
            update.trigger, self.expected_trigger,
            "Expected {:?} but got {:?}",
            self.expected_trigger, update.trigger
        );
        tracing::info!("✓ Received expected trigger: {:?}", update.trigger);

        // Verify group name if expected
        if let Some(expected_name) = &self.expected_group_name {
            let expected_group = context.get_group(expected_name)?;
            assert_eq!(
                update.item.mls_group_id, expected_group.mls_group_id,
                "Expected group '{}' but got different group ID",
                expected_name
            );
            tracing::info!("✓ Group ID matches expected group '{}'", expected_name);
        }

        // Verify last message content if expected
        if let Some(expected_content) = &self.expected_last_message_content {
            let last_msg = update.item.last_message.as_ref().ok_or_else(|| {
                WhitenoiseError::Other(anyhow::anyhow!(
                    "Expected last message with content '{}' but no last message in update",
                    expected_content
                ))
            })?;
            assert_eq!(
                &last_msg.content, expected_content,
                "Expected last message content '{}' but got '{}'",
                expected_content, last_msg.content
            );
            tracing::info!("✓ Last message content matches: '{}'", expected_content);
        }

        // Verify no last message if expected
        if self.expected_no_last_message {
            assert!(
                update.item.last_message.is_none(),
                "Expected no last message but found one"
            );
            tracing::info!("✓ Correctly has no last message");
        }

        Ok(())
    }
}
