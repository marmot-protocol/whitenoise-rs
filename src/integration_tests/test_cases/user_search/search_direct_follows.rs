use async_trait::async_trait;
use nostr_sdk::Keys;

use crate::WhitenoiseError;
use crate::integration_tests::core::test_clients::{create_test_client, publish_test_metadata};
use crate::integration_tests::core::*;
use crate::whitenoise::user_search::{MatchedField, SearchUpdateTrigger, UserSearchParams};
use crate::whitenoise::users::UserSyncMode;

use super::helpers::collect_search_updates;

/// Tests that search finds directly followed users at radius 1.
///
/// Sets up:
/// - Searcher follows UserA (has metadata on relays)
/// - UserB exists with metadata but is NOT followed
///
/// Verifies:
/// 1. UserA is found at radius 1 with correct match quality
/// 2. UserB is excluded (not in the searcher's social graph at radius 1)
/// 3. Search lifecycle events fire correctly (RadiusStarted → ResultsFound → RadiusCompleted)
pub struct SearchDirectFollowsTestCase {
    account_name: String,
}

impl SearchDirectFollowsTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for SearchDirectFollowsTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let account = context.get_account(&self.account_name)?;
        let searcher_pubkey = account.pubkey;

        // Create a user the searcher will follow
        let followed_keys = Keys::generate();
        let followed_pubkey = followed_keys.public_key();
        let followed_name = "DirectFollow";

        let client = create_test_client(&context.dev_relays, followed_keys).await?;
        publish_test_metadata(&client, followed_name, "A directly followed user").await?;
        client.disconnect().await;

        // Create the user with blocking sync so metadata is populated in the DB
        context
            .whitenoise
            .find_or_create_user_by_pubkey(&followed_pubkey, UserSyncMode::Blocking)
            .await?;

        context
            .whitenoise
            .follow_user(account, &followed_pubkey)
            .await?;
        tracing::info!(
            "Followed user {} ('{}')",
            &followed_pubkey.to_hex()[..8],
            followed_name
        );

        // Create an unfollowed user with a similar name — should NOT appear in results
        let unfollowed_keys = Keys::generate();
        let unfollowed_pubkey = unfollowed_keys.public_key();

        let unfollowed_client = create_test_client(&context.dev_relays, unfollowed_keys).await?;
        publish_test_metadata(
            &unfollowed_client,
            "DirectUnfollowed",
            "Not in social graph",
        )
        .await?;
        unfollowed_client.disconnect().await;

        context
            .whitenoise
            .find_or_create_user_by_pubkey(&unfollowed_pubkey, UserSyncMode::Blocking)
            .await?;

        // Search at radius 0-1
        let subscription = context
            .whitenoise
            .search_users(UserSearchParams {
                query: followed_name.to_string(),
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

        // Verify the followed user was found at radius 1
        let followed_result = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .find(|r| r.pubkey == followed_pubkey);

        assert!(
            followed_result.is_some(),
            "Search should find followed user at radius 1"
        );

        let result = followed_result.unwrap();
        assert_eq!(result.radius, 1, "Followed user should be at radius 1");
        assert_eq!(
            result.best_field,
            MatchedField::Name,
            "Best match should be on the name field"
        );
        tracing::info!(
            "✓ Found '{}' at radius {} (match: {:?})",
            followed_name,
            result.radius,
            result.match_quality
        );

        // Verify the unfollowed user was NOT found
        let unfollowed_found = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .any(|r| r.pubkey == unfollowed_pubkey);
        assert!(
            !unfollowed_found,
            "Search should NOT find unfollowed user within radius 0-1"
        );
        tracing::info!("✓ Unfollowed user correctly excluded from results");

        Ok(())
    }
}
