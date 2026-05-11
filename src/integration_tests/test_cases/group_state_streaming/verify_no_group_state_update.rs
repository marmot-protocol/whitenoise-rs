use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::group_state_streaming::GroupStateUpdate;
use async_trait::async_trait;
use tokio::sync::{Mutex, broadcast};

/// Negative counterpart to [`super::VerifyGroupStateUpdateTestCase`]: asserts
/// that a subscribed account receives **no** group state update during the
/// scenario step.
///
/// Use this to prove the per-`(account, group)` isolation contract — when one
/// account leaves or is removed, every *other* account's stream for the same
/// group must stay silent.
///
/// Usage:
/// ```ignore
/// let silent = VerifyNoGroupStateUpdateTestCase::new("uninvolved_account", "group_name");
/// silent.subscribe(&context).await?;
/// // ... perform action affecting a *different* account ...
/// // Run the positive verifier first to synchronise on MLS propagation, then:
/// silent.execute(&mut context).await?;
/// ```
///
/// `execute()` adds a short settling delay before checking, so any in-flight
/// processing on the uninvolved account's side has time to (incorrectly) fire
/// if the contract is broken.
pub struct VerifyNoGroupStateUpdateTestCase {
    account_name: String,
    group_name: String,
    settle_duration: tokio::time::Duration,
    receiver: Mutex<Option<broadcast::Receiver<GroupStateUpdate>>>,
}

impl VerifyNoGroupStateUpdateTestCase {
    pub fn new(account_name: &str, group_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
            settle_duration: tokio::time::Duration::from_millis(500),
            receiver: Mutex::new(None),
        }
    }

    /// Override the default settle duration before checking the receiver.
    pub fn with_settle_duration(mut self, settle: tokio::time::Duration) -> Self {
        self.settle_duration = settle;
        self
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
            "Subscribed to group state for '{}' on group '{}' to assert no events arrive",
            self.account_name,
            self.group_name,
        );
        Ok(())
    }
}

#[async_trait]
impl TestCase for VerifyNoGroupStateUpdateTestCase {
    async fn run(&self, _context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        // Absorb any in-flight processing before sampling the receiver.
        tokio::time::sleep(self.settle_duration).await;

        let mut guard = self.receiver.lock().await;
        let receiver = guard.as_mut().ok_or_else(|| {
            WhitenoiseError::Internal(
                "VerifyNoGroupStateUpdateTestCase: subscribe() must be called before run()."
                    .to_string(),
            )
        })?;

        match receiver.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) => {
                tracing::info!(
                    "✓ Account '{}' received no group state update on group '{}' (as expected)",
                    self.account_name,
                    self.group_name,
                );
                Ok(())
            }
            Ok(unexpected) => {
                panic!(
                    "Expected no group state update on group '{}' for account '{}', but received {:?}",
                    self.group_name, self.account_name, unexpected,
                );
            }
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                Err(WhitenoiseError::Internal(format!(
                    "group state stream for account '{}' lagged by {} updates — \
                     cannot prove isolation contract",
                    self.account_name, n,
                )))
            }
            Err(broadcast::error::TryRecvError::Closed) => Err(WhitenoiseError::Internal(format!(
                "group state stream for account '{}' was closed unexpectedly",
                self.account_name,
            ))),
        }
    }
}
