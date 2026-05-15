use async_trait::async_trait;
use nostr_sdk::PublicKey;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;

/// Blocks `target_pubkey` from `account_name`'s mute list.
///
/// Exercises `session.mute_list().block_user(...)` end-to-end: local cache
/// insert + NIP-51 kind-10000 publish to the account's relays. The retry
/// waits for the local-cache write to be observable.
pub struct BlockUserTestCase {
    account_name: String,
    target_pubkey: PublicKey,
}

impl BlockUserTestCase {
    pub fn new(account_name: &str, target_pubkey: PublicKey) -> Self {
        Self {
            account_name: account_name.to_string(),
            target_pubkey,
        }
    }
}

#[async_trait]
impl TestCase for BlockUserTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Blocking user {} from account: {}",
            &self.target_pubkey.to_hex()[..8],
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let session = context.whitenoise.require_session(&account.pubkey)?;

        session.mute_list().block_user(&self.target_pubkey).await?;

        retry_default(
            || async {
                let blocked = session
                    .mute_list()
                    .is_user_blocked(&self.target_pubkey)
                    .await?;
                if blocked {
                    Ok(())
                } else {
                    Err(WhitenoiseError::Internal(
                        "block_user did not produce a local cache entry yet".to_string(),
                    ))
                }
            },
            &format!(
                "verify block applied for user {}",
                &self.target_pubkey.to_hex()[..8]
            ),
        )
        .await?;

        tracing::info!(
            "✓ Account {} blocked user {}",
            self.account_name,
            &self.target_pubkey.to_hex()[..8]
        );

        Ok(())
    }
}
