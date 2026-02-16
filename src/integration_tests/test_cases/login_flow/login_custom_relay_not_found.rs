use async_trait::async_trait;
use nostr_sdk::prelude::*;

use crate::integration_tests::core::*;
use crate::{LoginStatus, WhitenoiseError};

/// Tests that `login_with_custom_relay` returns `NeedsRelayLists` when the
/// custom relay does not have the user's relay lists. The user can then
/// retry with a different relay or choose to publish defaults.
pub struct LoginCustomRelayNotFoundTestCase {
    account_name: String,
}

impl LoginCustomRelayNotFoundTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for LoginCustomRelayNotFoundTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Testing login_with_custom_relay (not found) for: {}",
            self.account_name
        );

        // Generate a brand new key with nothing published anywhere.
        let keys = Keys::generate();
        let expected_pubkey = keys.public_key();

        // Step 1: login_start should return NeedsRelayLists.
        let start_result = context
            .whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .map_err(WhitenoiseError::Login)?;

        assert_eq!(
            start_result.status,
            LoginStatus::NeedsRelayLists,
            "Expected NeedsRelayLists for a brand new key"
        );

        // Step 2: Try a custom relay -- lists won't be there either since
        // we never published anything.
        let custom_relay_url = RelayUrl::parse(context.dev_relays[0])?;
        let result = context
            .whitenoise
            .login_with_custom_relay(&expected_pubkey, custom_relay_url)
            .await
            .map_err(WhitenoiseError::Login)?;

        assert_eq!(
            result.status,
            LoginStatus::NeedsRelayLists,
            "Expected NeedsRelayLists when custom relay has no lists"
        );
        assert_eq!(result.account.pubkey, expected_pubkey);

        // The account should still be in partial state, ready for another
        // attempt or a cancel.
        context.add_account(&self.account_name, result.account);
        tracing::info!("✓ login_with_custom_relay correctly returned NeedsRelayLists");
        Ok(())
    }
}
