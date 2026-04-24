use crate::integration_tests::core::*;
use crate::{RelayType, WhitenoiseError};
use async_trait::async_trait;
use nostr_sdk::prelude::*;

pub struct LoginTestCase {
    account_name: String,
    metadata_name: Option<String>,
    metadata_about: Option<String>,
    relays: Vec<&'static str>,
}

impl LoginTestCase {
    pub fn new(account_name: &str) -> Self {
        let relays = if cfg!(debug_assertions) {
            vec!["ws://localhost:8080"]
        } else {
            vec![
                "wss://relay.damus.io",
                "wss://relay.primal.net",
                "wss://nos.lol",
            ]
        };
        Self {
            account_name: account_name.to_string(),
            metadata_name: None,
            metadata_about: None,
            relays,
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

        // Publish test events first via external client
        let test_client = create_test_client(&context.dev_relays, keys.clone()).await?;

        // Publish metadata if specified
        if let (Some(name), Some(about)) = (&self.metadata_name, &self.metadata_about) {
            publish_test_metadata(&test_client, name, about).await?;
        }

        // Publish relay list
        let relay_urls: Vec<String> = self.relays.iter().map(|s| s.to_string()).collect();
        publish_relay_lists(&test_client, relay_urls).await?;

        test_client.disconnect().await;

        // Login with the keys
        let account = context
            .whitenoise
            .login(keys.secret_key().to_secret_hex())
            .await?;

        assert_eq!(account.pubkey, expected_pubkey);
        let relays = account
            .relays(RelayType::Nip65, &context.whitenoise)
            .await?;
        assert_eq!(relays.len(), self.relays.len());
        for relay in relays {
            assert!(
                self.relays.contains(&relay.url.as_str()),
                "Relay {} not found in expected relays",
                relay.url
            );
        }
        context.add_account(&self.account_name, account);

        tracing::info!("✓ Successfully logged in account: {}", self.account_name);
        Ok(())
    }
}
