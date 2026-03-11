use crate::integration_tests::{
    core::*,
    test_cases::{account_management::*, shared::*},
};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

pub struct AccountManagementScenario {
    context: ScenarioContext,
}

impl AccountManagementScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for AccountManagementScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        CreateAccountsTestCase::with_names(vec![
            "acct_mgmt_account1",
            "acct_mgmt_account2",
            "acct_mgmt_account3",
        ])
        .execute(&mut self.context)
        .await?;

        // Regression: new identities must have last_synced_at set immediately.
        // Without this, a single unsynced account poisons global subscriptions
        // for ALL accounts on the daemon (since=None unbounded re-fetch).
        VerifyNewIdentitySyncedTestCase::for_accounts(vec![
            "acct_mgmt_account1",
            "acct_mgmt_account2",
            "acct_mgmt_account3",
        ])
        .execute(&mut self.context)
        .await?;

        LoginTestCase::new("account_with_previous_keys")
            .with_metadata("known_user", "A user with previous keys that logged in")
            .execute(&mut self.context)
            .await?;

        // Test logout functionality with verification
        LogoutAccountTestCase::for_account("acct_mgmt_account2")
            .expect_remaining_accounts(vec![
                "acct_mgmt_account1",
                "acct_mgmt_account3",
                "account_with_previous_keys",
            ])
            .execute(&mut self.context)
            .await?;

        Ok(())
    }
}
