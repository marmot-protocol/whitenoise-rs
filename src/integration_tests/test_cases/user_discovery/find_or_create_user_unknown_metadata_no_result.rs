use std::time::Duration;

use async_trait::async_trait;
use nostr_sdk::{Client, Filter, Keys, Kind, Metadata};

use crate::integration_tests::core::test_clients::{create_test_client, publish_relay_lists};
use crate::integration_tests::core::*;
use crate::{RelayType, WhitenoiseError};

const LOG_TARGET: &str =
    "integration_tests::test_cases::user_discovery::find_or_create_user_unknown_metadata_no_result";

async fn wait_for_relay_list_indexed(
    client: &Client,
    pubkey: nostr_sdk::PublicKey,
) -> Result<(), WhitenoiseError> {
    retry(
        30,
        Duration::from_millis(100),
        || async {
            let events = client
                .fetch_events(
                    Filter::new().author(pubkey).kind(Kind::RelayList),
                    Duration::from_secs(1),
                )
                .await?;

            if events.iter().next().is_some() {
                Ok(())
            } else {
                Err(WhitenoiseError::Other(anyhow::anyhow!(
                    "Relay-list event is not yet queryable for {}",
                    pubkey
                )))
            }
        },
        "wait for relay-list event to be queryable",
    )
    .await
}

/// Tests that unknown metadata stays unknown when targeted discovery finds nothing.
///
/// This covers the legacy empty-row behavior we want after the migration:
/// an empty local user record is still "unknown" until a valid kind-0 event is
/// processed. A no-result lookup must not silently convert it into known blank
/// metadata.
pub struct FindOrCreateUserUnknownMetadataNoResultTestCase {
    test_keys: Keys,
}

impl FindOrCreateUserUnknownMetadataNoResultTestCase {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for FindOrCreateUserUnknownMetadataNoResultTestCase {
    fn default() -> Self {
        Self {
            test_keys: Keys::generate(),
        }
    }
}

#[async_trait]
impl TestCase for FindOrCreateUserUnknownMetadataNoResultTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let test_pubkey = self.test_keys.public_key();
        tracing::info!(
            target: LOG_TARGET,
            "Testing unknown-metadata no-result behavior for pubkey: {}",
            test_pubkey
        );

        let client = create_test_client(&context.dev_relays, self.test_keys.clone()).await?;
        let expected_relays = context.test_relays();
        let relay_urls = expected_relays
            .iter()
            .map(|relay_url| relay_url.to_string())
            .collect();
        publish_relay_lists(&client, relay_urls).await?;
        wait_for_relay_list_indexed(&client, test_pubkey).await?;
        client.disconnect().await;

        let user = context
            .whitenoise
            .find_or_create_user_by_pubkey(
                &test_pubkey,
                crate::whitenoise::users::UserSyncMode::Blocking,
            )
            .await?;

        assert_eq!(user.pubkey, test_pubkey);
        assert_eq!(user.metadata, Metadata::new());
        assert!(
            user.metadata_known_at.is_none(),
            "Blocking discovery with no result must keep metadata unknown"
        );

        let stored_relays = user
            .relays_by_type(RelayType::Nip65, context.whitenoise)
            .await?;
        assert!(
            !stored_relays.is_empty(),
            "Blocking discovery should still process relay-list events for unknown users"
        );
        for expected_relay in &expected_relays {
            assert!(
                stored_relays
                    .iter()
                    .any(|relay| relay.url == *expected_relay),
                "Expected discovered NIP-65 relay {} to be stored",
                expected_relay
            );
        }

        let stored_user = context.whitenoise.find_user_by_pubkey(&test_pubkey).await?;
        assert_eq!(stored_user.metadata, Metadata::new());
        assert!(
            stored_user.metadata_known_at.is_none(),
            "Stored user must remain unknown when discovery returns no metadata"
        );

        tracing::info!(
            target: LOG_TARGET,
            "✓ Unknown metadata remained unknown while relay-list discovery still ran"
        );

        Ok(())
    }
}
