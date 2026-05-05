//! Test case: create an account whose initial key package is *not*
//! auto-published, so a fixture (e.g. the legacy-capability KP fixture) can
//! be the sole publisher of the account's key packages.
//!
//! Pairs with `publish_legacy_capability_key_package` (sibling module):
//! that fixture must be the only KP publisher for the account it targets,
//! otherwise the modern KP that `create_identity` publishes in a background
//! task wins resolver tie-breaks and silently shadows the legacy variant.
//!
//! Sibling to `CreateAccountsTestCase`: that case has the simple contract
//! "create accounts with KPs ready". This case is its no-initial-KP
//! counterpart and intentionally does NOT wait for a key-package
//! publication (there will not be one).

use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;

pub struct CreateLegacyPeerAccountTestCase {
    account_name: String,
}

impl CreateLegacyPeerAccountTestCase {
    pub fn with_name(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for CreateLegacyPeerAccountTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Creating legacy-peer account '{}' (no initial key package)...",
            self.account_name
        );

        let initial_count = context.whitenoise.get_accounts_count().await?;

        let account = context
            .whitenoise
            .create_identity_without_initial_key_package()
            .await?;
        tracing::info!(
            "✓ Created legacy-peer account {}: {}",
            self.account_name,
            account.pubkey.to_hex()
        );
        context.add_account(&self.account_name, account);

        let final_count = context.whitenoise.get_accounts_count().await?;
        assert_eq!(final_count, initial_count + 1);

        // Intentionally do NOT wait for a key-package publication: this
        // account is meant to have no published KP until the legacy fixture
        // publishes one.

        Ok(())
    }
}
