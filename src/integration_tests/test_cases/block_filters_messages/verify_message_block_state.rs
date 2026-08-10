use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::database::aggregated_messages::PaginationOptions;

/// Asserts the `is_blocked` stamp on a single message, as seen from one
/// account's own aggregated-message view.
///
/// The message is identified by the context key it was stored under (via
/// `SendMessageTestCase::with_message_id_key`). Because message delivery is
/// asynchronous, the fetch is retried until the account has processed the
/// message; the test then fails if the stamp does not match
/// `expected_is_blocked`.
pub struct VerifyMessageBlockStateTestCase {
    account_name: String,
    group_name: String,
    message_id_key: String,
    expected_is_blocked: bool,
}

impl VerifyMessageBlockStateTestCase {
    pub fn new(
        account_name: &str,
        group_name: &str,
        message_id_key: &str,
        expected_is_blocked: bool,
    ) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
            message_id_key: message_id_key.to_string(),
            expected_is_blocked,
        }
    }
}

#[async_trait]
impl TestCase for VerifyMessageBlockStateTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Verifying is_blocked = {} for message '{}' in group {} (account: {})",
            self.expected_is_blocked,
            self.message_id_key,
            self.group_name,
            self.account_name,
        );

        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;
        let message_id = context.get_message_id(&self.message_id_key)?.clone();

        // Message delivery is asynchronous: retry until the account has
        // processed the message, then read its frozen-at-ingest stamp.
        let observed = retry(
            20,
            std::time::Duration::from_millis(150),
            || async {
                let messages = context
                    .whitenoise
                    .fetch_aggregated_messages_for_group(
                        &account.pubkey,
                        &group.mls_group_id,
                        &PaginationOptions::default(),
                        Some(200),
                    )
                    .await?;
                match messages.iter().find(|m| m.id == message_id) {
                    Some(message) => Ok(message.is_blocked),
                    None => Err(WhitenoiseError::Internal(format!(
                        "account '{}' has not yet processed message '{}'",
                        self.account_name, self.message_id_key,
                    ))),
                }
            },
            &format!(
                "account '{}' receives message '{}'",
                self.account_name, self.message_id_key,
            ),
        )
        .await?;

        if observed != self.expected_is_blocked {
            return Err(WhitenoiseError::Internal(format!(
                "message '{}': expected is_blocked = {}, got {}",
                self.message_id_key, self.expected_is_blocked, observed,
            )));
        }

        tracing::info!(
            "✓ message '{}': is_blocked = {}",
            self.message_id_key,
            observed,
        );
        Ok(())
    }
}
