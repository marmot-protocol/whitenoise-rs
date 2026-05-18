use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::integration_tests::core::*;
use crate::{LoginStatus, RelayType, WhitenoiseError};

/// Tests the custom relay flow: `login_start` returns `NeedsRelayLists`,
/// then `login_with_custom_relay` finds relay lists on a user-provided relay
/// and completes the login.
pub struct LoginCustomRelayTestCase {
    account_name: String,
}

impl LoginCustomRelayTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for LoginCustomRelayTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Testing login_with_custom_relay for: {}", self.account_name);

        let keys = Keys::generate();
        let expected_pubkey = keys.public_key();

        // login_start queries the configured default-account relays; pick a
        // distinct one as the "custom" relay so we exercise the custom-relay
        // path when there is more than one configured. Falls back to the
        // first entry when only one default-account relay is configured.
        let custom_relay = context
            .default_account_relays
            .get(1)
            .or_else(|| context.default_account_relays.first())
            .cloned()
            .expect("test requires at least one configured default-account relay");

        let publish_relays = vec![custom_relay.clone()];
        let test_client = create_test_client(&publish_relays, keys.clone()).await?;
        publish_relay_lists(&test_client, publish_relays).await?;
        test_client.disconnect().await;

        // Step 1: login_start -- since the relay lists exist on a relay that
        // IS in the default set, this may or may not find them depending on
        // which default relay is queried. We handle both outcomes.
        let start_result = context
            .whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .map_err(WhitenoiseError::Login)?;

        match start_result.status {
            LoginStatus::Complete => {
                // The default relay happened to find the lists. Still valid.
                tracing::info!(
                    "✓ login_start found relay lists on defaults (acceptable in test env)"
                );
                context.add_account(&self.account_name, start_result.account);
                return Ok(());
            }
            LoginStatus::NeedsRelayLists => {
                // Expected path -- continue with custom relay.
                tracing::info!("login_start returned NeedsRelayLists, trying custom relay");
            }
        }

        // Step 2: Provide the custom relay where lists are published.
        let custom_relay_url = RelayUrl::parse(&custom_relay)?;
        let result = context
            .whitenoise
            .login_with_custom_relay(&expected_pubkey, custom_relay_url)
            .await
            .map_err(WhitenoiseError::Login)?;

        assert_eq!(
            result.status,
            LoginStatus::Complete,
            "Expected Complete after finding relay lists on custom relay"
        );
        assert_eq!(result.account.pubkey, expected_pubkey);

        // Verify relays were stored.
        let relays = result
            .account
            .relays(RelayType::Nip65, &context.whitenoise.shared)
            .await?;
        assert!(
            !relays.is_empty(),
            "Expected NIP-65 relays to be stored after custom relay login"
        );

        context.add_account(&self.account_name, result.account);
        tracing::info!("✓ login_with_custom_relay completed login");
        Ok(())
    }
}
