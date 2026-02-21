use std::time::Duration;

use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;

/// Verifies that re-registering an external signer recovers account subscriptions
/// after a startup-like gap where signer and client subscription state were lost.
pub struct RegisterExternalSignerRecoversSubscriptionsTestCase;

#[async_trait]
impl TestCase for RegisterExternalSignerRecoversSubscriptionsTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Testing external signer re-registration subscription recovery");

        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Create an external account through the production login path.
        let account = context
            .whitenoise
            .login_with_external_signer(pubkey, keys.clone())
            .await?;
        assert!(
            account.uses_external_signer(),
            "Expected account to use external signer"
        );

        retry(
            60,
            Duration::from_millis(100),
            || async {
                let current = context.whitenoise.find_account_by_pubkey(&pubkey).await?;
                if context
                    .whitenoise
                    .is_account_subscriptions_operational(&current)
                    .await?
                {
                    Ok(())
                } else {
                    Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "External account subscriptions are not yet operational"
                    )))
                }
            },
            "wait initial external account subscriptions to become operational",
        )
        .await?;

        // Simulate startup gap:
        // - signer not yet registered on app restore
        // - nostr client starts with no active subscriptions.
        context.whitenoise.remove_external_signer(&pubkey);
        context.whitenoise.reset_nostr_client().await?;

        let current = context.whitenoise.find_account_by_pubkey(&pubkey).await?;
        let before = context
            .whitenoise
            .is_account_subscriptions_operational(&current)
            .await?;
        assert!(
            !before,
            "Account subscriptions should be non-operational before signer re-registration"
        );

        // Re-register signer for restored external account.
        context
            .whitenoise
            .register_external_signer(pubkey, keys)
            .await?;

        // Regression assertion: signer re-registration should recover subscriptions.
        retry(
            60,
            Duration::from_millis(100),
            || async {
                let refreshed = context.whitenoise.find_account_by_pubkey(&pubkey).await?;
                if context
                    .whitenoise
                    .is_account_subscriptions_operational(&refreshed)
                    .await?
                {
                    Ok(())
                } else {
                    Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "register_external_signer did not recover account subscriptions"
                    )))
                }
            },
            "wait account subscriptions to recover after signer re-registration",
        )
        .await?;

        tracing::info!("✓ external signer re-registration recovered subscriptions");
        Ok(())
    }
}
