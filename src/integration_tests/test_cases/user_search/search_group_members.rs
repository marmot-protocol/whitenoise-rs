use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::user_search::{SearchUpdateTrigger, UserSearchParams};

use super::helpers::collect_search_updates;

/// Tests that search finds group co-members at radius 1, even without a follow relationship.
///
/// Sets up:
/// - Searcher creates a group with a member account (via CreateGroupTestCase in the scenario)
/// - The member is NOT followed by the searcher
/// - Metadata is set on the member's account so search can match against it
///
/// Verifies:
/// 1. The member is found at radius 1 via group co-membership
/// 2. Search lifecycle events fire correctly
pub struct SearchGroupMembersTestCase {
    searcher_account_name: String,
    member_account_name: String,
}

impl SearchGroupMembersTestCase {
    pub fn new(searcher_account_name: &str, member_account_name: &str) -> Self {
        Self {
            searcher_account_name: searcher_account_name.to_string(),
            member_account_name: member_account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for SearchGroupMembersTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let searcher = context.get_account(&self.searcher_account_name)?;
        let searcher_pubkey = searcher.pubkey;
        let member = context.get_account(&self.member_account_name)?;
        let member_pubkey = member.pubkey;

        // Set metadata on the member's account so search can match it.
        let member_name = "GroupBuddy";
        let metadata = nostr_sdk::Metadata::new()
            .name(member_name)
            .about("A group co-member");
        member
            .update_metadata(&metadata, context.whitenoise)
            .await?;
        tracing::info!(
            "Set metadata for member {} ('{}')",
            &member_pubkey.to_hex()[..8],
            member_name
        );

        // The searcher already has an accepted group with the member
        // (set up by CreateGroupTestCase in the scenario). No follow relationship exists.

        // Search at radius 0-1
        let subscription = context
            .whitenoise
            .search_users(UserSearchParams {
                query: member_name.to_string(),
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

        // Verify the group member was found at radius 1
        let member_result = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .find(|r| r.pubkey == member_pubkey);

        assert!(
            member_result.is_some(),
            "Search should find group co-member at radius 1 (member pubkey: {})",
            &member_pubkey.to_hex()[..8]
        );

        let result = member_result.unwrap();
        assert_eq!(
            result.radius, 1,
            "Group co-member should be at radius 1, got radius {}",
            result.radius
        );
        tracing::info!(
            "✓ Found group co-member '{}' at radius {} (match: {:?})",
            member_name,
            result.radius,
            result.match_quality
        );

        Ok(())
    }
}
