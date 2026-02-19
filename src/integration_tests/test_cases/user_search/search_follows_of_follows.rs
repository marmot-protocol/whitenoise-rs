use async_trait::async_trait;
use nostr_sdk::Keys;

use crate::WhitenoiseError;
use crate::integration_tests::core::test_clients::{
    create_test_client, publish_follow_list, publish_test_metadata,
};
use crate::integration_tests::core::*;
use crate::whitenoise::user_search::{SearchUpdateTrigger, UserSearchParams};
use crate::whitenoise::users::UserSyncMode;

use super::helpers::collect_search_updates;

/// Tests that search discovers users at radius 2 (follows of follows).
///
/// Sets up a three-hop social graph:
///
/// ```text
/// Searcher --follows--> MiddleUser --follows--> TargetUser
/// ```
///
/// - Searcher follows MiddleUser (created via blocking sync)
/// - MiddleUser publishes a contact list to relays that includes TargetUser
/// - TargetUser publishes metadata to relays
///
/// Verifies:
/// 1. TargetUser is found at radius 2 through MiddleUser's follow list
/// 2. TargetUser is NOT found when searching only radius 0-1
pub struct SearchFollowsOfFollowsTestCase {
    account_name: String,
}

impl SearchFollowsOfFollowsTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for SearchFollowsOfFollowsTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let account = context.get_account(&self.account_name)?;
        let searcher_pubkey = account.pubkey;

        // --- Set up the social graph: Searcher → MiddleUser → TargetUser ---

        let middle_keys = Keys::generate();
        let middle_pubkey = middle_keys.public_key();

        let target_keys = Keys::generate();
        let target_pubkey = target_keys.public_key();
        let target_name = "FoFTarget";

        // Publish TargetUser's metadata to relays
        let target_client = create_test_client(&context.dev_relays, target_keys).await?;
        publish_test_metadata(&target_client, target_name, "A friend of a friend").await?;
        target_client.disconnect().await;
        tracing::info!(
            "Published metadata for TargetUser {}",
            &target_pubkey.to_hex()[..8]
        );

        // Publish MiddleUser's metadata and follow list (pointing to TargetUser)
        let middle_client = create_test_client(&context.dev_relays, middle_keys.clone()).await?;
        publish_test_metadata(&middle_client, "MiddleUser", "Connects searcher to target").await?;
        publish_follow_list(&middle_client, &[target_pubkey]).await?;
        middle_client.disconnect().await;
        tracing::info!(
            "Published metadata + follow list for MiddleUser {} → TargetUser {}",
            &middle_pubkey.to_hex()[..8],
            &target_pubkey.to_hex()[..8]
        );

        // Create MiddleUser in our DB with blocking sync (populates metadata)
        context
            .whitenoise
            .find_or_create_user_by_pubkey(&middle_pubkey, UserSyncMode::Blocking)
            .await?;

        // Searcher follows MiddleUser
        context
            .whitenoise
            .follow_user(account, &middle_pubkey)
            .await?;
        tracing::info!(
            "Searcher {} follows MiddleUser {}",
            &searcher_pubkey.to_hex()[..8],
            &middle_pubkey.to_hex()[..8]
        );

        // --- Verify: TargetUser should NOT appear at radius 0-1 ---

        let subscription_r1 = context
            .whitenoise
            .search_users(UserSearchParams {
                query: target_name.to_string(),
                searcher_pubkey,
                radius_start: 0,
                radius_end: 1,
            })
            .await?;

        let updates_r1 = collect_search_updates(subscription_r1.updates).await;

        let found_at_r1 = updates_r1
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .any(|r| r.pubkey == target_pubkey);
        assert!(
            !found_at_r1,
            "TargetUser should NOT be found at radius 0-1 (only reachable at radius 2)"
        );
        tracing::info!("✓ TargetUser correctly absent from radius 0-1 results");

        // --- Verify: TargetUser SHOULD appear at radius 2 ---

        let subscription_r2 = context
            .whitenoise
            .search_users(UserSearchParams {
                query: target_name.to_string(),
                searcher_pubkey,
                radius_start: 0,
                radius_end: 2,
            })
            .await?;

        let updates_r2 = collect_search_updates(subscription_r2.updates).await;

        let completed = updates_r2
            .iter()
            .any(|u| matches!(u.trigger, SearchUpdateTrigger::SearchCompleted { .. }));
        assert!(completed, "Search should emit SearchCompleted");

        let target_result = updates_r2
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .find(|r| r.pubkey == target_pubkey);

        assert!(
            target_result.is_some(),
            "TargetUser should be found at radius 2 via MiddleUser's follow list"
        );

        let result = target_result.unwrap();
        assert_eq!(
            result.radius, 2,
            "TargetUser should be at radius 2 (follow of a follow)"
        );
        tracing::info!(
            "✓ Found '{}' at radius {} (match: {:?})",
            target_name,
            result.radius,
            result.match_quality
        );

        Ok(())
    }
}
