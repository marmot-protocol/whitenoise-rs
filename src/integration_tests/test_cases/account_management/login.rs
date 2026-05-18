use std::collections::HashSet;

use crate::integration_tests::core::*;
use crate::{RelayType, WhitenoiseError};
use async_trait::async_trait;
use nostr_sdk::prelude::*;

pub struct LoginTestCase {
    account_name: String,
    metadata_name: Option<String>,
    metadata_about: Option<String>,
}

impl LoginTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            metadata_name: None,
            metadata_about: None,
        }
    }

    pub fn with_metadata(mut self, name: &str, about: &str) -> Self {
        self.metadata_name = Some(name.to_string());
        self.metadata_about = Some(about.to_string());
        self
    }
}

#[async_trait]
impl TestCase for LoginTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Testing login for account: {}", self.account_name);

        let keys = Keys::generate();
        let expected_pubkey = keys.public_key();

        // login_start reads from `default_account_relays`, so both the publish
        // destination and the payload values come from that role.
        let publish_relays = &context.default_account_relays;
        let test_client = create_test_client(publish_relays, keys.clone()).await?;

        if let (Some(name), Some(about)) = (&self.metadata_name, &self.metadata_about) {
            publish_test_metadata(&test_client, name, about).await?;
        }

        publish_relay_lists(&test_client, publish_relays.clone()).await?;

        test_client.disconnect().await;

        let account = context
            .whitenoise
            .login(keys.secret_key().to_secret_hex())
            .await?;

        assert_eq!(account.pubkey, expected_pubkey);
        let stored_relays = account
            .relays(RelayType::Nip65, &context.whitenoise.shared)
            .await?;
        let expected: HashSet<String> = publish_relays.iter().cloned().collect();
        let actual: HashSet<String> = stored_relays.iter().map(|r| r.url.to_string()).collect();
        assert_eq!(
            actual, expected,
            "stored NIP-65 relay set should exactly match the published set"
        );
        context.add_account(&self.account_name, account);

        tracing::info!("✓ Successfully logged in account: {}", self.account_name);
        Ok(())
    }
}
