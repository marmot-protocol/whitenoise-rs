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

/// Tests incremental radius search: first search (0,1) finds nothing,
/// then a second search (2,2) finds the target at radius 2 only.
///
/// This validates that `radius_start == radius_end` works correctly
/// for single-layer searches, enabling a UX pattern where nearby
/// results are searched first, then expanded on demand.
///
/// ```text
/// Searcher --follows--> MiddleUser --follows--> TargetUser
/// ```
pub struct SearchIncrementalRadiusTestCase {
    account_name: String,
}

impl SearchIncrementalRadiusTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for SearchIncrementalRadiusTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let account = context.get_account(&self.account_name)?;
        let searcher_pubkey = account.pubkey;

        // --- Set up the social graph: Searcher → MiddleUser → TargetUser ---

        let middle_keys = Keys::generate();
        let middle_pubkey = middle_keys.public_key();

        let target_keys = Keys::generate();
        let target_pubkey = target_keys.public_key();
        let target_name = "IncrTarget";

        // Publish TargetUser's metadata to relays
        let target_client = create_test_client(&context.dev_relays, target_keys).await?;
        publish_test_metadata(&target_client, target_name, "Incremental radius target").await?;
        target_client.disconnect().await;
        tracing::info!(
            "Published metadata for TargetUser {}",
            &target_pubkey.to_hex()[..8]
        );

        // Publish MiddleUser's metadata and follow list (pointing to TargetUser)
        let middle_client = create_test_client(&context.dev_relays, middle_keys.clone()).await?;
        publish_test_metadata(&middle_client, "IncrMiddle", "Connects searcher to target").await?;
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

        // --- Search 1: radius (0, 1) — should NOT find TargetUser ---

        let sub_r1 = context
            .whitenoise
            .search_users(UserSearchParams {
                query: target_name.to_string(),
                searcher_pubkey,
                radius_start: 0,
                radius_end: 1,
            })
            .await?;

        let updates_r1 = collect_search_updates(sub_r1.updates).await;

        let found_at_r1 = updates_r1
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .any(|r| r.pubkey == target_pubkey);
        assert!(
            !found_at_r1,
            "TargetUser should NOT be found at radius 0-1 (only reachable at radius 2)"
        );
        tracing::info!("✓ TargetUser correctly absent from radius (0, 1) results");

        // --- Search 2: radius (2, 2) — single-layer, SHOULD find TargetUser ---

        let sub_r2 = context
            .whitenoise
            .search_users(UserSearchParams {
                query: target_name.to_string(),
                searcher_pubkey,
                radius_start: 2,
                radius_end: 2,
            })
            .await?;

        let updates_r2 = collect_search_updates(sub_r2.updates).await;

        // Should only emit RadiusStarted for radius 2, not 0 or 1
        let radius_started_events: Vec<u8> = updates_r2
            .iter()
            .filter_map(|u| match u.trigger {
                SearchUpdateTrigger::RadiusStarted { radius } => Some(radius),
                _ => None,
            })
            .collect();
        assert_eq!(
            radius_started_events,
            vec![2],
            "Single-layer search (2, 2) should only emit RadiusStarted for radius 2, got: {:?}",
            radius_started_events
        );
        tracing::info!("✓ RadiusStarted only emitted for radius 2");

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
            "TargetUser should be found at radius (2, 2) via MiddleUser's follow list"
        );

        let result = target_result.unwrap();
        assert_eq!(
            result.radius, 2,
            "TargetUser should be at radius 2 (follow of a follow)"
        );
        tracing::info!(
            "✓ Found '{}' at radius {} via single-layer search (2, 2)",
            target_name,
            result.radius,
        );

        Ok(())
    }
}
