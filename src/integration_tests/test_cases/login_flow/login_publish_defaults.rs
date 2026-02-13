use crate::integration_tests::core::*;
use crate::{LoginStatus, RelayType, WhitenoiseError};
use async_trait::async_trait;

/// After `login_start` returned `NeedsRelayLists`, this test case calls
/// `login_publish_default_relays` to publish defaults and complete the login.
///
/// Expects the account referenced by `account_name` to already be in the
/// context from a prior `LoginStartNoRelaysTestCase`.
pub struct LoginPublishDefaultsTestCase {
    account_name: String,
}

impl LoginPublishDefaultsTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for LoginPublishDefaultsTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Testing login_publish_default_relays for: {}",
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let pubkey = account.pubkey;

        let result = context
            .whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .map_err(WhitenoiseError::Login)?;

        assert_eq!(
            result.status,
            LoginStatus::Complete,
            "Expected Complete after publishing default relays"
        );
        assert_eq!(result.account.pubkey, pubkey);

        // Verify all three relay types are now stored.
        for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
            let relays = result
                .account
                .relays(relay_type, context.whitenoise)
                .await?;
            assert!(
                !relays.is_empty(),
                "Expected {:?} relays to be stored after publishing defaults",
                relay_type
            );
        }

        // Update the account in context with the completed version.
        context.add_account(&self.account_name, result.account);
        tracing::info!("✓ login_publish_default_relays completed login");
        Ok(())
    }
}
