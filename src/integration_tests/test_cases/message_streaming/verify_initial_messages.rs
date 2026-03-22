use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use std::collections::HashSet;

/// Test case that subscribes to group messages and verifies the initial snapshot
/// contains all expected messages with correct state.
pub struct VerifyInitialMessagesTestCase {
    group_name: String,
    expected_message_keys: Vec<String>,
    expected_with_reactions: Vec<String>,
    expected_no_reactions: Vec<String>,
    expected_deleted: Vec<String>,
}

impl VerifyInitialMessagesTestCase {
    pub fn new(group_name: &str) -> Self {
        Self {
            group_name: group_name.to_string(),
            expected_message_keys: vec![],
            expected_with_reactions: vec![],
            expected_no_reactions: vec![],
            expected_deleted: vec![],
        }
    }

    pub fn expect_messages(mut self, keys: Vec<&str>) -> Self {
        self.expected_message_keys = keys.into_iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn expect_with_reactions(mut self, keys: Vec<&str>) -> Self {
        self.expected_with_reactions = keys.into_iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn expect_no_reactions(mut self, keys: Vec<&str>) -> Self {
        self.expected_no_reactions = keys.into_iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn expect_deleted(mut self, keys: Vec<&str>) -> Self {
        self.expected_deleted = keys.into_iter().map(|s| s.to_string()).collect();
        self
    }
}

#[async_trait]
impl TestCase for VerifyInitialMessagesTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Verifying initial messages for group '{}' via subscription",
            self.group_name
        );

        let group = context.get_group(&self.group_name)?;

        let subscription = context
            .whitenoise
            .subscribe_to_group_messages(&group.mls_group_id, None)
            .await?;

        tracing::info!(
            "Received {} initial messages",
            subscription.initial_messages.len()
        );

        // Build actual message IDs set
        let actual_ids: HashSet<String> = subscription
            .initial_messages
            .iter()
            .map(|m| m.id.clone())
            .collect();

        // Verify all expected messages are present
        for key in &self.expected_message_keys {
            let msg_id = context.get_message_id(key)?;
            assert!(
                actual_ids.contains(msg_id),
                "Expected message '{}' (ID: {}) not found in initial messages",
                key,
                msg_id
            );
        }

        // Verify messages with reactions
        for key in &self.expected_with_reactions {
            let msg_id = context.get_message_id(key)?;
            let msg = subscription
                .initial_messages
                .iter()
                .find(|m| &m.id == msg_id)
                .ok_or_else(|| {
                    WhitenoiseError::Other(anyhow::anyhow!(
                        "Message '{}' not found for reaction check",
                        key
                    ))
                })?;

            assert!(
                !msg.reactions.user_reactions.is_empty(),
                "Expected message '{}' to have reactions, but found none",
                key
            );
            tracing::info!(
                "✓ Message '{}' has {} reaction(s)",
                key,
                msg.reactions.user_reactions.len()
            );
        }

        // Verify messages without reactions
        for key in &self.expected_no_reactions {
            let msg_id = context.get_message_id(key)?;
            let msg = subscription
                .initial_messages
                .iter()
                .find(|m| &m.id == msg_id)
                .ok_or_else(|| {
                    WhitenoiseError::Other(anyhow::anyhow!(
                        "Message '{}' not found for no-reaction check",
                        key
                    ))
                })?;

            assert!(
                msg.reactions.user_reactions.is_empty(),
                "Expected message '{}' to have no reactions, but found {}",
                key,
                msg.reactions.user_reactions.len()
            );
            tracing::info!("✓ Message '{}' correctly has no reactions", key);
        }

        // Verify deleted messages are NOT in the initial snapshot
        for key in &self.expected_deleted {
            let msg_id = context.get_message_id(key)?;
            let found = subscription
                .initial_messages
                .iter()
                .any(|m| &m.id == msg_id);

            assert!(
                !found,
                "Expected message '{}' to be excluded from initial snapshot because it is deleted, but it was found",
                key
            );
            tracing::info!(
                "✓ Message '{}' is correctly excluded from the initial snapshot",
                key
            );
        }

        tracing::info!(
            "✓ Verified {} initial messages ({} with reactions, {} without reactions, {} deleted)",
            subscription.initial_messages.len(),
            self.expected_with_reactions.len(),
            self.expected_no_reactions.len(),
            self.expected_deleted.len()
        );

        Ok(())
    }
}
