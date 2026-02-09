//! Metadata matching logic for user search.
//!
//! This module provides types and functions for matching search queries against
//! user metadata, with support for different match qualities and field priorities.

use nostr_sdk::Metadata;
use serde::{Deserialize, Serialize};

/// Which metadata field matched the query.
///
/// Fields are ordered by priority (highest to lowest):
/// 1. Name - the username (@handle)
/// 2. Nip05 - verified identifier (user@domain.com)
/// 3. DisplayName - human-readable display name
/// 4. About - bio/description (lowest priority)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchedField {
    /// Username (name field in metadata).
    Name,
    /// NIP-05 identifier (user@domain.com).
    Nip05,
    /// Display name.
    DisplayName,
    /// About/bio text.
    About,
}

impl MatchedField {
    /// Returns priority for sorting (lower = higher priority).
    ///
    /// Used in `UserSearchResult::sort_key()` to determine result ordering
    /// when match quality is the same.
    pub fn priority(&self) -> u8 {
        match self {
            Self::Name => 0,
            Self::Nip05 => 1,
            Self::DisplayName => 2,
            Self::About => 3,
        }
    }
}

/// Quality of match against a field.
///
/// Match qualities are ordered by precedence:
/// 1. Exact - query exactly equals field value (case-insensitive)
/// 2. Prefix - field starts with query (case-insensitive)
/// 3. Contains - field contains query somewhere (case-insensitive)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchQuality {
    /// Query exactly equals the field value (case-insensitive).
    Exact,
    /// Field starts with the query (case-insensitive).
    Prefix,
    /// Field contains the query somewhere (case-insensitive).
    Contains,
}

impl MatchQuality {
    /// Returns priority for sorting (lower = higher priority).
    ///
    /// Used in `UserSearchResult::sort_key()` to determine result ordering.
    /// Match quality takes precedence over field priority.
    pub fn priority(&self) -> u8 {
        match self {
            Self::Exact => 0,
            Self::Prefix => 1,
            Self::Contains => 2,
        }
    }
}

/// Result of matching a query against user metadata.
///
/// A match exists when `quality` is `Some`. Check with `quality.is_some()`
/// rather than a separate boolean — this makes invalid states unrepresentable.
#[derive(Debug, Clone)]
pub struct MatchResult {
    /// The best match quality found (Exact > Prefix > Contains).
    /// `None` means no match.
    pub quality: Option<MatchQuality>,
    /// The highest-priority field that matched (Name > Nip05 > DisplayName > About).
    /// `None` means no match.
    pub best_field: Option<MatchedField>,
    /// All fields that matched - useful for highlighting in UI.
    pub matched_fields: Vec<MatchedField>,
}

