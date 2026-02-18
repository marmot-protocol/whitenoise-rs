use std::time::Duration;

use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::core::{RetryConfig, ScenarioContext, TestCase, retry_until};

/// Verifies that a member's MLS epoch has advanced after joining a group,
/// confirming the post-welcome self-update (MIP-02) completed successfully.
///
/// After accepting a Welcome message, each member performs a self-update to
/// rotate their leaf node key material, replacing the publicly-available
/// KeyPackage keys with fresh material. This test case verifies that the
/// self-update commit was applied by checking that the group epoch has
/// reached at least 2 (epoch 1 = group creation/welcome, epoch 2 = self-update).
///
/// Note: We check for a minimum epoch rather than comparing against a
/// "before" snapshot because the self-update runs as a background task
/// and may complete before or after the welcome wait finishes.
pub struct VerifySelfUpdateTestCase {
    account_names: Vec<String>,
    group_name: String,
    /// The minimum epoch that indicates the self-update has been applied.
    /// For a member joining via welcome: epoch 1 = welcome accepted,
    /// epoch 2+ = at least one self-update has been applied.
    min_expected_epoch: u64,
}

impl VerifySelfUpdateTestCase {
    /// Creates a new VerifySelfUpdateTestCase for multiple accounts
    ///
    /// # Arguments
    /// * `account_names` - Names of accounts whose self-update should be verified
    /// * `group_name` - Name of the group to check
    pub fn new(account_names: Vec<&str>, group_name: &str) -> Self {
        Self {
            account_names: account_names.into_iter().map(str::to_string).collect(),
            group_name: group_name.to_string(),
            min_expected_epoch: 2,
        }
    }

    /// Creates a test case for a single account
    pub fn for_account(account_name: &str, group_name: &str) -> Self {
        Self::new(vec![account_name], group_name)
    }
}

#[async_trait]
impl TestCase for VerifySelfUpdateTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let group = context.get_group(&self.group_name)?;
        let group_id = group.mls_group_id.clone();
        let wn = context.whitenoise;

        // The self-update runs as a deferred background task after welcome
        // processing. It waits for in-flight messages at the current epoch to
        // arrive before advancing the epoch, then performs MDK operations and
        // a relay round-trip. Use a generous retry window to accommodate the
        // intentional delay plus network/processing time.
        let config = RetryConfig::new(40, Duration::from_millis(500));

        for account_name in &self.account_names {
            let account = context.get_account(account_name)?;
            tracing::info!(
                "Verifying self-update for {} in group '{}'...",
                account_name,
                self.group_name
            );

            let min_epoch = self.min_expected_epoch;
            let account_name_owned = account_name.clone();
            let final_epoch = retry_until(
                config.clone(),
                || {
                    let account = account.clone();
                    let group_id = group_id.clone();
                    let account_name = account_name_owned.clone();
                    async move {
                        let current_group = wn.group(&account, &group_id).await?;
                        if current_group.epoch >= min_epoch {
                            Ok(current_group.epoch)
                        } else {
                            Err(WhitenoiseError::Other(anyhow::anyhow!(
                                "{} epoch is {} but expected >= {} \
                                 (self-update not yet applied)",
                                account_name,
                                current_group.epoch,
                                min_epoch
                            )))
                        }
                    }
                },
                &format!("{} self-update epoch advancement", account_name),
            )
            .await?;

            tracing::info!(
                "✓ {} self-update verified: epoch {} (>= minimum {})",
                account_name,
                final_epoch,
                self.min_expected_epoch
            );
        }

        if self.account_names.len() > 1 {
            tracing::info!(
                "✓ All {} accounts have completed post-welcome self-update",
                self.account_names.len()
            );
        }

        Ok(())
    }
}
