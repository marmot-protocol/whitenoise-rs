use async_trait::async_trait;
use nostr_sdk::Keys;

use crate::WhitenoiseError;
use crate::integration_tests::core::test_clients::{create_test_client, publish_test_metadata};
use crate::integration_tests::core::*;
use crate::whitenoise::user_search::{SearchUpdateTrigger, UserSearchParams};
use crate::whitenoise::users::UserSyncMode;

use super::helpers::collect_search_updates;

/// Tests that search finds a followed user even when their User record has
/// empty metadata (as happens during contact list sync before the background
/// metadata fetch completes).
///
/// This exercises the fix in `get_metadata_for_pubkey`: when a User record
/// exists with default (empty) metadata, the function falls through to the
/// cache/network layers instead of returning empty metadata that can never match.
///
/// Sets up:
/// - Target publishes metadata to relays
/// - Target's User record is created via Background mode (empty metadata in DB)
/// - Searcher follows Target
///
/// Verifies:
/// - Search finds Target at radius 1 via network fallback
pub struct SearchEmptyMetadataTestCase {
    account_name: String,
}

impl SearchEmptyMetadataTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for SearchEmptyMetadataTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let account = context.get_account(&self.account_name)?;
        let searcher_pubkey = account.pubkey;

        let target_keys = Keys::generate();
        let target_pubkey = target_keys.public_key();
        let target_name = "EmptyMetaTarget";

        // Step 1: Publish metadata to relays
        let test_client = create_test_client(&context.dev_relays, target_keys).await?;
        publish_test_metadata(&test_client, target_name, "Published via relay").await?;
        test_client.disconnect().await;
        tracing::info!(
            "Published metadata for {} to relays",
            &target_pubkey.to_hex()[..8]
        );

        // Step 2: Create a User record with empty metadata using Background mode.
        // Background mode returns immediately with an empty-metadata User record
        // and schedules an async fetch. We search before that fetch completes.
        context
            .whitenoise
            .find_or_create_user_by_pubkey(&target_pubkey, UserSyncMode::Background)
            .await?;
        tracing::info!(
            "Created User record via Background mode for {}",
            &target_pubkey.to_hex()[..8]
        );

        // Step 3: Follow the target so they appear at radius 1
        context
            .whitenoise
            .follow_user(account, &target_pubkey)
            .await?;

        // Step 4: Search immediately (before background metadata sync completes)
        let subscription = context
            .whitenoise
            .search_users(UserSearchParams {
                query: target_name.to_string(),
                searcher_pubkey,
                radius_start: 0,
                radius_end: 1,
            })
            .await?;

        let updates = collect_search_updates(subscription.updates).await;

        // Verify search completed
        let completed = updates
            .iter()
            .any(|u| matches!(u.trigger, SearchUpdateTrigger::SearchCompleted { .. }));
        assert!(completed, "Search should emit SearchCompleted");

        // Verify the target was found via network fallback despite empty DB metadata
        let found = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .any(|r| r.pubkey == target_pubkey);

        assert!(
            found,
            "Search should find user via network fallback when User record has empty metadata"
        );

        tracing::info!(
            "✓ Search found user {} with empty DB metadata via network fallback",
            &target_pubkey.to_hex()[..8]
        );

        Ok(())
    }
}
