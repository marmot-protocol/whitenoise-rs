use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;

pub struct FetchMetadataTestCase {
    account_name: String,
}

impl FetchMetadataTestCase {
    pub fn for_account(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for FetchMetadataTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Fetching metadata for account: {}", self.account_name);

        let account = context.get_account(&self.account_name)?;
        let metadata = account.metadata(context.whitenoise).await?;

        assert!(
            metadata.name.is_none(),
            "New account should have no metadata name set but got: {:?}",
            metadata.name
        );
        tracing::info!("✓ Metadata name is correctly empty for new account");

        tracing::info!("✓ Metadata fetched successfully for {}", self.account_name);
        Ok(())
    }
}
