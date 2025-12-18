use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use mdk_core::prelude::*;
use nostr_sdk::PublicKey;

pub struct CreateGroupTestCase {
    group_name: String,
    group_description: String,
    creator_account: String,
    member_accounts: Vec<String>,
}

impl CreateGroupTestCase {
    pub fn basic() -> Self {
        Self {
            group_name: "test_group".to_string(),
            group_description: "A group for integration testing".to_string(),
            creator_account: "creator".to_string(),
            member_accounts: vec!["member1".to_string(), "member2".to_string()],
        }
    }

    pub fn with_name(mut self, name: &str) -> Self {
        self.group_name = name.to_string();
        self
    }

    pub fn with_members(mut self, creator: &str, members: Vec<&str>) -> Self {
        self.creator_account = creator.to_string();
        self.member_accounts = members.into_iter().map(|s| s.to_string()).collect();
        self
    }
}

#[async_trait]
impl TestCase for CreateGroupTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Creating group '{}'...", self.group_name);

        let creator = context.get_account(&self.creator_account)?;
        let member_pubkeys: Vec<PublicKey> = self
            .member_accounts
            .iter()
            .map(|name| context.get_account(name).map(|acc| acc.pubkey))
            .collect::<Result<Vec<_>, _>>()?;
        let admin_pubkeys = vec![creator.pubkey];

        let test_group = context
            .whitenoise
            .create_group(
                creator,
                member_pubkeys,
                NostrGroupConfigData::new(
                    self.group_name.clone(),
                    self.group_description.clone(),
                    None, // image_hash
                    None, // image_key
                    None, // image_nonce
                    context.test_relays(),
                    admin_pubkeys,
                ),
                None,
            )
            .await?;

        // Give some time for MLS group synchronization
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        tracing::info!("✓ Group '{}' created successfully", test_group.name);
        context.add_group(&self.group_name, test_group);
        Ok(())
    }
}
