use async_trait::async_trait;
use nostr_sdk::{EventBuilder, Keys, Metadata, Timestamp};

use crate::WhitenoiseError;
use crate::integration_tests::core::test_clients::create_test_client;
use crate::integration_tests::core::*;

use super::helpers::wait_for_latest_metadata_event;

const LOG_TARGET: &str =
    "integration_tests::test_cases::user_discovery::resolve_user_known_metadata_no_refresh";

pub struct ResolveUserKnownMetadataNoRefreshTestCase {
    test_keys: Keys,
    initial_metadata: Metadata,
    newer_metadata: Metadata,
}

impl ResolveUserKnownMetadataNoRefreshTestCase {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for ResolveUserKnownMetadataNoRefreshTestCase {
    fn default() -> Self {
        Self {
            test_keys: Keys::generate(),
            initial_metadata: Metadata::new().name("Local Known Name"),
            newer_metadata: Metadata::new().name("Network Newer Name"),
        }
    }
}

#[async_trait]
impl TestCase for ResolveUserKnownMetadataNoRefreshTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let test_pubkey = self.test_keys.public_key();
        tracing::info!(
            target: LOG_TARGET,
            "Testing known-metadata no-refresh behavior for pubkey: {}",
            test_pubkey
        );

        context.whitenoise.create_identity().await?;

        let client = create_test_client(&context.dev_relays, self.test_keys.clone()).await?;

        let initial_event_id = *client
            .send_event_builder(EventBuilder::metadata(&self.initial_metadata))
            .await?
            .id();
        wait_for_latest_metadata_event(
            &client,
            test_pubkey,
            initial_event_id,
            "wait for initial metadata event to be queryable",
        )
        .await?;

        let initial_user = retry_default(
            || async {
                let user = context
                    .whitenoise
                    .resolve_user_blocking(&test_pubkey)
                    .await?;
                if user.metadata.name == self.initial_metadata.name {
                    Ok(user)
                } else {
                    Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "Initial metadata has not been resolved yet"
                    )))
                }
            },
            &format!(
                "wait for initial blocking resolution for user {}",
                &test_pubkey.to_hex()[..8]
            ),
        )
        .await?;
        assert!(initial_user.metadata_is_known());

        context
            .whitenoise
            .reset_user_resolution_run_count_for_testing(&test_pubkey);

        let newer_timestamp = Timestamp::from(Timestamp::now().as_secs() + 60);
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
            "wait for newer metadata event to be queryable",
        )
        .await?;
        client.disconnect().await;

        let user_after = context
            .whitenoise
            .resolve_user_blocking(&test_pubkey)
            .await?;

        assert!(user_after.metadata_is_known());
        assert_eq!(
            context
                .whitenoise
                .user_resolution_run_count_for_testing(&test_pubkey),
            0,
            "Known metadata should not trigger a second targeted discovery run"
        );

        tracing::info!(
            target: LOG_TARGET,
            "✓ Known metadata skipped targeted discovery on the second blocking resolution"
        );

        Ok(())
    }
}
