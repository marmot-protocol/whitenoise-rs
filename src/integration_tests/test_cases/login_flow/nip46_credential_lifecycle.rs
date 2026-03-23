use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;

/// Verifies the NIP-46 credential lifecycle through an external signer account:
///
/// 1. Login with external signer and store NIP-46 credentials in the keychain.
/// 2. Verify credentials are retrievable from the keychain.
/// 3. Logout and verify credentials are cleaned up.
/// 4. Verify the reconnect-failed set is managed correctly.
pub struct Nip46CredentialLifecycleTestCase;

#[async_trait]
impl TestCase for Nip46CredentialLifecycleTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Testing NIP-46 credential lifecycle (store → retrieve → logout cleanup)");

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

        // Step 2: Store NIP-46 credentials (simulating what login_nip46 would do)
        let app_keys = Keys::generate();
        let bunker_uri = "bunker://0000000000000000000000000000000000000000000000000000000000000000?relay=wss://relay.nsec.app";
        context
            .whitenoise
            .secrets_store
            .store_nip46_credentials(&pubkey, app_keys.secret_key(), bunker_uri)?;

        // Verify credentials exist in keychain
        let (retrieved_secret, retrieved_uri) = context
            .whitenoise
            .secrets_store
            .get_nip46_credentials(&pubkey)?;
        assert_eq!(retrieved_uri, bunker_uri);
        assert_eq!(
            retrieved_secret.to_secret_hex(),
            app_keys.secret_key().to_secret_hex()
        );

        // Step 3: Simulate reconnect failure (e.g. bunker offline at startup)
        context
            .whitenoise
            .nip46_reconnect_failed
            .insert(pubkey);
        assert!(
            !context
                .whitenoise
                .nip46_reconnect_failed_accounts()
                .is_empty(),
            "Should have a reconnect-failed account"
        );

        // Step 4: Re-register signer clears reconnect failure
        context
            .whitenoise
            .register_external_signer(pubkey, keys.clone())
            .await?;
        assert!(
            !context
                .whitenoise
                .nip46_reconnect_failed_accounts()
                .contains(&pubkey),
            "Re-registration should clear the reconnect-failed flag"
        );

        // Step 5: Logout should clean up NIP-46 credentials
        context.whitenoise.logout(&pubkey).await?;

        let cred_result = context
            .whitenoise
            .secrets_store
            .get_nip46_credentials(&pubkey);
        assert!(
            cred_result.is_err(),
            "NIP-46 credentials should be cleaned up after logout"
        );

        tracing::info!("✓ NIP-46 credential lifecycle verified: store → retrieve → logout cleanup");
        Ok(())
    }
}
