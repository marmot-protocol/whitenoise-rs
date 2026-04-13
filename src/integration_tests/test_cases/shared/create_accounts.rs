use std::time::Duration;

use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;

pub struct CreateAccountsTestCase {
    account_names: Vec<String>,
}

impl CreateAccountsTestCase {
    pub fn with_names(account_names: Vec<&str>) -> Self {
        Self {
            account_names: account_names.iter().map(|s| s.to_string()).collect(),
        }
    }
}

#[async_trait]
impl TestCase for CreateAccountsTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Creating {} accounts...", self.account_names.len());

        let initial_count = context.whitenoise.get_accounts_count().await?;

        for name in &self.account_names {
            let account = context.whitenoise.create_identity().await?;
            tracing::info!("✓ Created {}: {}", name, account.pubkey.to_hex());
            context.add_account(name, account);
        }

        let final_count = context.whitenoise.get_accounts_count().await?;
        assert_eq!(final_count, initial_count + self.account_names.len());

        retry(
            50,
            Duration::from_millis(100),
            || async {
                for name in &self.account_names {
                    let account = context.get_account(name)?;
                    let key_packages = context
                        .whitenoise
                        .fetch_all_key_packages_for_account(account)
                        .await?;

                    if key_packages.is_empty() {
                        return Err(WhitenoiseError::Internal(format!(
                            "Key package not yet published for account '{}' ({})",
                            name,
                            account.pubkey.to_hex()
                        )));
                    }
                }

                Ok(())
            },
            "wait for created accounts to publish initial key packages",
        )
        .await?;

        tracing::info!(
            "✓ Created {} accounts with published key packages",
            self.account_names.len()
        );
        Ok(())
    }
}
