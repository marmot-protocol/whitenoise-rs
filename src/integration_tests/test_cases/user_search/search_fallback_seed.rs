use async_trait::async_trait;
use nostr_sdk::PublicKey;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::relays::RelayType;
use crate::whitenoise::user_search::{SearchUpdateTrigger, UserSearchParams};

use super::helpers::{collect_search_updates, wait_for_result};

/// Jeff's pubkey (the fallback seed).
const JEFF_PUBKEY: &str = "1739d937dc8c0c7370aa27585938c119e25c41f6c441a5d34c6d38503e3136ef";
/// Max's pubkey (one of Jeff's follows).
const MAX_PUBKEY: &str = "b7ed68b062de6b4a12e51fd5285c1e1e0ed0e5128cda93ab11b4150b55ed32fc";

/// Tests that a new account with no follows can discover users via the
/// fallback seed injection.
///
/// When the social graph is empty, the search injects a well-connected seed
/// pubkey (Jeff) as an entrypoint. The pipeline fetches Jeff's metadata and
/// contact list from the network, then expands into his follows. This test
/// verifies:
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
        // Account has no follows — social graph is empty.

        let jeff_pk = PublicKey::parse(JEFF_PUBKEY).expect("valid Jeff pubkey");
        let max_pk = PublicKey::parse(MAX_PUBKEY).expect("valid Max pubkey");

        // Add public relays so the pipeline can reach Jeff's and Max's data.
        // In debug mode the default relays are local-only.
        let public_relay_urls = [
            "wss://relay.damus.io",
            "wss://relay.primal.net",
            "wss://nos.lol",
        ];
        for url in &public_relay_urls {
            let relay_url = nostr_sdk::RelayUrl::parse(url).expect("valid relay URL");
            let relay = context
                .whitenoise
                .find_or_create_relay_by_url(&relay_url)
                .await?;
            account
                .add_relay(&relay, RelayType::Nip65, context.whitenoise)
                .await?;
        }
        tracing::info!("Connected to public relays for fallback seed test");

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

        // Don't wait for SearchCompleted — Jeff follows hundreds of accounts and
        // draining all batches through tiers 3-5 takes 30-40s. Instead, return as
        // soon as Max appears in a ResultsFound event.
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
