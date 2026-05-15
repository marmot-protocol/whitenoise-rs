use crate::WhitenoiseError;
use crate::integration_tests::core::test_clients::{create_test_client, publish_relay_lists};
use crate::integration_tests::core::*;
use async_trait::async_trait;
use nostr_sdk::{Keys, Metadata};

/// Tests `resolve_user` for a new user whose metadata is still unknown locally.
///
/// This test verifies:
/// - User is created immediately in the database
/// - Method returns immediately WITHOUT waiting for metadata/relays
/// - Metadata is empty immediately after the call
/// - Background fetch eventually completes and populates metadata
pub struct ResolveUserTestCase {
    test_keys: Keys,
    test_metadata: Metadata,
}

impl ResolveUserTestCase {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for ResolveUserTestCase {
    fn default() -> Self {
        let keys = Keys::generate();
        let metadata = Metadata::new()
            .name("Background User")
            .display_name("Background Display")
            .about("Testing resolve_user background discovery");

        Self {
            test_keys: keys,
            test_metadata: metadata,
        }
    }
}

#[async_trait]
impl TestCase for ResolveUserTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let test_pubkey = self.test_keys.public_key();
        tracing::info!("Testing resolve_user for pubkey: {}", test_pubkey);

        // Create an identity so we can subscribe to events
        context.whitenoise.create_identity().await?;

        // Publish fixture data to the discovery plane so the background fetch
        // can find it. The NIP-65 payload lists the same relays so any
        // subsequent protocol-correct lookup stays local.
        let discovery_relays = &context.discovery_relays;
        let test_client = create_test_client(discovery_relays, self.test_keys.clone()).await?;

        tracing::info!("Publishing test metadata and relays for test pubkey");
        test_client
            .send_event_builder(nostr_sdk::EventBuilder::metadata(&self.test_metadata))
            .await?;

        publish_relay_lists(&test_client, discovery_relays.clone()).await?;
        test_client.disconnect().await;

        // `resolve_user` should return the local snapshot immediately.
        let user = context.whitenoise.resolve_user(&test_pubkey).await?;

        assert_eq!(user.pubkey, test_pubkey, "User pubkey should match");
        assert!(user.id.is_some(), "User should have an ID after creation");

        tracing::info!(
            "✓ User created with resolve_user: ID {} for pubkey: {}",
            user.id.unwrap(),
            test_pubkey
        );

        // The user should be created immediately, but metadata should be empty initially
        assert_eq!(
            user.metadata,
            nostr_sdk::Metadata::default(),
            "Metadata should be empty immediately after resolve_user returns"
        );

        tracing::info!("✓ resolve_user returns immediately with an unknown local snapshot");

        // Wait for background discovery to complete.
        tracing::info!("Waiting for background metadata fetch to complete...");
        let updated_user = retry_default(
            || async {
                let u = context.whitenoise.find_user_by_pubkey(&test_pubkey).await?;
                if u.metadata != nostr_sdk::Metadata::default() {
                    Ok(u)
                } else {
                    Err(WhitenoiseError::Internal(
                        "Background metadata fetch not yet complete".to_string(),
                    ))
                }
            },
            &format!(
                "wait for background metadata fetch for user {}",
                &test_pubkey.to_hex()[..8]
            ),
        )
        .await?;

        assert_eq!(
            updated_user.metadata.name, self.test_metadata.name,
            "Metadata name should match after background fetch"
        );
        assert!(updated_user.metadata_known_at.is_some());

        tracing::info!("✓ Background discovery completed successfully");

        Ok(())
    }
}
