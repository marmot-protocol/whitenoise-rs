use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;

/// Test case to verify that after deleting a reaction, the parent message
/// no longer shows that reaction in its aggregated reaction summary.
pub struct VerifyReactionDeletionTestCase {
    account_name: String,
    group_name: String,
    target_message_id_key: String,
    reactor_pubkey_account: String,
}

impl VerifyReactionDeletionTestCase {
    pub fn new(
        account_name: &str,
        group_name: &str,
        target_message_id_key: &str,
        reactor_pubkey_account: &str,
    ) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
            target_message_id_key: target_message_id_key.to_string(),
            reactor_pubkey_account: reactor_pubkey_account.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for VerifyReactionDeletionTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Verifying reaction deletion for message {} in group {}",
            self.target_message_id_key,
            self.group_name
        );

        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;
        let target_message_id = context.get_message_id(&self.target_message_id_key)?;
        let reactor_account = context.get_account(&self.reactor_pubkey_account)?;

        // Retry until the reaction is removed (deletion event needs time to propagate)
        retry(
            20,
            std::time::Duration::from_millis(100),
            || async {
                let aggregated_messages = context
                    .whitenoise
                    .fetch_aggregated_messages_for_group(&account.pubkey, &group.mls_group_id)
                    .await?;

                let target_message = aggregated_messages
                    .iter()
                    .find(|msg| msg.id == *target_message_id)
                    .ok_or_else(|| {
                        WhitenoiseError::Other(anyhow::anyhow!(
                            "Target message {} not found in aggregated messages",
                            target_message_id
                        ))
                    })?;

                let reactor_has_reaction = target_message
                    .reactions
                    .user_reactions
                    .iter()
                    .any(|r| r.user == reactor_account.pubkey);

                if reactor_has_reaction {
                    Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "Reaction from {} still present on message {}",
                        &reactor_account.pubkey.to_hex()[..8],
                        target_message_id
                    )))
                } else {
                    Ok(())
                }
            },
            &format!(
                "verify reaction from {} deleted on message {}",
                &reactor_account.pubkey.to_hex()[..8],
                target_message_id
            ),
        )
        .await?;

        tracing::info!(
            "✓ Verified: No reaction from {} on message {} after deletion",
            &reactor_account.pubkey.to_hex()[..8],
            target_message_id
        );

        Ok(())
    }
}
