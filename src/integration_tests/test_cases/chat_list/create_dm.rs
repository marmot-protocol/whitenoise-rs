use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use mdk_core::prelude::*;

/// Creates a DirectMessage group between two accounts.
/// DM groups have an empty name and both participants are admins.
pub struct CreateDmTestCase {
    context_name: String,
    creator_account: String,
    other_account: String,
}

impl CreateDmTestCase {
    pub fn new(creator: &str, other: &str) -> Self {
        Self {
            context_name: format!("dm_{}_{}", creator, other),
            creator_account: creator.to_string(),
            other_account: other.to_string(),
        }
    }

    pub fn with_context_name(mut self, name: &str) -> Self {
        self.context_name = name.to_string();
        self
    }
}

#[async_trait]
impl TestCase for CreateDmTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Creating DM '{}' between '{}' and '{}'...",
            self.context_name,
            self.creator_account,
            self.other_account
        );

        let creator = context.get_account(&self.creator_account)?;
        let other = context.get_account(&self.other_account)?;

        let dm_group = context
            .whitenoise
            .create_group(
                creator,
                vec![other.pubkey],
                NostrGroupConfigData::new(
                    String::new(),
                    String::new(),
                    None,
                    None,
                    None,
                    context.test_relays(),
                    vec![creator.pubkey, other.pubkey],
                ),
                None,
            )
            .await?;

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        tracing::info!(
            "✓ DM '{}' created between '{}' and '{}'",
            self.context_name,
            self.creator_account,
            self.other_account
        );
        context.add_group(&self.context_name, dm_group);
        Ok(())
    }
}
