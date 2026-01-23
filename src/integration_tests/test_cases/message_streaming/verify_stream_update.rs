use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::message_streaming::{MessageUpdate, UpdateTrigger};
use async_trait::async_trait;
use tokio::sync::{Mutex, broadcast};

/// Test case that verifies a real-time stream update is received correctly.
///
/// This test case uses a two-phase approach since we need to:
/// 1. Subscribe to group messages FIRST to get a receiver
/// 2. Perform some action (send message, add reaction, etc.)
/// 3. Call `run()` (via `execute()`) to verify the update was received
///
/// Usage:
/// ```ignore
/// let verifier = VerifyStreamUpdateTestCase::new("group", UpdateTrigger::NewMessage);
/// verifier.subscribe(&context).await?;
/// // ... perform action that triggers the update ...
/// verifier.execute(&mut context).await?;
/// ```
pub struct VerifyStreamUpdateTestCase {
    group_name: String,
    expected_trigger: UpdateTrigger,
    expected_message_key: Option<String>,
    expected_deleted: bool,
    expected_has_reactions: Option<bool>,
    receiver: Mutex<Option<broadcast::Receiver<MessageUpdate>>>,
}

impl VerifyStreamUpdateTestCase {
    pub fn new(group_name: &str, expected_trigger: UpdateTrigger) -> Self {
        Self {
            group_name: group_name.to_string(),
            expected_trigger,
            expected_message_key: None,
            expected_deleted: false,
            expected_has_reactions: None,
            receiver: Mutex::new(None),
        }
    }

    /// The message key we expect in the update
    pub fn expect_message_key(mut self, key: &str) -> Self {
        self.expected_message_key = Some(key.to_string());
        self
    }

    /// Expect the message to be marked as deleted
    pub fn expect_deleted(mut self) -> Self {
        self.expected_deleted = true;
        self
    }

    /// Expect the message to have reactions (or not)
    pub fn expect_has_reactions(mut self, has: bool) -> Self {
        self.expected_has_reactions = Some(has);
        self
    }

    /// Subscribe to the group. Must be called before `execute()`.
    pub async fn subscribe(&self, context: &ScenarioContext) -> Result<(), WhitenoiseError> {
        let group = context.get_group(&self.group_name)?;
        let subscription = context
            .whitenoise
            .subscribe_to_group_messages(&group.mls_group_id)
            .await?;

        let mut guard = self.receiver.lock().await;
        *guard = Some(subscription.updates);

        tracing::info!(
            "Subscribed to group '{}', waiting for {:?} update",
            self.group_name,
            self.expected_trigger
        );
        Ok(())
    }
}

#[async_trait]
impl TestCase for VerifyStreamUpdateTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let mut guard = self.receiver.lock().await;
        let receiver = guard.as_mut().ok_or_else(|| {
            WhitenoiseError::Other(anyhow::anyhow!(
                "VerifyStreamUpdateTestCase: subscribe() must be called before run(). \
                 The scenario should first call subscribe(), then perform the action \
                 that triggers the update, then call execute()."
            ))
        })?;

        // Pre-resolve the expected message ID to avoid borrow issues in async block
        let expected_id = self
            .expected_message_key
            .as_ref()
            .and_then(|key| context.get_message_id(key).ok().cloned());

        let expected_trigger = self.expected_trigger.clone();

        // Wait for the expected update with timeout, skipping stale events that don't match
        // This handles race conditions where previous test updates arrive late
        let update = tokio::time::timeout(tokio::time::Duration::from_secs(5), async {
            loop {
                match receiver.recv().await {
                    Ok(update) => {
                        // Check if this is the expected message (if we have an expected ID)
                        let is_expected_message = expected_id
                            .as_ref()
                            .map(|id| update.message.id == *id)
                            .unwrap_or(true);

                        if update.trigger == expected_trigger && is_expected_message {
                            return Ok(update);
                        }

                        // Log skipped update for debugging
                        tracing::debug!(
                            "Skipping stale update: got {:?} for message {}, waiting for {:?}",
                            update.trigger,
                            update.message.id,
                            expected_trigger
                        );
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await
        .map_err(|_| {
            WhitenoiseError::Other(anyhow::anyhow!(
                "Timeout waiting for {:?} update",
                self.expected_trigger
            ))
        })?
        .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!("Failed to receive update: {}", e)))?;

        // Verify trigger type
        assert_eq!(
            update.trigger, self.expected_trigger,
            "Expected {:?} but got {:?}",
            self.expected_trigger, update.trigger
        );
        tracing::info!("✓ Received expected trigger: {:?}", update.trigger);

        // Verify message ID if expected
        if let Some(expected_key) = &self.expected_message_key {
            let expected_id = context.get_message_id(expected_key)?;
            assert_eq!(
                &update.message.id, expected_id,
                "Expected message '{}' but got different ID",
                expected_key
            );
            tracing::info!("✓ Message ID matches expected key '{}'", expected_key);
        }

        // Verify deleted status
        if self.expected_deleted {
            assert!(update.message.is_deleted, "Expected message to be deleted");
            tracing::info!("✓ Message is correctly marked as deleted");
        }

        // Verify reactions
        if let Some(has_reactions) = self.expected_has_reactions {
            let actual_has = !update.message.reactions.user_reactions.is_empty();
            assert_eq!(
                actual_has, has_reactions,
                "Expected has_reactions={} but got {}",
                has_reactions, actual_has
            );
            tracing::info!("✓ Reactions state matches expected (has={})", has_reactions);
        }

        Ok(())
    }
}
