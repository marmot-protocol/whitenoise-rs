use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use nostr_sdk::{Keys, Metadata};

const LOG_TARGET: &str =
    "integration_tests::test_cases::user_discovery::find_or_create_user_unknown_metadata_no_result";

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

        let stored_user = context.whitenoise.find_user_by_pubkey(&test_pubkey).await?;
        assert_eq!(stored_user.metadata, Metadata::new());
        assert!(
            stored_user.metadata_known_at.is_none(),
            "Stored user must remain unknown when discovery returns no metadata"
        );

        tracing::info!(
            target: LOG_TARGET,
            "✓ Unknown metadata remained unknown after no-result discovery"
        );

        Ok(())
    }
}
