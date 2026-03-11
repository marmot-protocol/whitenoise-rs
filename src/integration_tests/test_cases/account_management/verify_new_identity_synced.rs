use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;

/// Verifies that newly created identities have `last_synced_at` set immediately.
///
/// Regression test: `create_identity` used to leave `last_synced_at = NULL`, which
/// poisoned `compute_global_since_timestamp()` for ALL accounts on the same daemon,
/// forcing global subscriptions to use `since=None` (unbounded re-fetch). This
/// broke messaging for existing accounts whenever a new identity was added.
pub struct VerifyNewIdentitySyncedTestCase {
    account_names: Vec<String>,
}

impl VerifyNewIdentitySyncedTestCase {
    /// Verify that all named accounts (previously created via `CreateAccountsTestCase`)
    /// have `last_synced_at` set.
    pub fn for_accounts(names: Vec<&str>) -> Self {
        Self {
            account_names: names.iter().map(|s| s.to_string()).collect(),
        }
    }
}

#[async_trait]
impl TestCase for VerifyNewIdentitySyncedTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Verifying {} accounts have last_synced_at set...",
            self.account_names.len()
        );

        for name in &self.account_names {
            let account = context.get_account(name)?;

            assert!(
                account.last_synced_at.is_some(),
                "Account '{}' ({}) has last_synced_at = None. \
                 New identities must be marked as synced immediately to prevent \
                 poisoning compute_global_since_timestamp() for all accounts.",
                name,
                account.pubkey.to_hex()
            );

            tracing::info!(
                "✓ Account '{}' has last_synced_at = {:?}",
                name,
                account.last_synced_at
            );
        }

        tracing::info!(
            "✓ All {} accounts have last_synced_at set",
            self.account_names.len()
        );
        Ok(())
    }
}
