//! Graph traversal helpers for user search.
//!
//! This module provides functions for fetching follows and metadata with a
//! cache hierarchy to minimize network requests:
//!
//! 1. Account.follows() for local accounts
//! 2. cached_graph_users table for recently fetched data
//! 3. Network fetch with caching for new data

use std::collections::{HashMap, HashSet};

use futures::stream::{self, StreamExt};
use nostr_sdk::{Filter, Kind, Metadata, PublicKey};

/// Maximum authors to include in a single relay filter query.
/// Most relays silently reject filters with more than ~40 authors.
const MAX_AUTHORS_PER_FILTER: usize = 40;

/// Maximum concurrent relay requests when fetching in chunks.
/// Limits resource usage on mobile and avoids relay rate-limiting.
const MAX_CONCURRENT_RELAY_REQUESTS: usize = 5;

/// Timeout for relay requests during search graph traversal.
/// Longer than the default 5s because search fetches large batches of authors
/// that compete with background sync for relay connections.
const SEARCH_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::cached_graph_user::CachedGraphUser;
use crate::whitenoise::users::User;

/// Batch fetch metadata for multiple pubkeys.
///
/// Uses the same cache hierarchy as single fetch but optimized for batch operations:
/// 1. Check User table (skip entries with empty metadata)
/// 2. Batch query cached_graph_users table
/// 3. Single batched network request for all cache misses
///
/// Returns a map of pubkey -> metadata. Pubkeys for which no metadata could be
/// retrieved (or whose metadata is empty) are omitted from the map.
pub(super) async fn get_metadata_batch(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> HashMap<PublicKey, Metadata> {
    let mut results: HashMap<PublicKey, Metadata> = HashMap::new();

    // 1. Batch query User table (skip empty metadata — background sync may not have completed)
    let users = User::find_by_pubkeys(pubkeys, &whitenoise.database)
        .await
        .unwrap_or_default();

    for user in users {
        if user.metadata != Metadata::new() {
            results.insert(user.pubkey, user.metadata);
        }
    }

    let mut remaining: Vec<PublicKey> = pubkeys
        .iter()
        .filter(|pk| !results.contains_key(pk))
        .copied()
        .collect();

    if remaining.is_empty() {
        return results;
    }

    // 2. Batch query cache (skip None and empty metadata)
    if let Ok(cached_users) =
        CachedGraphUser::find_fresh_batch(&remaining, &whitenoise.database).await
    {
        for cached in &cached_users {
            if let Some(ref m) = cached.metadata {
                if *m != Metadata::new() {
                    results.insert(cached.pubkey, m.clone());
                }
            }
        }
        remaining.retain(|pk| !results.contains_key(pk));
    }

    if remaining.is_empty() {
        return results;
    }
    // 3. Batch network fetch for cache misses (metadata only)
    let fetched = fetch_and_cache_metadata_batch(whitenoise, &remaining).await;
    for cached in fetched {
        if let Some(ref m) = cached.metadata {
            if *m != Metadata::new() {
                results.insert(cached.pubkey, m.clone());
            }
        }
    }

    results
}

/// Batch fetch follows for multiple pubkeys.
///
/// Uses the same cache hierarchy as single fetch but optimized for batch operations:
/// 1. Check local accounts
/// 2. Batch query cached_graph_users table
/// 3. Single network request for all cache misses
///
/// Returns a map of pubkey -> follows list. Pubkeys for which no data could be
/// retrieved are omitted from the map.
pub(super) async fn get_follows_batch(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> HashMap<PublicKey, Vec<PublicKey>> {
    let mut results: HashMap<PublicKey, Vec<PublicKey>> = HashMap::new();
    let mut remaining: HashSet<PublicKey> = pubkeys.iter().copied().collect();

    // 1. Check local accounts
    for pk in pubkeys {
        if let Ok(account) = Account::find_by_pubkey(pk, &whitenoise.database).await
            && let Ok(follows) = account.follows(&whitenoise.database).await
        {
            results.insert(*pk, follows.into_iter().map(|u| u.pubkey).collect());
            remaining.remove(pk);
        }
    }

    if remaining.is_empty() {
        return results;
    }

    // 2. Batch query cache (skip entries where follows is None — not yet fetched)
    let remaining_vec: Vec<PublicKey> = remaining.iter().copied().collect();
    if let Ok(cached_users) =
        CachedGraphUser::find_fresh_batch(&remaining_vec, &whitenoise.database).await
    {
        for cached in cached_users {
            if let Some(follows) = cached.follows {
                results.insert(cached.pubkey, follows);
                remaining.remove(&cached.pubkey);
            }
        }
    }

    if remaining.is_empty() {
        return results;
    }

    // 3. Batch network fetch for cache misses (follows only)
    let needs_fetch: Vec<PublicKey> = remaining.into_iter().collect();
    let fetched = fetch_and_cache_follows_batch(whitenoise, &needs_fetch).await;
    for cached in fetched {
        if let Some(follows) = cached.follows {
            results.insert(cached.pubkey, follows);
        }
    }

    results
}

/// Extract followed pubkeys from a contact list event.
///
/// Parses all `p` tags from the event, returning the pubkeys of followed users.
fn parse_follows_from_event(event: &nostr_sdk::Event) -> Vec<PublicKey> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            if tag.kind() == nostr_sdk::TagKind::p() {
                tag.content().and_then(|c| PublicKey::parse(c).ok())
            } else {
                None
            }
        })
        .collect()
}

