use crate::WhitenoiseError;
use crate::integration_tests::core::test_clients::create_test_client;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use nostr_sdk::{EventBuilder, Keys, Metadata, Timestamp};

/// Tests that targeted discovery cannot overwrite newer already-processed metadata.
///
/// This simulates a legacy row whose `metadata_known_at` is still `NULL` even
/// though a newer metadata event has already been processed. A subsequent
/// blocking lookup must not let an older relay result clobber the stored
/// metadata.
pub struct FindOrCreateUserPreservesNewerProcessedMetadataTestCase {
    test_keys: Keys,
    newer_metadata: Metadata,
    older_metadata: Metadata,
}

impl FindOrCreateUserPreservesNewerProcessedMetadataTestCase {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for FindOrCreateUserPreservesNewerProcessedMetadataTestCase {
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
impl TestCase for FindOrCreateUserPreservesNewerProcessedMetadataTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let test_pubkey = self.test_keys.public_key();
        tracing::info!(
            "Testing targeted discovery ordering for legacy unknown metadata on pubkey: {}",
            test_pubkey
        );

        let newer_timestamp = Timestamp::now();
        let older_timestamp = Timestamp::from(newer_timestamp.as_secs().saturating_sub(3600));

        let client = create_test_client(&context.dev_relays, self.test_keys.clone()).await?;
        client
            .send_event_builder(
                EventBuilder::metadata(&self.newer_metadata).custom_created_at(newer_timestamp),
            )
            .await?;
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        let initial_user = retry_default(
            || async {
                let user = context
                    .whitenoise
                    .find_or_create_user_by_pubkey(
                        &test_pubkey,
                        crate::whitenoise::users::UserSyncMode::Blocking,
                    )
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

        client
            .send_event_builder(
                EventBuilder::metadata(&self.older_metadata).custom_created_at(older_timestamp),
            )
            .await?;
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        client.disconnect().await;

        let user_after = context
            .whitenoise
            .find_or_create_user_by_pubkey(
                &test_pubkey,
                crate::whitenoise::users::UserSyncMode::Blocking,
            )
            .await?;

        assert_eq!(
            user_after.metadata.name, self.newer_metadata.name,
            "Older relay metadata must not overwrite newer processed metadata"
        );

        let stored_user = context.whitenoise.find_user_by_pubkey(&test_pubkey).await?;
        assert_eq!(stored_user.metadata.name, self.newer_metadata.name);

        tracing::info!("✓ Older targeted discovery result did not overwrite newer metadata");

        Ok(())
    }
}
