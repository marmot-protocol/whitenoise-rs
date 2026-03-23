use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;

/// Verifies the NIP-46 external signer account lifecycle through public APIs:
///
/// 1. Login with external signer creates an external account.
/// 2. External account has a signer registered and is retrievable.
/// 3. Reconnect-failed list is initially empty for healthy accounts.
/// 4. Re-registering a signer keeps the account healthy.
/// 5. Logout removes the account.
pub struct Nip46CredentialLifecycleTestCase;

#[async_trait]
impl TestCase for Nip46CredentialLifecycleTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Testing NIP-46 external signer account lifecycle via public APIs");

        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Step 1: Create an external account
        let account = context
            .whitenoise
            .login_with_external_signer(pubkey, keys.clone())
            .await?;
        assert!(
            account.uses_external_signer(),
            "Expected account to use external signer"
        );

        // Step 2: Verify the account is findable and has signer registered
        let found = context.whitenoise.find_account_by_pubkey(&pubkey).await?;
        assert_eq!(found.pubkey, pubkey);
        assert!(found.uses_external_signer());

        // Step 3: Reconnect-failed list should not contain this healthy account
        let failed = context.whitenoise.nip46_reconnect_failed_accounts();
        assert!(
            !failed.contains(&pubkey),
            "Healthy account should not be in reconnect-failed list"
        );

        // Step 4: Re-registering the signer should succeed and keep account healthy
        context
            .whitenoise
            .register_external_signer(pubkey, keys.clone())
            .await?;
        let still_failed = context.whitenoise.nip46_reconnect_failed_accounts();
        assert!(
            !still_failed.contains(&pubkey),
            "Re-registered account should not be in reconnect-failed list"
        );

        // Step 5: Logout should remove the account
        context.whitenoise.logout(&pubkey).await?;
        let find_result = context.whitenoise.find_account_by_pubkey(&pubkey).await;
        assert!(
            find_result.is_err(),
            "Account should not be findable after logout"
        );

        tracing::info!(
            "✓ NIP-46 external signer lifecycle verified: login → signer → re-register → logout"
        );
        Ok(())
    }
}
