use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::group_state_streaming::GroupStateUpdate;
use async_trait::async_trait;
use tokio::sync::{Mutex, broadcast};

/// Test case that verifies a real-time group state update is received correctly.
///
/// Two-phase like the other streaming verifiers:
/// 1. Subscribe BEFORE performing the action that triggers the update
/// 2. Perform the action (leave group, admin removal, etc.)
/// 3. Call `execute()` to await and verify the expected update
///
/// Usage:
/// ```ignore
/// let verifier = VerifyGroupStateUpdateTestCase::new(
///     "account_name",
///     "group_name",
///     GroupStateUpdate::LeftGroup,
/// );
/// verifier.subscribe(&context).await?;
/// // ... perform action that triggers the update ...
/// verifier.execute(&mut context).await?;
/// ```
pub struct VerifyGroupStateUpdateTestCase {
    account_name: String,
    group_name: String,
    expected_update: GroupStateUpdate,
    receiver: Mutex<Option<broadcast::Receiver<GroupStateUpdate>>>,
}

impl VerifyGroupStateUpdateTestCase {
    pub fn new(account_name: &str, group_name: &str, expected_update: GroupStateUpdate) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
            expected_update,
            receiver: Mutex::new(None),
        }
    }

    /// Subscribe to the group state stream. Must be called before `execute()`.
    pub async fn subscribe(&self, context: &ScenarioContext) -> Result<(), WhitenoiseError> {
        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;
        let subscription = context
            .whitenoise
            .subscribe_to_group_state(&account.pubkey, &group.mls_group_id)
            .await?;

        let mut guard = self.receiver.lock().await;
        *guard = Some(subscription.updates);

        tracing::info!(
            "Subscribed to group state for '{}' on group '{}', waiting for {:?} update",
            self.account_name,
            self.group_name,
            self.expected_update,
        );
        Ok(())
    }
}

#[async_trait]
impl TestCase for VerifyGroupStateUpdateTestCase {
    async fn run(&self, _context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let mut guard = self.receiver.lock().await;
        let receiver = guard.as_mut().ok_or_else(|| {
            WhitenoiseError::Internal(
                "VerifyGroupStateUpdateTestCase: subscribe() must be called before run()."
                    .to_string(),
            )
        })?;

        let update = tokio::time::timeout(tokio::time::Duration::from_secs(15), receiver.recv())
            .await
            .map_err(|_| {
                WhitenoiseError::Internal(format!(
                    "Timeout waiting for {:?} on group '{}' for account '{}'",
                    self.expected_update, self.group_name, self.account_name,
                ))
            })?
            .map_err(|e| WhitenoiseError::Internal(format!("Failed to receive update: {}", e)))?;

        assert_eq!(
            update, self.expected_update,
            "Expected {:?} but got {:?} for account '{}' on group '{}'",
            self.expected_update, update, self.account_name, self.group_name,
        );
        tracing::info!(
            "✓ Account '{}' received expected {:?} on group '{}'",
            self.account_name,
            update,
            self.group_name,
        );

        Ok(())
    }
}
