use async_trait::async_trait;
use nostr_sdk::PublicKey;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;

/// Unblocks `target_pubkey` from `account_name`'s mute list.
///
/// Exercises `session.mute_list().unblock_user(...)`: local cache delete +
/// NIP-51 republish without the entry. Reaches the rollback path only if
/// publish fails, which the happy-path scenario doesn't trigger.
pub struct UnblockUserTestCase {
    account_name: String,
    target_pubkey: PublicKey,
}

impl UnblockUserTestCase {
    pub fn new(account_name: &str, target_pubkey: PublicKey) -> Self {
        Self {
            account_name: account_name.to_string(),
            target_pubkey,
        }
    }
}

#[async_trait]
impl TestCase for UnblockUserTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Unblocking user {} from account: {}",
            &self.target_pubkey.to_hex()[..8],
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let session = context.whitenoise.require_session(&account.pubkey)?;

        session
            .mute_list()
            .unblock_user(&self.target_pubkey)
            .await?;

        retry_default(
            || async {
                let blocked = session
                    .mute_list()
                    .is_user_blocked(&self.target_pubkey)
                    .await?;
                if blocked {
                    Err(WhitenoiseError::Internal(
                        "unblock_user did not clear the local cache entry yet".to_string(),
                    ))
                } else {
                    Ok(())
                }
            },
            &format!(
                "verify unblock applied for user {}",
                &self.target_pubkey.to_hex()[..8]
            ),
        )
        .await?;

        tracing::info!(
            "✓ Account {} unblocked user {}",
            self.account_name,
            &self.target_pubkey.to_hex()[..8]
        );

        Ok(())
    }
}
