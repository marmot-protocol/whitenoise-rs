//! Public types for user search.

use nostr_sdk::{Metadata, PublicKey};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::matcher::{MatchQuality, MatchedField};

/// A single search result with metadata and social distance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSearchResult {
    /// The user's public key.
    pub pubkey: PublicKey,
    /// The user's metadata (fetched from network or cache, not persisted as User).
    pub metadata: Metadata,
    /// Social distance from searcher (0 = self, 1 = direct follow, etc.).
    pub radius: u8,
    /// Quality of the match (for ordering within same radius).
    pub match_quality: MatchQuality,
    /// The highest-priority field that matched (for ordering within same quality).
    pub best_field: MatchedField,
    /// Which metadata fields matched the query (useful for UI highlighting).
    pub matched_fields: Vec<MatchedField>,
}

impl UserSearchResult {
    /// Returns a sort key for ordering results within the same radius.
    ///
    /// Sorting priority (lower values sort first):
    /// 1. Match quality (Exact < Prefix < Contains)
    /// 2. Field priority (Name < Nip05 < DisplayName < About)
    ///
    /// This follows the `ChatListItem::sort_key()` pattern from chat_list.rs.
    pub fn sort_key(&self) -> (u8, u8) {
        (self.match_quality.priority(), self.best_field.priority())
    }
}

/// Parameters for a user search request.
#[derive(Debug, Clone)]
pub struct UserSearchParams {
    /// The query string to search for.
    pub query: String,
    /// The pubkey of the account performing the search.
    pub searcher_pubkey: PublicKey,
    /// Starting radius for the search (inclusive).
    pub radius_start: u8,
    /// Ending radius for the search (inclusive).
    pub radius_end: u8,
}

/// What triggered a search update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchUpdateTrigger {
    /// Starting to search a new radius level.
    RadiusStarted { radius: u8 },

    /// Found matching results at the current radius.
    ResultsFound,

    /// Completed searching a radius level.
    RadiusCompleted {
        radius: u8,
        total_pubkeys_searched: usize,
    },

    /// Radius was capped due to too many pubkeys (graph explosion mitigation).
    /// Search continues but some pubkeys at this radius were skipped.
    RadiusCapped {
        radius: u8,
        cap: usize,
        actual: usize,
    },

    /// Radius fetch timed out (graph explosion mitigation).
    /// Search continues with partial data for this radius.
    RadiusTimeout { radius: u8 },

    /// Search completed (all radius levels searched or limit reached).
    SearchCompleted {
        final_radius: u8,
        total_results: usize,
    },

    /// Error occurred during search (search may continue with partial results).
    Error { message: String },
}

/// A streaming update during user search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSearchUpdate {
    /// What triggered this update.
    pub trigger: SearchUpdateTrigger,

    /// New results found since last update (if any).
    /// Results within this batch are sorted by match quality, then field priority.
    pub new_results: Vec<UserSearchResult>,

    /// Total results found so far.
    pub total_result_count: usize,
}

/// Result of starting a user search.
///
/// Drop the receiver to cancel the search (implicit cancellation).
pub struct UserSearchSubscription {
    /// Receiver for real-time search updates as results are found.
    pub updates: broadcast::Receiver<UserSearchUpdate>,
}

/// Buffer size for broadcast channel (matches NotificationStreamManager).
pub(crate) const SEARCH_CHANNEL_BUFFER_SIZE: usize = 100;

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::Keys;

    fn create_test_result(quality: MatchQuality, field: MatchedField) -> UserSearchResult {
        UserSearchResult {
            pubkey: Keys::generate().public_key(),
            metadata: Metadata::new().name("test"),
            radius: 1,
            match_quality: quality,
            best_field: field,
            matched_fields: vec![MatchedField::Name],
        }
    }

    #[test]
    fn user_search_result_is_cloneable() {
        let result = create_test_result(MatchQuality::Exact, MatchedField::Name);
        let cloned = result.clone();
        assert_eq!(result.pubkey, cloned.pubkey);
        assert_eq!(result.radius, cloned.radius);
    }

    #[test]
    fn search_update_trigger_equality() {
        let trigger1 = SearchUpdateTrigger::RadiusStarted { radius: 1 };
        let trigger2 = SearchUpdateTrigger::RadiusStarted { radius: 1 };
        let trigger3 = SearchUpdateTrigger::RadiusStarted { radius: 2 };

        assert_eq!(trigger1, trigger2);
        assert_ne!(trigger1, trigger3);
    }

    #[test]
    fn sort_key_quality_dominates_field() {
        // Exact match on About field should sort before Prefix match on Name field
        let exact_about = create_test_result(MatchQuality::Exact, MatchedField::About);
        let prefix_name = create_test_result(MatchQuality::Prefix, MatchedField::Name);

        assert!(exact_about.sort_key() < prefix_name.sort_key());
    }

    #[test]
    fn sort_key_same_quality_uses_field_priority() {
        // Same quality: Name field should sort before About field
        let exact_name = create_test_result(MatchQuality::Exact, MatchedField::Name);
        let exact_about = create_test_result(MatchQuality::Exact, MatchedField::About);

        assert!(exact_name.sort_key() < exact_about.sort_key());
    }

    #[test]
    fn sort_key_can_be_used_for_sorting() {
        let results = vec![
            create_test_result(MatchQuality::Contains, MatchedField::Name),
            create_test_result(MatchQuality::Exact, MatchedField::About),
            create_test_result(MatchQuality::Prefix, MatchedField::DisplayName),
            create_test_result(MatchQuality::Exact, MatchedField::Name),
        ];

        let mut sorted = results.clone();
        sorted.sort_by_key(|r| r.sort_key());

        // Order should be: Exact/Name, Exact/About, Prefix/DisplayName, Contains/Name
        assert_eq!(sorted[0].match_quality, MatchQuality::Exact);
        assert_eq!(sorted[0].best_field, MatchedField::Name);

        assert_eq!(sorted[1].match_quality, MatchQuality::Exact);
        assert_eq!(sorted[1].best_field, MatchedField::About);

        assert_eq!(sorted[2].match_quality, MatchQuality::Prefix);
        assert_eq!(sorted[3].match_quality, MatchQuality::Contains);
    }

    #[test]
    fn user_search_update_serializes() {
        let update = UserSearchUpdate {
            trigger: SearchUpdateTrigger::ResultsFound,
            new_results: vec![create_test_result(MatchQuality::Exact, MatchedField::Name)],
            total_result_count: 1,
        };

        let json = serde_json::to_string(&update).unwrap();
        assert!(json.contains("ResultsFound"));

        let deserialized: UserSearchUpdate = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.trigger, SearchUpdateTrigger::ResultsFound);
        assert_eq!(deserialized.total_result_count, 1);
    }

    #[test]
    fn user_search_params_stores_all_fields() {
        let pubkey = Keys::generate().public_key();
        let params = UserSearchParams {
            query: "alice".to_string(),
            searcher_pubkey: pubkey,
            radius_start: 0,
            radius_end: 3,
        };

        assert_eq!(params.query, "alice");
        assert_eq!(params.searcher_pubkey, pubkey);
        assert_eq!(params.radius_start, 0);
        assert_eq!(params.radius_end, 3);
    }
}
