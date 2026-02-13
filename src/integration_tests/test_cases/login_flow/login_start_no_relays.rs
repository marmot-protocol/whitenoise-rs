use crate::integration_tests::core::*;
use crate::{LoginStatus, WhitenoiseError};
use async_trait::async_trait;
use nostr_sdk::prelude::*;

/// Tests that `login_start` returns `NeedsRelayLists` when the user has no
/// relay lists published on the network.
pub struct LoginStartNoRelaysTestCase {
    account_name: String,
}

impl LoginStartNoRelaysTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for LoginStartNoRelaysTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Testing login_start with no relay lists for: {}",
            self.account_name
        );

        // Generate a brand new key with nothing published on the network.
        let keys = Keys::generate();
        let expected_pubkey = keys.public_key();

        let result = context
            .whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .map_err(WhitenoiseError::Login)?;

        assert_eq!(
            result.status,
            LoginStatus::NeedsRelayLists,
            "Expected NeedsRelayLists when no relay lists are published"
        );
        assert_eq!(result.account.pubkey, expected_pubkey);

        // The account should exist in the database (partial state).
        let found = context
            .whitenoise
            .find_account_by_pubkey(&expected_pubkey)
            .await;
        assert!(
            found.is_ok(),
            "Partial account should exist in DB after login_start"
        );

        context.add_account(&self.account_name, result.account);
        tracing::info!("✓ login_start correctly returned NeedsRelayLists");
        Ok(())
    }
}
