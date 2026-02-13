use crate::integration_tests::core::*;
use crate::{LoginStatus, RelayType, WhitenoiseError};
use async_trait::async_trait;
use nostr_sdk::prelude::*;

/// Tests the happy path: relay lists exist on the network, so `login_start`
/// completes the full login in a single call.
pub struct LoginStartHappyPathTestCase {
    account_name: String,
}

impl LoginStartHappyPathTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for LoginStartHappyPathTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Testing login_start happy path for: {}", self.account_name);

        let keys = Keys::generate();
        let expected_pubkey = keys.public_key();

        // Publish relay lists via an external client so they exist on the network.
        let test_client = create_test_client(&context.dev_relays, keys.clone()).await?;
        let relay_urls: Vec<String> = context.dev_relays.iter().map(|s| s.to_string()).collect();
        publish_relay_lists(&test_client, relay_urls).await?;
        test_client.disconnect().await;

        // login_start should find the relay lists and return Complete.
        let result = context
            .whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .map_err(WhitenoiseError::Login)?;

        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(result.account.pubkey, expected_pubkey);

        // Verify relay lists were stored.
        let relays = result
            .account
            .relays(RelayType::Nip65, context.whitenoise)
            .await?;
        assert!(
            !relays.is_empty(),
            "Expected NIP-65 relays to be stored after login"
        );

        context.add_account(&self.account_name, result.account);
        tracing::info!("✓ login_start happy path completed");
        Ok(())
    }
}
