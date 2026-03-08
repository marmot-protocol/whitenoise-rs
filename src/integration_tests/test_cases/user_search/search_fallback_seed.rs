use async_trait::async_trait;
use nostr_sdk::PublicKey;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::test_fixtures::nostr::{
    JEFF_PUBKEY_HEX, MAX_PUBKEY_HEX, publish_user_search_seed_events,
};
use crate::whitenoise::user_search::{SearchUpdateTrigger, UserSearchParams};

use super::helpers::{collect_search_updates, wait_for_result};

/// Tests that a new account with no follows can discover users via the
/// fallback seed injection.
///
/// When the social graph is empty, the search injects a well-connected seed
/// pubkey (Jeff) as an entrypoint. The pipeline fetches Jeff's metadata and
/// contact list from the local relays (pre-seeded with real events), then
/// expands into his follows. This test verifies:
///
/// 1. Searching "Jeff" finds the seed itself (it's pushed as a candidate)
/// 2. Searching "Max" finds one of Jeff's follows (graph expansion works)
pub struct SearchFallbackSeedTestCase {
    account_name: String,
}

impl SearchFallbackSeedTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for SearchFallbackSeedTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let account = context.get_account(&self.account_name)?;
        let searcher_pubkey = account.pubkey;

        let jeff_pk = PublicKey::parse(JEFF_PUBKEY_HEX).expect("valid Jeff pubkey");
        let max_pk = PublicKey::parse(MAX_PUBKEY_HEX).expect("valid Max pubkey");

        // Seed Jeff's metadata, contact list, and Max's metadata to local relays
        // so the pipeline can resolve them without hitting the public network.
        publish_user_search_seed_events(&context.dev_relays).await?;
        tracing::info!("Seeded fallback events to local relays");

        // --- Search 1: "Jeff" should find the fallback seed itself ---

        let sub_jeff = context
            .whitenoise
            .search_users(UserSearchParams {
                query: "Jeff".to_string(),
                searcher_pubkey,
                radius_start: 0,
                radius_end: 2,
            })
            .await?;

        let updates_jeff = collect_search_updates(sub_jeff.updates).await;

        let completed = updates_jeff
            .iter()
            .any(|u| matches!(u.trigger, SearchUpdateTrigger::SearchCompleted { .. }));
        assert!(completed, "Search should emit SearchCompleted");

        let found_jeff = updates_jeff
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .any(|r| r.pubkey == jeff_pk);

        assert!(
            found_jeff,
            "Searching 'Jeff' should find the fallback seed itself"
        );
        tracing::info!("✓ Found Jeff (fallback seed) via empty-graph injection");

        // --- Search 2: "Max" should find Jeff's follow via graph expansion ---

        let sub_max = context
            .whitenoise
            .search_users(UserSearchParams {
                query: "Max".to_string(),
                searcher_pubkey,
                radius_start: 0,
                radius_end: 3,
            })
            .await?;

        let updates_max = wait_for_result(sub_max.updates, &max_pk).await;

        let found_max = updates_max
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .any(|r| r.pubkey == max_pk);

        assert!(
            found_max,
            "Searching 'Max' should find Jeff's follow via fallback graph expansion"
        );
        tracing::info!("✓ Found Max (Jeff's follow) via fallback graph expansion");

        Ok(())
    }
}
