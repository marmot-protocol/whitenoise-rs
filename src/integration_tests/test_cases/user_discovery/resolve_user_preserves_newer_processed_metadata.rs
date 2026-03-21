use std::time::Duration;

use async_trait::async_trait;
use nostr_sdk::{Client, EventBuilder, EventId, Filter, Keys, Kind, Metadata, Timestamp};

use crate::WhitenoiseError;
use crate::integration_tests::core::test_clients::create_test_client;
use crate::integration_tests::core::*;

const LOG_TARGET: &str = "integration_tests::test_cases::user_discovery::resolve_user_preserves_newer_processed_metadata";

async fn wait_for_latest_metadata_event(
    client: &Client,
    pubkey: nostr_sdk::PublicKey,
    expected_event_id: EventId,
    description: &str,
) -> Result<(), WhitenoiseError> {
    // Kind-0 is replaceable, so older events are not guaranteed to remain
    // queryable by exact ID once the relay has reconciled them. Poll the same
    // author+kind lookup shape used by targeted discovery instead.
    retry(
        30,
        Duration::from_millis(100),
        || async {
            let events = client
                .fetch_events(
                    Filter::new().author(pubkey).kind(Kind::Metadata),
                    Duration::from_secs(1),
                )
                .await?;

            if events.iter().any(|event| event.id == expected_event_id) {
                Ok(())
            } else {
                Err(WhitenoiseError::Other(anyhow::anyhow!(
                    "Latest metadata query does not yet return expected event {}",
                    expected_event_id.to_hex()
                )))
            }
        },
        description,
    )
    .await
}

/// Tests that targeted discovery cannot overwrite newer already-processed metadata.
///
/// This simulates a legacy row whose `metadata_known_at` is still `NULL` even
/// though a newer metadata event has already been processed. A subsequent
/// blocking lookup must not let an older relay result clobber the stored
/// metadata.
pub struct ResolveUserPreservesNewerProcessedMetadataTestCase {
    test_keys: Keys,
    newer_metadata: Metadata,
    older_metadata: Metadata,
}

impl ResolveUserPreservesNewerProcessedMetadataTestCase {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for ResolveUserPreservesNewerProcessedMetadataTestCase {
    fn default() -> Self {
        Self {
            test_keys: Keys::generate(),
            newer_metadata: Metadata::new()
                .name("Newer Name")
                .display_name("Newer Display"),
            older_metadata: Metadata::new()
                .name("Older Name")
                .display_name("Older Display"),
        }
    }
}

#[async_trait]
impl TestCase for ResolveUserPreservesNewerProcessedMetadataTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let test_pubkey = self.test_keys.public_key();
        tracing::info!(
            target: LOG_TARGET,
            "Testing targeted discovery ordering for legacy unknown metadata on pubkey: {}",
            test_pubkey
        );

        let newer_timestamp = Timestamp::now();
        let older_timestamp = Timestamp::from(newer_timestamp.as_secs().saturating_sub(3600));

        let client = create_test_client(&context.dev_relays, self.test_keys.clone()).await?;
        let newer_event_id = *client
            .send_event_builder(
                EventBuilder::metadata(&self.newer_metadata).custom_created_at(newer_timestamp),
            )
            .await?
            .id();
        wait_for_latest_metadata_event(
            &client,
            test_pubkey,
            newer_event_id,
            &format!(
                "wait for metadata lookup to return newer event {}",
                newer_event_id.to_hex()
            ),
        )
        .await?;

        let initial_user = retry_default(
            || async {
                let user = context
                    .whitenoise
                    .resolve_user_blocking(&test_pubkey)
                    .await?;

                if user.metadata.name == self.newer_metadata.name {
                    Ok(user)
                } else {
                    Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "Expected newer metadata to be processed first, got {:?}",
                        user.metadata.name
                    )))
                }
            },
            &format!(
                "wait for blocking discovery to process newer metadata for user {}",
                &test_pubkey.to_hex()[..8]
            ),
        )
        .await?;

        assert!(
            initial_user.metadata_known_at.is_some(),
            "Initial blocking lookup should mark metadata as known"
        );

        context
            .whitenoise
            .set_user_metadata_known_at_for_testing(&test_pubkey, None)
            .await?;

        let legacy_user = context.whitenoise.find_user_by_pubkey(&test_pubkey).await?;
        assert!(
            legacy_user.metadata_known_at.is_none(),
            "Legacy repair simulation should clear metadata_known_at"
        );
        assert_eq!(legacy_user.metadata.name, self.newer_metadata.name);

        let _older_event_id = *client
            .send_event_builder(
                EventBuilder::metadata(&self.older_metadata).custom_created_at(older_timestamp),
            )
            .await?
            .id();
        wait_for_latest_metadata_event(
            &client,
            test_pubkey,
            newer_event_id,
            &format!(
                "wait for metadata lookup to keep returning newer event {} after older publish",
                newer_event_id.to_hex()
            ),
        )
        .await?;
        client.disconnect().await;

        let user_after = context
            .whitenoise
            .resolve_user_blocking(&test_pubkey)
            .await?;

        assert_eq!(
            user_after.metadata.name, self.newer_metadata.name,
            "Older relay metadata must not overwrite newer processed metadata"
        );

        let stored_user = context.whitenoise.find_user_by_pubkey(&test_pubkey).await?;
        assert_eq!(stored_user.metadata.name, self.newer_metadata.name);

        tracing::info!(
            target: LOG_TARGET,
            "✓ Older targeted discovery result did not overwrite newer metadata"
        );

        Ok(())
    }
}