/// Check if a user's metadata matches the search query.
///
/// Matches against fields in priority order:
/// 1. name (highest priority)
/// 2. nip05 (username@domain.com)
/// 3. display_name
/// 4. about (description, lowest priority)
///
/// Returns the best match quality and field found.
///
/// ## Ordering Logic
///
/// Results are ordered by:
/// 1. Radius (closest first) - handled at search level
/// 2. Match quality (Exact > Prefix > Contains)
/// 3. Field priority (Name > Nip05 > DisplayName > About)
///
/// ## Examples
///
/// ```
/// use nostr_sdk::Metadata;
/// use whitenoise::whitenoise::user_search::matcher::match_metadata;
///
/// let metadata = Metadata::new().name("alice").about("Developer and artist");
///
/// // Exact match on name
/// let result = match_metadata(&metadata, "alice");
/// assert!(result.quality.is_some());
///
/// // Contains match on about
/// let result = match_metadata(&metadata, "developer");
/// assert!(result.quality.is_some());
///
/// // No match
/// let result = match_metadata(&metadata, "bob");
/// assert!(result.quality.is_none());
/// ```
pub fn match_metadata(metadata: &Metadata, query: &str) -> MatchResult {
    let query = query.trim();
    if query.is_empty() {
        return MatchResult {
            quality: None,
            best_field: None,
            matched_fields: Vec::new(),
        };
    }

    let query_lower = query.to_lowercase();

    // Fields to check in priority order
    let fields: [(MatchedField, Option<&str>); 4] = [
        (MatchedField::Name, metadata.name.as_deref()),
        (MatchedField::Nip05, metadata.nip05.as_deref()),
        (MatchedField::DisplayName, metadata.display_name.as_deref()),
        (MatchedField::About, metadata.about.as_deref()),
    ];

    let mut best_quality: Option<MatchQuality> = None;
    let mut best_field: Option<MatchedField> = None;
    let mut matched_fields = Vec::new();

    for (field_enum, field_value) in fields {
        if let Some(value) = field_value {
            let value_lower = value.to_lowercase();

            let quality = if value_lower == query_lower {
                Some(MatchQuality::Exact)
            } else if value_lower.starts_with(&query_lower) {
                Some(MatchQuality::Prefix)
            } else if value_lower.contains(&query_lower) {
                Some(MatchQuality::Contains)
            } else {
                None
            };

            if let Some(q) = quality {
                matched_fields.push(field_enum);

                // Update best match if this is better
                // Better means: higher quality (lower priority value), or same quality but higher field priority
                let is_better_match = match (best_quality, best_field) {
                    (None, _) => true,
                    (Some(prev_q), Some(prev_f)) => {
                        q.priority() < prev_q.priority()
                            || (q.priority() == prev_q.priority()
                                && field_enum.priority() < prev_f.priority())
                    }
                    (Some(prev_q), None) => q.priority() <= prev_q.priority(),
                };

                if is_better_match {
                    best_quality = Some(q);
                    best_field = Some(field_enum);
                }
            }
        }
    }

    MatchResult {
        quality: best_quality,
        best_field,
        matched_fields,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_quality_exact_has_lowest_priority_value() {
        assert!(MatchQuality::Exact.priority() < MatchQuality::Prefix.priority());
        assert!(MatchQuality::Prefix.priority() < MatchQuality::Contains.priority());
    }

    #[test]
    fn matched_field_name_has_lowest_priority_value() {
        assert!(MatchedField::Name.priority() < MatchedField::Nip05.priority());
        assert!(MatchedField::Nip05.priority() < MatchedField::DisplayName.priority());
        assert!(MatchedField::DisplayName.priority() < MatchedField::About.priority());
    }

    #[test]
    fn tuple_ordering_quality_dominates_field() {
        // When comparing tuples, quality (first element) takes precedence
        // Exact match on About field should sort before Prefix match on Name field
        let exact_about = (
            MatchQuality::Exact.priority(),
            MatchedField::About.priority(),
        );
        let prefix_name = (
            MatchQuality::Prefix.priority(),
            MatchedField::Name.priority(),
        );
        assert!(exact_about < prefix_name);
    }

    #[test]
    fn same_quality_uses_field_priority() {
        // With same quality, field priority determines order
        // Exact on Name should sort before Exact on About
        let exact_name = (
            MatchQuality::Exact.priority(),
            MatchedField::Name.priority(),
        );
        let exact_about = (
            MatchQuality::Exact.priority(),
            MatchedField::About.priority(),
        );
        assert!(exact_name < exact_about);
    }

    #[test]
    fn matched_field_serializes_correctly() {
        let json = serde_json::to_string(&MatchedField::Name).unwrap();
        assert_eq!(json, "\"Name\"");

        let deserialized: MatchedField = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, MatchedField::Name);
    }

    #[test]
    fn match_quality_serializes_correctly() {
        let json = serde_json::to_string(&MatchQuality::Prefix).unwrap();
        assert_eq!(json, "\"Prefix\"");

        let deserialized: MatchQuality = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, MatchQuality::Prefix);
    }

    // Tests for match_metadata function

    #[test]
    fn no_match_returns_none() {
        let metadata = Metadata::new().name("alice");
        let result = match_metadata(&metadata, "bob");

        assert!(result.quality.is_none());
        assert!(result.best_field.is_none());
        assert!(result.matched_fields.is_empty());
    }

    #[test]
    fn exact_match_on_name() {
        let metadata = Metadata::new().name("alice");
        let result = match_metadata(&metadata, "alice");

        assert_eq!(result.quality, Some(MatchQuality::Exact));
        assert_eq!(result.best_field, Some(MatchedField::Name));
        assert_eq!(result.matched_fields, vec![MatchedField::Name]);
    }

    #[test]
    fn prefix_match_on_name() {
        let metadata = Metadata::new().name("alice");
        let result = match_metadata(&metadata, "ali");

        assert_eq!(result.quality, Some(MatchQuality::Prefix));
        assert_eq!(result.best_field, Some(MatchedField::Name));
    }

    #[test]
    fn contains_match_on_name() {
        let metadata = Metadata::new().name("alice");
        let result = match_metadata(&metadata, "lic");

        assert_eq!(result.quality, Some(MatchQuality::Contains));
        assert_eq!(result.best_field, Some(MatchedField::Name));
    }

    #[test]
    fn match_is_case_insensitive() {
        let metadata = Metadata::new().name("Alice");
        let result = match_metadata(&metadata, "ALICE");

        assert_eq!(result.quality, Some(MatchQuality::Exact));
        assert_eq!(result.best_field, Some(MatchedField::Name));
    }

    #[test]
    fn best_quality_wins_over_field_priority() {
        // Exact match on About should beat Prefix match on Name
        let metadata = Metadata::new().name("alice_test").about("alice");

        let result = match_metadata(&metadata, "alice");

        // "alice" is exact match on about, prefix match on name
        // Exact (priority 0) beats Prefix (priority 1)
        assert_eq!(result.quality, Some(MatchQuality::Exact));
        assert_eq!(result.best_field, Some(MatchedField::About));
    }

    #[test]
    fn same_quality_uses_field_priority_in_metadata() {
        // Exact match on Name should beat Exact match on About
        let metadata = Metadata::new().name("alice").about("alice");

        let result = match_metadata(&metadata, "alice");

        assert_eq!(result.quality, Some(MatchQuality::Exact));
        // Name has higher priority than About
        assert_eq!(result.best_field, Some(MatchedField::Name));
    }

    #[test]
    fn multiple_fields_match_all_recorded() {
        let metadata = Metadata::new()
            .name("alice")
            .display_name("Alice Wonderland")
            .about("Alice is a developer");

        let result = match_metadata(&metadata, "alice");

        // All three fields should be recorded as matching
        assert!(result.matched_fields.contains(&MatchedField::Name));
        assert!(result.matched_fields.contains(&MatchedField::DisplayName));
        assert!(result.matched_fields.contains(&MatchedField::About));
    }

    #[test]
    fn empty_query_matches_nothing() {
        let metadata = Metadata::new().name("alice");
        let result = match_metadata(&metadata, "");

        assert!(result.quality.is_none());
    }

    #[test]
    fn whitespace_only_query_matches_nothing() {
        let metadata = Metadata::new().name("alice").about("has spaces in it");

        assert!(match_metadata(&metadata, "   ").quality.is_none());
        assert!(match_metadata(&metadata, "\t").quality.is_none());
        assert!(match_metadata(&metadata, " \n ").quality.is_none());
    }

    #[test]
    fn query_with_leading_trailing_whitespace_still_matches() {
        let metadata = Metadata::new().name("alice");
        let result = match_metadata(&metadata, "  alice  ");

        assert_eq!(result.quality, Some(MatchQuality::Exact));
    }

    #[test]
    fn empty_metadata_matches_nothing() {
        let metadata = Metadata::new();
        let result = match_metadata(&metadata, "alice");

        assert!(result.quality.is_none());
    }

    #[test]
    fn nip05_match() {
        let metadata = Metadata::new().nip05("alice@example.com");
        let result = match_metadata(&metadata, "alice@example.com");

        assert_eq!(result.quality, Some(MatchQuality::Exact));
        assert_eq!(result.best_field, Some(MatchedField::Nip05));
    }

    #[test]
    fn display_name_match() {
        let metadata = Metadata::new().display_name("Alice Wonderland");
        let result = match_metadata(&metadata, "wonderland");

        assert_eq!(result.quality, Some(MatchQuality::Contains));
        assert_eq!(result.best_field, Some(MatchedField::DisplayName));
    }

    #[test]
    fn about_match() {
        let metadata = Metadata::new().about("Developer and artist");
        let result = match_metadata(&metadata, "developer");

        assert_eq!(result.quality, Some(MatchQuality::Prefix));
        assert_eq!(result.best_field, Some(MatchedField::About));
    }
}
