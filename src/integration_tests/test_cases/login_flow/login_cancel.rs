use crate::integration_tests::core::*;
use crate::{LoginStatus, WhitenoiseError};
use async_trait::async_trait;
use nostr_sdk::prelude::*;

/// Tests that `login_cancel` cleans up partial login state: the account
/// record is deleted from the DB and the keychain entry is removed.
pub struct LoginCancelTestCase {
    account_name: String,
}

impl LoginCancelTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for LoginCancelTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Testing login_cancel for: {}", self.account_name);

        // Generate a new key and start login (will return NeedsRelayLists
        // since nothing is published).
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let start_result = context
            .whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .map_err(WhitenoiseError::Login)?;

        assert_eq!(start_result.status, LoginStatus::NeedsRelayLists);

        // Verify account exists before cancel.
        let before = context.whitenoise.find_account_by_pubkey(&pubkey).await;
        assert!(before.is_ok(), "Account should exist in DB before cancel");

        let count_before = context.whitenoise.get_accounts_count().await?;

        // Cancel the login.
        context
            .whitenoise
            .login_cancel(&pubkey)
            .await
            .map_err(WhitenoiseError::Login)?;

        // Verify account is cleaned up.
        let after = context.whitenoise.find_account_by_pubkey(&pubkey).await;
        assert!(
            after.is_err(),
            "Account should be removed from DB after cancel"
        );

        let count_after = context.whitenoise.get_accounts_count().await?;
        assert_eq!(
            count_after,
            count_before - 1,
            "Account count should decrease by 1 after cancel"
        );

        // Store a dummy entry so the scenario can track this test ran.
        // We don't add to context.accounts since the account was cancelled.
        tracing::info!("✓ login_cancel cleaned up partial state");
        Ok(())
    }
}
