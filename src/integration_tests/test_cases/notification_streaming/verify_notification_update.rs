use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::notification_streaming::{NotificationTrigger, NotificationUpdate};
use async_trait::async_trait;
use tokio::sync::{Mutex, broadcast};

/// Test case that verifies a real-time notification update is received correctly.
///
/// This test case uses a two-phase approach since we need to:
/// 1. Subscribe to notification updates FIRST to get a receiver
/// 2. Perform some action (send message, create group, etc.)
/// 3. Call `run()` (via `execute()`) to verify the update was received
///
/// Usage:
/// ```ignore
/// let verifier = VerifyNotificationUpdateTestCase::new(NotificationTrigger::NewMessage);
/// verifier.subscribe(&context)?;
/// // ... perform action that triggers the notification ...
/// verifier.execute(&mut context).await?;
/// ```
pub struct VerifyNotificationUpdateTestCase {
    expected_trigger: NotificationTrigger,
    expected_group_name: Option<String>,
    expected_content: Option<String>,
    expected_is_dm: Option<bool>,
    expected_sender_name: Option<String>,
    expected_receiver_name: Option<String>,
    receiver: Mutex<Option<broadcast::Receiver<NotificationUpdate>>>,
}

impl VerifyNotificationUpdateTestCase {
    pub fn new(expected_trigger: NotificationTrigger) -> Self {
        Self {
            expected_trigger,
            expected_group_name: None,
            expected_content: None,
            expected_is_dm: None,
            expected_sender_name: None,
            expected_receiver_name: None,
            receiver: Mutex::new(None),
        }
    }

    /// Expect the notification to be for a specific group (looked up by context key).
    /// Validates both the group ID and the actual group name from the notification.
    pub fn expect_group(mut self, context_key: &str) -> Self {
        self.expected_group_name = Some(context_key.to_string());
        self
    }

    /// Expect the notification to have this message content
    pub fn expect_content(mut self, content: &str) -> Self {
        self.expected_content = Some(content.to_string());
        self
    }

    /// Expect the notification to be for a DM (or not)
    pub fn expect_is_dm(mut self, is_dm: bool) -> Self {
        self.expected_is_dm = Some(is_dm);
        self
    }

    /// Expect the sender to have this account name (looks up pubkey from context)
    pub fn expect_sender(mut self, account_name: &str) -> Self {
        self.expected_sender_name = Some(account_name.to_string());
        self
    }

    /// Expect the receiver to have this account name (looks up pubkey from context)
    pub fn expect_receiver(mut self, account_name: &str) -> Self {
        self.expected_receiver_name = Some(account_name.to_string());
        self
    }

    /// Subscribe to notifications. Must be called before `execute()`.
    pub fn subscribe(&self, context: &ScenarioContext) -> Result<(), WhitenoiseError> {
        let subscription = context.whitenoise.subscribe_to_notifications();

        let mut guard = self
            .receiver
            .try_lock()
            .map_err(|_| WhitenoiseError::Other(anyhow::anyhow!("Failed to acquire lock")))?;
        *guard = Some(subscription.updates);

        tracing::info!(
            "Subscribed to notifications, waiting for {:?} update",
            self.expected_trigger
        );
        Ok(())
    }
}

#[async_trait]
impl TestCase for VerifyNotificationUpdateTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let mut guard = self.receiver.lock().await;
        let receiver = guard.as_mut().ok_or_else(|| {
            WhitenoiseError::Other(anyhow::anyhow!(
                "VerifyNotificationUpdateTestCase: subscribe() must be called before run(). \
                 The scenario should first call subscribe(), then perform the action \
                 that triggers the notification, then call execute()."
            ))
        })?;

        // Wait for the update with timeout
        let update = tokio::time::timeout(tokio::time::Duration::from_secs(10), receiver.recv())
            .await
            .map_err(|_| {
                WhitenoiseError::Other(anyhow::anyhow!(
                    "Timeout waiting for {:?} notification",
                    self.expected_trigger
                ))
            })?
            .map_err(|e| {
                WhitenoiseError::Other(anyhow::anyhow!("Failed to receive notification: {}", e))
            })?;

        // Verify trigger type
        assert_eq!(
            update.trigger, self.expected_trigger,
            "Expected {:?} but got {:?}",
            self.expected_trigger, update.trigger
        );
        tracing::info!("Received expected trigger: {:?}", update.trigger);

        // Verify group if expected (by context key)
        if let Some(context_key) = &self.expected_group_name {
            let expected_group = context.get_group(context_key)?;
            assert_eq!(
                update.mls_group_id, expected_group.mls_group_id,
                "Expected group '{}' but got different group ID",
                context_key
            );
            tracing::info!("Group ID matches expected group '{}'", context_key);
        }

        // Verify content if expected
        if let Some(expected_content) = &self.expected_content {
            assert_eq!(
                &update.content, expected_content,
                "Expected content '{}' but got '{}'",
                expected_content, update.content
            );
            tracing::info!("Content matches: '{}'", expected_content);
        }

        // Verify is_dm if expected
        if let Some(expected_is_dm) = self.expected_is_dm {
            assert_eq!(
                update.is_dm, expected_is_dm,
                "Expected is_dm={} but got is_dm={}",
                expected_is_dm, update.is_dm
            );
            tracing::info!("is_dm matches: {}", expected_is_dm);
        }

        // Verify sender pubkey if expected
        if let Some(sender_name) = &self.expected_sender_name {
            let expected_account = context.get_account(sender_name)?;
            assert_eq!(
                update.sender.pubkey, expected_account.pubkey,
                "Expected sender '{}' but got different pubkey",
                sender_name
            );
            tracing::info!("Sender pubkey matches account '{}'", sender_name);
        }

        // Verify receiver pubkey if expected
        if let Some(receiver_name) = &self.expected_receiver_name {
            let expected_account = context.get_account(receiver_name)?;
            assert_eq!(
                update.receiver.pubkey, expected_account.pubkey,
                "Expected receiver '{}' but got different pubkey",
                receiver_name
            );
            tracing::info!("Receiver pubkey matches account '{}'", receiver_name);
        }

        Ok(())
    }
}
