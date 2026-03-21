use crate::WhitenoiseError;
use crate::integration_tests::core::test_clients::{create_test_client, publish_relay_lists};
use crate::integration_tests::core::*;
use async_trait::async_trait;
use nostr_sdk::{Keys, Metadata, RelayUrl};

/// Tests `resolve_user_blocking`.
///
/// This test verifies:
/// - User creation when user doesn't exist
/// - blocking lookup populates metadata and relay lists before returning when they are available
/// - Idempotency (calling twice returns the same user)
pub struct ResolveUserBlockingTestCase {
    test_keys: Keys,
    should_have_metadata: bool,
    should_have_relays: bool,
    test_metadata: Option<Metadata>,
    test_relays: Vec<RelayUrl>,
}

impl ResolveUserBlockingTestCase {
    pub fn basic() -> Self {
        let keys = Keys::generate();
        Self {
            test_keys: keys,
            should_have_metadata: false,
            should_have_relays: false,
            test_metadata: None,
            test_relays: vec![],
        }
    }

    pub fn with_metadata(mut self) -> Self {
        let metadata = Metadata::new()
            .name("Test User")
            .display_name("Test Display Name")
            .about("Test about section");

        self.should_have_metadata = true;
        self.test_metadata = Some(metadata);
        self
    }

    pub fn with_relays(mut self) -> Self {
        let test_relays = if cfg!(debug_assertions) {
            vec![
                RelayUrl::parse("ws://localhost:8080").unwrap(),
                RelayUrl::parse("ws://localhost:7777").unwrap(),
            ]
        } else {
            vec![
                RelayUrl::parse("wss://relay.damus.io").unwrap(),
                RelayUrl::parse("wss://relay.primal.net").unwrap(),
                RelayUrl::parse("wss://nos.lol").unwrap(),
            ]
        };

        self.should_have_relays = true;
        self.test_relays = test_relays;
        self
    }

    async fn publish_metadata(&self, context: &ScenarioContext) -> Result<(), WhitenoiseError> {
        let test_client = create_test_client(&context.dev_relays, self.test_keys.clone()).await?;

        if let Some(metadata) = &self.test_metadata {
            tracing::info!("Publishing test metadata for test pubkey");
            test_client
                .send_event_builder(nostr_sdk::EventBuilder::metadata(metadata))
                .await?;
        }

        test_client.disconnect().await;
        Ok(())
    }

    async fn publish_relays_data(&self, context: &ScenarioContext) -> Result<(), WhitenoiseError> {
        let test_client = create_test_client(&context.dev_relays, self.test_keys.clone()).await?;

        tracing::info!("Publishing test relay list for test pubkey");
        let relay_urls: Vec<String> = self.test_relays.iter().map(|url| url.to_string()).collect();
        publish_relay_lists(&test_client, relay_urls).await?;

        test_client.disconnect().await;
        Ok(())
    }
}

#[async_trait]
impl TestCase for ResolveUserBlockingTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let test_pubkey = self.test_keys.public_key();
        tracing::info!("Testing resolve_user_blocking for pubkey: {}", test_pubkey);
        let user_exists = context
            .whitenoise
            .find_user_by_pubkey(&test_pubkey)
            .await
            .is_ok();
        assert!(!user_exists, "User should not exist initially");

        if self.should_have_metadata || self.should_have_relays {
            // Create an account: We need to have at least one account to be able to subscribe to events
            context.whitenoise.create_identity().await?;
        }

        if self.should_have_metadata {
            self.publish_metadata(context).await?;
        }

        if self.should_have_relays {
            self.publish_relays_data(context).await?;
        }

        let user = context
            .whitenoise
            .resolve_user_blocking(&test_pubkey)
            .await?;

        assert_eq!(user.pubkey, test_pubkey, "User pubkey should match");
        assert!(user.id.is_some(), "User should have an ID after creation");

        tracing::info!(
            "✓ User created with ID: {} for pubkey: {}",
            user.id.unwrap(),
            test_pubkey
        );

        let found_user = context.whitenoise.find_user_by_pubkey(&test_pubkey).await?;
        assert_eq!(found_user.pubkey, test_pubkey, "Found user should match");
        assert_eq!(found_user.id, user.id, "Found user ID should match");

        tracing::info!("✓ User can be found by pubkey after creation");

        if self.should_have_metadata {
            tracing::info!("✓ resolve_user_blocking returned metadata before returning");
        }

        if self.should_have_metadata {
            if let Some(expected_metadata) = &self.test_metadata {
                assert_eq!(
                    user.metadata.name, expected_metadata.name,
                    "Metadata name should match published data"
                );
                assert_eq!(
                    user.metadata.display_name, expected_metadata.display_name,
                    "Metadata display_name should match published data"
                );
                assert_eq!(
                    user.metadata.about, expected_metadata.about,
                    "Metadata about should match published data"
                );

                tracing::info!(
                    "✓ User metadata matches published data: name={:?}, display_name={:?}",
                    user.metadata.name,
                    user.metadata.display_name
                );
            }
        } else {
            assert!(
                user.metadata.name.is_none() || user.metadata.name == Some(String::new()),
                "User should have empty/no name when no metadata published"
            );
            tracing::info!("✓ User has empty metadata as expected (nothing published)");
        }

        if self.should_have_relays {
            let user_relays = user
                .relays_by_type(
                    crate::whitenoise::relays::RelayType::Nip65,
                    context.whitenoise,
                )
                .await?;

            let relay_urls: Vec<&RelayUrl> = user_relays.iter().map(|r| &r.url).collect();
            for expected_relay in &self.test_relays {
                assert!(
                    relay_urls.contains(&expected_relay),
                    "User should have relay {} that was published",
                    expected_relay
                );
            }

            tracing::info!(
                "✓ User relay list matches published data: {} relays found",
                user_relays.len()
            );
        } else {
            tracing::info!("✓ No relay publication needed for this test case");
        }

        let user_again = context.whitenoise.resolve_user(&test_pubkey).await?;
        assert_eq!(
            user_again.id, user.id,
            "Should return same user on second call"
        );
        assert_eq!(
            user_again.pubkey, user.pubkey,
            "Should return same user pubkey"
        );
        assert_eq!(
            user_again.metadata, user.metadata,
            "Should return same user metadata"
        );

        tracing::info!("✓ resolve_user returns the existing local snapshot on second call");

        Ok(())
    }
}