/// What kind of data to fetch from relays.
enum FetchMode {
    MetadataOnly,
    FollowsOnly,
}

/// Batch fetch metadata from network and persist to cache (metadata only).
///
/// Uses partial upsert so existing follows data is preserved.
pub(super) async fn fetch_and_cache_metadata_batch(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> Vec<CachedGraphUser> {
    fetch_and_cache_batch_inner(whitenoise, pubkeys, FetchMode::MetadataOnly).await
}

/// Batch fetch follows from network and persist to cache (follows only).
///
/// Uses partial upsert so existing metadata is preserved.
pub(super) async fn fetch_and_cache_follows_batch(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> Vec<CachedGraphUser> {
    fetch_and_cache_batch_inner(whitenoise, pubkeys, FetchMode::FollowsOnly).await
}

/// Inner implementation for split fetch-and-cache.
///
/// Fetches only the requested kind from relays, then uses partial upsert
/// to avoid clobbering the other field in the cache.
async fn fetch_and_cache_batch_inner(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
    mode: FetchMode,
) -> Vec<CachedGraphUser> {
    if pubkeys.is_empty() {
        return Vec::new();
    }

    let all_relays: Vec<_> = whitenoise.nostr.client.relays().await.into_keys().collect();
    if all_relays.is_empty() {
        return Vec::new();
    }

    let kind = match mode {
        FetchMode::MetadataOnly => Kind::Metadata,
        FetchMode::FollowsOnly => Kind::ContactList,
    };

    // Fetch events in chunks with bounded concurrency to avoid overly large
    // filter queries while limiting relay load and mobile resource usage
    let chunks: Vec<Vec<PublicKey>> = pubkeys
        .chunks(MAX_AUTHORS_PER_FILTER)
        .map(|c| c.to_vec())
        .collect();

    let chunk_results: Vec<_> = stream::iter(chunks.into_iter().map(|chunk| {
        let relays = all_relays.clone();
        async move {
            let filter = Filter::new().authors(chunk).kinds([kind]);
            whitenoise
                .nostr
                .client
                .fetch_events_from(relays, filter, SEARCH_FETCH_TIMEOUT)
                .await
        }
    }))
    .buffer_unordered(MAX_CONCURRENT_RELAY_REQUESTS)
    .collect()
    .await;

    let mut events_by_author: HashMap<PublicKey, Vec<_>> = HashMap::new();

    for result in chunk_results {
        let events = match result {
            Ok(events) => events,
            Err(e) => {
                tracing::debug!(
                    target: "whitenoise::user_search::graph",
                    "Batch fetch failed for chunk: {}",
                    e
                );
                continue;
            }
        };

        for event in events.iter() {
            events_by_author
                .entry(event.pubkey)
                .or_default()
                .push(event.clone());
        }
    }

    // Build and cache results using partial upserts.
    // We cache "not found" as empty defaults (Metadata::new() / vec![]) so that
    // subsequent searches get a cache hit instead of re-fetching from relays.
    let mut results = Vec::new();
    for pk in pubkeys {
        let latest = events_by_author
            .get(pk)
            .and_then(|events| events.iter().max_by_key(|e| e.created_at));

        let cache_result = match mode {
            FetchMode::MetadataOnly => {
                let metadata = latest
                    .and_then(|e| serde_json::from_str::<Metadata>(&e.content).ok())
                    .unwrap_or_default();
                CachedGraphUser::upsert_metadata_only(pk, &metadata, &whitenoise.database).await
            }
            FetchMode::FollowsOnly => {
                let follows = latest
                    .map(parse_follows_from_event)
                    .unwrap_or_default();
                CachedGraphUser::upsert_follows_only(pk, &follows, &whitenoise.database).await
            }
        };

        match cache_result {
            Ok(saved) => results.push(saved),
            Err(e) => {
                tracing::debug!(
                    target: "whitenoise::user_search::graph",
                    "Failed to cache user {}: {}",
                    pk.to_hex(),
                    e
                );
            }
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use nostr_sdk::Keys;

    // --- get_follows_batch tests ---

    #[tokio::test]
    async fn get_follows_batch_returns_empty_for_empty_input() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let result = get_follows_batch(&whitenoise, &[]).await;

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn get_follows_batch_uses_account_for_local_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();
        let target = Keys::generate().public_key();
        whitenoise.follow_user(&account, &target).await.unwrap();

        let result = get_follows_batch(&whitenoise, &[account.pubkey]).await;

        assert_eq!(result.len(), 1);
        let follows = result.get(&account.pubkey).unwrap();
        assert!(follows.contains(&target));
    }

    #[tokio::test]
    async fn get_follows_batch_uses_cache() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys1 = Keys::generate();
        let keys2 = Keys::generate();
        let follow1 = Keys::generate().public_key();
        let follow2 = Keys::generate().public_key();

        let cached1 = CachedGraphUser::new(
            keys1.public_key(),
            Some(Metadata::new()),
            Some(vec![follow1]),
        );
        cached1.upsert(&whitenoise.database).await.unwrap();

        let cached2 = CachedGraphUser::new(
            keys2.public_key(),
            Some(Metadata::new()),
            Some(vec![follow2]),
        );
        cached2.upsert(&whitenoise.database).await.unwrap();

        let result =
            get_follows_batch(&whitenoise, &[keys1.public_key(), keys2.public_key()]).await;

        assert_eq!(result.len(), 2);
        assert!(result.get(&keys1.public_key()).unwrap().contains(&follow1));
        assert!(result.get(&keys2.public_key()).unwrap().contains(&follow2));
    }

    #[tokio::test]
    async fn get_follows_batch_handles_mixed_sources() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Local account
        let account = whitenoise.create_identity().await.unwrap();
        let account_follow = Keys::generate().public_key();
        whitenoise
            .follow_user(&account, &account_follow)
            .await
            .unwrap();

        // Cached user
        let cached_pk = Keys::generate().public_key();
        let cached_follow = Keys::generate().public_key();
        let cached =
            CachedGraphUser::new(cached_pk, Some(Metadata::new()), Some(vec![cached_follow]));
        cached.upsert(&whitenoise.database).await.unwrap();

        let result = get_follows_batch(&whitenoise, &[account.pubkey, cached_pk]).await;

        assert_eq!(result.len(), 2);
        assert!(
            result
                .get(&account.pubkey)
                .unwrap()
                .contains(&account_follow)
        );
        assert!(result.get(&cached_pk).unwrap().contains(&cached_follow));
    }

    #[tokio::test]
    async fn get_follows_batch_attempts_network_fetch_for_cache_miss() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let unknown_pk = Keys::generate().public_key();

        let result = get_follows_batch(&whitenoise, &[unknown_pk]).await;

        // Result may be empty or contain an entry with empty follows
        if let Some(follows) = result.get(&unknown_pk) {
            assert!(follows.is_empty());
        }
    }

    #[tokio::test]
    async fn get_follows_batch_handles_empty_relays_gracefully() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Pre-populate cache
        let cached_pk = Keys::generate().public_key();
        let cached = CachedGraphUser::new(cached_pk, Some(Metadata::new()), Some(vec![]));
        cached.upsert(&whitenoise.database).await.unwrap();

        let unknown_pk = Keys::generate().public_key();

        let result = get_follows_batch(&whitenoise, &[cached_pk, unknown_pk]).await;

        assert!(result.contains_key(&cached_pk));
    }

    // --- get_metadata_batch tests ---

    #[tokio::test]
    async fn get_metadata_batch_returns_empty_for_empty_input() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let result = get_metadata_batch(&whitenoise, &[]).await;

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn get_metadata_batch_resolves_from_user_table() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = Keys::generate();
        let user = User {
            id: None,
            pubkey: keys.public_key(),
            metadata: Metadata::new().name("Alice").about("From user table"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        user.save(&whitenoise.database).await.unwrap();

        let result = get_metadata_batch(&whitenoise, &[keys.public_key()]).await;

        assert_eq!(result.len(), 1);
        let m = result.get(&keys.public_key()).unwrap();
        assert_eq!(m.name, Some("Alice".to_string()));
        assert_eq!(m.about, Some("From user table".to_string()));
    }

    #[tokio::test]
    async fn get_metadata_batch_resolves_from_cache() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = Keys::generate();
        let cached = CachedGraphUser::new(
            keys.public_key(),
            Some(Metadata::new().name("Bob").about("From cache")),
            Some(vec![]),
        );
        cached.upsert(&whitenoise.database).await.unwrap();

        let result = get_metadata_batch(&whitenoise, &[keys.public_key()]).await;

        assert_eq!(result.len(), 1);
        let m = result.get(&keys.public_key()).unwrap();
        assert_eq!(m.name, Some("Bob".to_string()));
        assert_eq!(m.about, Some("From cache".to_string()));
    }

    #[tokio::test]
    async fn get_metadata_batch_skips_empty_user_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = Keys::generate();

        // User record with empty metadata
        let user = User {
            id: None,
            pubkey: keys.public_key(),
            metadata: Metadata::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        user.save(&whitenoise.database).await.unwrap();

        // Cache has real metadata
        let cached = CachedGraphUser::new(
            keys.public_key(),
            Some(Metadata::new().name("Alice")),
            Some(vec![]),
        );
        cached.upsert(&whitenoise.database).await.unwrap();

        let result = get_metadata_batch(&whitenoise, &[keys.public_key()]).await;

        assert_eq!(result.len(), 1);
        let m = result.get(&keys.public_key()).unwrap();
        assert_eq!(
            m.name,
            Some("Alice".to_string()),
            "Should return cached metadata, not empty User metadata"
        );
    }

    #[tokio::test]
    async fn get_metadata_batch_resolves_multiple_users_from_user_table() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys_a = Keys::generate();
        let keys_b = Keys::generate();

        for (keys, name) in [(&keys_a, "Alice"), (&keys_b, "Bob")] {
            let user = User {
                id: None,
                pubkey: keys.public_key(),
                metadata: Metadata::new().name(name),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            user.save(&whitenoise.database).await.unwrap();
        }

        let result =
            get_metadata_batch(&whitenoise, &[keys_a.public_key(), keys_b.public_key()]).await;

        assert_eq!(result.len(), 2);
        assert_eq!(
            result.get(&keys_a.public_key()).unwrap().name,
            Some("Alice".to_string())
        );
        assert_eq!(
            result.get(&keys_b.public_key()).unwrap().name,
            Some("Bob".to_string())
        );
    }

    #[tokio::test]
    async fn get_metadata_batch_resolves_more_than_one_chunk_of_cached_entries() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create more entries than MAX_AUTHORS_PER_FILTER to exercise multi-chunk resolution
        let count = 100;
        let mut pubkeys = Vec::with_capacity(count);

        for i in 0..count {
            let keys = Keys::generate();
            let cached = CachedGraphUser::new(
                keys.public_key(),
                Some(Metadata::new().name(format!("User{}", i))),
                Some(vec![]),
            );
            cached.upsert(&whitenoise.database).await.unwrap();
            pubkeys.push(keys.public_key());
        }

        let result = get_metadata_batch(&whitenoise, &pubkeys).await;

        assert_eq!(result.len(), count);
        for (i, pk) in pubkeys.iter().enumerate() {
            assert_eq!(result.get(pk).unwrap().name, Some(format!("User{}", i)),);
        }
    }

    #[tokio::test]
    async fn get_metadata_batch_handles_mixed_sources() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // User with populated metadata (tier 1)
        let user_keys = Keys::generate();
        let user = User {
            id: None,
            pubkey: user_keys.public_key(),
            metadata: Metadata::new().name("FromUser"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        user.save(&whitenoise.database).await.unwrap();

        // Cached user (tier 2)
        let cached_keys = Keys::generate();
        let cached = CachedGraphUser::new(
            cached_keys.public_key(),
            Some(Metadata::new().name("FromCache")),
            Some(vec![]),
        );
        cached.upsert(&whitenoise.database).await.unwrap();

        let result = get_metadata_batch(
            &whitenoise,
            &[user_keys.public_key(), cached_keys.public_key()],
        )
        .await;

        assert_eq!(result.len(), 2);
        assert_eq!(
            result.get(&user_keys.public_key()).unwrap().name,
            Some("FromUser".to_string())
        );
        assert_eq!(
            result.get(&cached_keys.public_key()).unwrap().name,
            Some("FromCache".to_string())
        );
    }
}
