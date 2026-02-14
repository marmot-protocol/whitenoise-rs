//! Cached user data for social graph traversal.
//!
//! This module provides the `CachedGraphUser` type for caching user metadata and
//! follow lists discovered during social graph traversal (web of trust). This is
//! used by the user search feature to avoid creating full `User` records for
//! every user discovered during search.
//!
//! Key differences from `User`:
//! - Does NOT trigger subscriptions or background syncing
//! - Has a 24-hour TTL for freshness
//! - Stores follow list alongside metadata
//! - Is NOT exposed to Flutter/FRB

use chrono::{DateTime, Utc};
use nostr_sdk::{Metadata, PublicKey};

/// Default TTL for cached graph user data (24 hours).
pub const DEFAULT_CACHE_TTL_HOURS: i64 = 24;

/// Cached user data for users discovered via social graph traversal (web of trust).
///
/// These are cached BEFORE any search filtering - they represent the "search space".
/// This is NOT a User - it doesn't trigger subscriptions or background syncing.
#[derive(Clone, PartialEq, Eq)]
pub struct CachedGraphUser {
    /// Database ID (None before save, Some after).
    pub id: Option<i64>,
    /// The user's Nostr public key.
    pub pubkey: PublicKey,
    /// The user's profile metadata (name, display_name, about, etc.).
    pub metadata: Metadata,
    /// List of pubkeys this user follows.
    pub follows: Vec<PublicKey>,
    /// When this cache entry was created.
    pub created_at: DateTime<Utc>,
    /// When this cache entry was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Custom Debug impl to prevent sensitive data from leaking into logs.
impl std::fmt::Debug for CachedGraphUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedGraphUser")
            .field("pubkey", &self.pubkey)
            .finish()
    }
}

impl CachedGraphUser {
    /// Creates a new `CachedGraphUser` with the current timestamp.
    pub fn new(pubkey: PublicKey, metadata: Metadata, follows: Vec<PublicKey>) -> Self {
        let now = Utc::now();
        Self {
            id: None,
            pubkey,
            metadata,
            follows,
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::Keys;

    #[test]
    fn new_creates_with_current_timestamp() {
        let keys = Keys::generate();
        let before = Utc::now();
        let user = CachedGraphUser::new(keys.public_key(), Metadata::new(), vec![]);
        let after = Utc::now();

        assert!(user.created_at >= before);
        assert!(user.created_at <= after);
        assert!(user.updated_at >= before);
        assert!(user.updated_at <= after);
        assert!(user.id.is_none());
    }

    #[test]
    fn stores_metadata_and_follows() {
        let keys = Keys::generate();
        let follow1 = Keys::generate().public_key();
        let follow2 = Keys::generate().public_key();

        let metadata = Metadata::new()
            .name("Alice")
            .display_name("Alice Wonderland")
            .about("Down the rabbit hole");

        let user =
            CachedGraphUser::new(keys.public_key(), metadata.clone(), vec![follow1, follow2]);

        assert_eq!(user.pubkey, keys.public_key());
        assert_eq!(user.metadata.name, Some("Alice".to_string()));
        assert_eq!(
            user.metadata.display_name,
            Some("Alice Wonderland".to_string())
        );
        assert_eq!(
            user.metadata.about,
            Some("Down the rabbit hole".to_string())
        );
        assert_eq!(user.follows.len(), 2);
        assert!(user.follows.contains(&follow1));
        assert!(user.follows.contains(&follow2));
    }
}
