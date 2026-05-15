use async_trait::async_trait;
use nostr_sdk::PublicKey;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;

/// Asserts the exact set of blocked pubkeys returned by
/// `session.mute_list().get_blocked_users()`.
///
/// Comparison is set-based — the underlying SQL ordering is not part of the
/// contract.
pub struct VerifyBlockedUsersTestCase {
    account_name: String,
    expected: Vec<PublicKey>,
}

impl VerifyBlockedUsersTestCase {
    pub fn new(account_name: &str, expected: Vec<PublicKey>) -> Self {
        Self {
            account_name: account_name.to_string(),
            expected,
        }
    }
}

#[async_trait]
impl TestCase for VerifyBlockedUsersTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Verifying blocked users for account: {} (expecting {} entries)",
            self.account_name,
            self.expected.len()
        );

        let account = context.get_account(&self.account_name)?;
        let session = context.whitenoise.require_session(&account.pubkey)?;

        let entries = session.mute_list().get_blocked_users().await?;
        let actual: std::collections::BTreeSet<PublicKey> =
            entries.iter().map(|e| e.muted_pubkey).collect();
        let expected: std::collections::BTreeSet<PublicKey> =
            self.expected.iter().copied().collect();

        if actual != expected {
            return Err(WhitenoiseError::Internal(format!(
                "blocked-users mismatch: expected {:?}, got {:?}",
                expected
                    .iter()
                    .map(|p| p.to_hex()[..8].to_string())
                    .collect::<Vec<_>>(),
                actual
                    .iter()
                    .map(|p| p.to_hex()[..8].to_string())
                    .collect::<Vec<_>>(),
            )));
        }

        tracing::info!(
            "✓ Blocked-users set matches expected ({} entries)",
            self.expected.len()
        );

        Ok(())
    }
}
