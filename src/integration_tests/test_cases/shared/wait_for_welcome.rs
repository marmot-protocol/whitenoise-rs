use crate::WhitenoiseError;
use crate::integration_tests::core::{retry_default, ScenarioContext, TestCase};
use async_trait::async_trait;

/// Test case that waits for specified accounts to receive and process welcome messages
/// for a given group. This handles the asynchronous nature of MLS welcome auto-finalization.
pub struct WaitForWelcomeTestCase {
    account_names: Vec<String>,
    group_name: String,
}

impl WaitForWelcomeTestCase {
    /// Creates a new WaitForWelcomeTestCase
    ///
    /// # Arguments
    /// * `account_names` - Names of accounts that should receive the welcome
    /// * `group_name` - Name of the group to wait for
    pub fn new(account_names: Vec<&str>, group_name: &str) -> Self {
        Self {
            account_names: account_names.into_iter().map(String::from).collect(),
            group_name: group_name.to_string(),
        }
    }

    /// Creates a test case for a single account
    pub fn for_account(account_name: &str, group_name: &str) -> Self {
        Self::new(vec![account_name], group_name)
    }
}

#[async_trait]
impl TestCase for WaitForWelcomeTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let group = context.get_group(&self.group_name)?;
        let group_id = group.mls_group_id.clone();

        for account_name in &self.account_names {
            let account = context.get_account(account_name)?.clone();
            tracing::info!(
                "Waiting for {} to receive welcome for group '{}'...",
                account_name,
                self.group_name
            );

            let wn = context.whitenoise;
            let acc = account.clone();
            let gid = group_id.clone();
            let name = account_name.clone();

            retry_default(
                || {
                    let wn = wn;
                    let acc = acc.clone();
                    let gid = gid.clone();
                    async move {
                        let groups = wn.groups(&acc, true).await?;
                        if groups.iter().any(|g| g.mls_group_id == gid) {
                            Ok(())
                        } else {
                            Err(WhitenoiseError::Other(anyhow::anyhow!(
                                "{} has not yet received the group",
                                name
                            )))
                        }
                    }
                },
                &format!("{} welcome auto-finalization", account_name),
            )
            .await?;

            tracing::info!("✓ {} has received and processed the welcome", account_name);
        }

        if self.account_names.len() > 1 {
            tracing::info!(
                "✓ All {} accounts have received welcome and auto-finalized MLS membership",
                self.account_names.len()
            );
        }

        Ok(())
    }
}
