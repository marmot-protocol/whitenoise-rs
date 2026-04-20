use async_trait::async_trait;
use nostr_sdk::PublicKey;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;

pub struct UnfollowUserTestCase {
    follower_account_name: String,
    target_pubkey: PublicKey,
}

impl UnfollowUserTestCase {
    pub fn new(follower_account_name: &str, target_pubkey: PublicKey) -> Self {
        Self {
            follower_account_name: follower_account_name.to_string(),
            target_pubkey,
        }
    }
}

#[async_trait]
impl TestCase for UnfollowUserTestCase {
    #[allow(deprecated)]
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Unfollowing user {} from account: {}",
            &self.target_pubkey.to_hex()[..8],
            self.follower_account_name
        );

        let account = context.get_account(&self.follower_account_name)?;

        // Perform the unfollow operation
        context
            .whitenoise
            .unfollow_user(account, &self.target_pubkey)
            .await?;

        // Wait for unfollow operation to be reflected in the system
        retry_default(
            || async {
                let is_following = context
                    .whitenoise
                    .is_following_user(account, &self.target_pubkey)
                    .await?;

                if !is_following {
                    Ok(())
                } else {
                    Err(WhitenoiseError::Internal(
                        "Follow relationship still exists after unfollow".to_string(),
                    ))
                }
            },
            &format!(
                "verify unfollow for user {}",
                &self.target_pubkey.to_hex()[..8]
            ),
        )
        .await?;

        tracing::info!(
            "✓ Account {} is no longer following user {}",
            self.follower_account_name,
            &self.target_pubkey.to_hex()[..8]
        );

        Ok(())
    }
}
