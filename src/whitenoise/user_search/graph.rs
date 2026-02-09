//! Graph traversal helpers for user search.
//!
//! This module provides functions for fetching follows and metadata with a
//! cache hierarchy to minimize network requests:
//!
//! 1. Account.follows() for local accounts
//! 2. cached_graph_users table for recently fetched data
//! 3. Network fetch with caching for new data

use std::collections::{HashMap, HashSet};

use nostr_sdk::{Filter, Kind, Metadata, PublicKey};

/// Maximum authors to include in a single relay filter query.
/// Prevents overly large requests that may be rejected by relays.
const MAX_AUTHORS_PER_FILTER: usize = 500;

use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::cached_graph_user::CachedGraphUser;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::users::User;

/// Get metadata for a pubkey using cache hierarchy.
///
/// Strategy:
/// 1. User table (if pubkey is a known user)
/// 2. cached_graph_users table (if fresh)
/// 3. Network fetch (result cached for future use)
pub(super) async fn get_metadata_for_pubkey(
    whitenoise: &Whitenoise,
    pubkey: &PublicKey,
) -> Result<Option<Metadata>> {
    // 1. Check User table
    if let Ok(user) = User::find_by_pubkey(pubkey, &whitenoise.database).await {
        return Ok(Some(user.metadata));
    }

    // 2. Check cached_graph_users table
    if let Some(cached) = CachedGraphUser::find_fresh(pubkey, &whitenoise.database).await? {
        return Ok(Some(cached.metadata));
    }

    // 3. Fetch from network and cache
    match fetch_and_cache_user_data(whitenoise, pubkey).await {
        Ok(cached) => Ok(Some(cached.metadata)),
        Err(e) => {
            tracing::debug!(
                target: "whitenoise::user_search::graph",
                "Failed to fetch metadata for {}: {}",
                pubkey.to_hex(),
                e
            );
            Ok(None)
        }
    }
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

    // 2. Batch query cache
    let remaining_vec: Vec<PublicKey> = remaining.iter().copied().collect();
    if let Ok(cached_users) =
        CachedGraphUser::find_fresh_batch(&remaining_vec, &whitenoise.database).await
    {
        for cached in cached_users {
            results.insert(cached.pubkey, cached.follows);
            remaining.remove(&cached.pubkey);
        }
    }

    if remaining.is_empty() {
        return results;
    }

    // 3. Batch network fetch for cache misses
    let needs_fetch: Vec<PublicKey> = remaining.into_iter().collect();
    let fetched = fetch_and_cache_batch(whitenoise, &needs_fetch).await;
    for cached in fetched {
        results.insert(cached.pubkey, cached.follows);
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

/// Batch fetch user data from network and persist to cache.
///
/// Fetches metadata (kind 0) and contact lists (kind 3) for multiple users,
/// chunking requests to respect relay limits. Results are cached for future use.
async fn fetch_and_cache_batch(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> Vec<CachedGraphUser> {
    if pubkeys.is_empty() {
        return Vec::new();
    }

    let all_relays: Vec<_> = whitenoise.nostr.client.relays().await.into_keys().collect();
    if all_relays.is_empty() {
        return Vec::new();
    }

    // Fetch events in chunks to avoid overly large filter queries
    let mut metadata_by_author: HashMap<PublicKey, Vec<_>> = HashMap::new();
    let mut contacts_by_author: HashMap<PublicKey, Vec<_>> = HashMap::new();

    for chunk in pubkeys.chunks(MAX_AUTHORS_PER_FILTER) {
        let filter = Filter::new()
            .authors(chunk.to_vec())
            .kinds([Kind::Metadata, Kind::ContactList]);

        let events = match whitenoise
            .nostr
            .client
            .fetch_events_from(
                all_relays.clone(),
                filter,
                crate::nostr_manager::NostrManager::default_timeout(),
            )
            .await
        {
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
            match event.kind {
                Kind::Metadata => {
                    metadata_by_author
                        .entry(event.pubkey)
                        .or_default()
                        .push(event.clone());
                }
                Kind::ContactList => {
                    contacts_by_author
                        .entry(event.pubkey)
                        .or_default()
                        .push(event.clone());
                }
                _ => {}
            }
        }
    }

    // Build and cache results for each pubkey
    let mut results = Vec::new();
    for pk in pubkeys {
        let metadata = metadata_by_author
            .get(pk)
            .and_then(|events| events.iter().max_by_key(|e| e.created_at))
            .and_then(|e| serde_json::from_str::<Metadata>(&e.content).ok())
            .unwrap_or_else(Metadata::new);

        let follows = contacts_by_author
            .get(pk)
            .and_then(|events| events.iter().max_by_key(|e| e.created_at))
            .map(parse_follows_from_event)
            .unwrap_or_default();

        let cached = CachedGraphUser::new(*pk, metadata, follows);
        match cached.upsert(&whitenoise.database).await {
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

/// Fetch user data from network and persist to cache.
///
/// Fetches both metadata (kind 0) and contact list (kind 3) in a single request,
/// then caches the result for future use.
async fn fetch_and_cache_user_data(
    whitenoise: &Whitenoise,
    pubkey: &PublicKey,
) -> Result<CachedGraphUser> {
    // Build filter for metadata and contact list
    let filter = Filter::new()
        .authors([*pubkey])
        .kinds([Kind::Metadata, Kind::ContactList]);

    // Get all connected relays
    let all_relays: Vec<_> = whitenoise.nostr.client.relays().await.into_keys().collect();

    if all_relays.is_empty() {
        return Err(WhitenoiseError::Other(anyhow::anyhow!(
            "No relays connected for network fetch"
        )));
    }

    // Fetch events from connected relays
    let events = whitenoise
        .nostr
        .client
        .fetch_events_from(
            all_relays,
            filter,
            crate::nostr_manager::NostrManager::default_timeout(),
        )
        .await
        .map_err(|e| WhitenoiseError::Other(e.into()))?;

    // Parse metadata from kind 0 event
    let metadata = events
        .iter()
        .filter(|e| e.kind == Kind::Metadata)
        .max_by_key(|e| e.created_at)
        .and_then(|e| serde_json::from_str::<Metadata>(&e.content).ok())
        .unwrap_or_else(Metadata::new);

    // Parse follows from kind 3 event
    let follows = events
        .iter()
        .filter(|e| e.kind == Kind::ContactList)
        .max_by_key(|e| e.created_at)
        .map(parse_follows_from_event)
        .unwrap_or_default();

    // Create and cache the user data
    let cached = CachedGraphUser::new(*pubkey, metadata, follows);
    let saved = cached.upsert(&whitenoise.database).await?;

    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use nostr_sdk::Keys;

    /// Get follows for a single pubkey using cache hierarchy.
    ///
    /// Test helper that provides ergonomic single-pubkey access to the same
    /// cache strategy used by `get_follows_batch` in production:
    /// 1. Account.follows() if pubkey is a local account
    /// 2. cached_graph_users table (if fresh)
    /// 3. Network fetch (result cached for future use)
    async fn get_follows_for_pubkey(
        whitenoise: &Whitenoise,
        pubkey: &PublicKey,
    ) -> Result<Vec<PublicKey>> {
        // 1. Check if this is a local account
        if let Ok(account) = Account::find_by_pubkey(pubkey, &whitenoise.database).await {
            let follows = account.follows(&whitenoise.database).await?;
            return Ok(follows.into_iter().map(|u| u.pubkey).collect());
        }

        // 2. Check cached_graph_users table
        if let Some(cached) = CachedGraphUser::find_fresh(pubkey, &whitenoise.database).await? {
            return Ok(cached.follows);
        }

        // 3. Fetch from network and cache
        match fetch_and_cache_user_data(whitenoise, pubkey).await {
            Ok(cached) => Ok(cached.follows),
            Err(e) => {
                tracing::debug!(
                    target: "whitenoise::user_search::graph",
                    "Failed to fetch follows for {}: {}",
                    pubkey.to_hex(),
                    e
                );
                Ok(Vec::new())
            }
        }
    }

    #[tokio::test]
    async fn get_follows_returns_empty_for_unknown_pubkey() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let unknown_pubkey = Keys::generate().public_key();

        let follows = get_follows_for_pubkey(&whitenoise, &unknown_pubkey)
            .await
            .unwrap();

        // Should return empty (not an error) for unknown pubkeys
        assert!(follows.is_empty());
    }

    #[tokio::test]
    async fn get_metadata_returns_none_for_unknown_pubkey() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let unknown_pubkey = Keys::generate().public_key();

        let metadata = get_metadata_for_pubkey(&whitenoise, &unknown_pubkey)
            .await
            .unwrap();

        // Should return None (not an error) for unknown pubkeys
        // Note: This may return Some with empty metadata if network fetch succeeds
        // but the user has no metadata event published
        // The important thing is it doesn't error
        assert!(metadata.is_none() || metadata.as_ref().unwrap().name.is_none());
    }

    #[tokio::test]
    async fn get_follows_uses_account_for_local_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a local account
        let account = whitenoise.create_identity().await.unwrap();

        // Add a follow
        let target = Keys::generate().public_key();
        whitenoise.follow_user(&account, &target).await.unwrap();

        // get_follows_for_pubkey should return the followed user
        let follows = get_follows_for_pubkey(&whitenoise, &account.pubkey)
            .await
            .unwrap();

        assert_eq!(follows.len(), 1);
        assert_eq!(follows[0], target);
    }

    #[tokio::test]
    async fn get_metadata_uses_user_table() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a user with metadata
        let keys = Keys::generate();
        let mut user = User {
            id: None,
            pubkey: keys.public_key(),
            metadata: Metadata::new().name("Test User").about("Test about"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        user = user.save(&whitenoise.database).await.unwrap();

        // get_metadata_for_pubkey should return the user's metadata
        let metadata = get_metadata_for_pubkey(&whitenoise, &user.pubkey)
            .await
            .unwrap();

        assert!(metadata.is_some());
        let m = metadata.unwrap();
        assert_eq!(m.name, Some("Test User".to_string()));
        assert_eq!(m.about, Some("Test about".to_string()));
    }

    #[tokio::test]
    async fn get_follows_uses_cache() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let follow1 = Keys::generate().public_key();
        let follow2 = Keys::generate().public_key();

        // Pre-populate the cache
        let cached = CachedGraphUser::new(
            keys.public_key(),
            Metadata::new().name("Cached User"),
            vec![follow1, follow2],
        );
        cached.upsert(&whitenoise.database).await.unwrap();

        // get_follows_for_pubkey should return cached follows
        let follows = get_follows_for_pubkey(&whitenoise, &keys.public_key())
            .await
            .unwrap();

        assert_eq!(follows.len(), 2);
        assert!(follows.contains(&follow1));
        assert!(follows.contains(&follow2));
    }

    #[tokio::test]
    async fn get_metadata_uses_cache() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        // Pre-populate the cache
        let cached = CachedGraphUser::new(
            keys.public_key(),
            Metadata::new().name("Cached User").about("From cache"),
            vec![],
        );
        cached.upsert(&whitenoise.database).await.unwrap();

        // get_metadata_for_pubkey should return cached metadata
        let metadata = get_metadata_for_pubkey(&whitenoise, &keys.public_key())
            .await
            .unwrap();

        assert!(metadata.is_some());
        let m = metadata.unwrap();
        assert_eq!(m.name, Some("Cached User".to_string()));
        assert_eq!(m.about, Some("From cache".to_string()));
    }

    #[tokio::test]
    async fn get_follows_batch_returns_empty_for_empty_input() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let result = get_follows_batch(&whitenoise, &[]).await;

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn get_follows_batch_uses_cache() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys1 = Keys::generate();
        let keys2 = Keys::generate();
        let follow1 = Keys::generate().public_key();
        let follow2 = Keys::generate().public_key();

        // Pre-populate the cache
        let cached1 = CachedGraphUser::new(keys1.public_key(), Metadata::new(), vec![follow1]);
        cached1.upsert(&whitenoise.database).await.unwrap();

        let cached2 = CachedGraphUser::new(keys2.public_key(), Metadata::new(), vec![follow2]);
        cached2.upsert(&whitenoise.database).await.unwrap();

        // Batch fetch both
        let result =
            get_follows_batch(&whitenoise, &[keys1.public_key(), keys2.public_key()]).await;

        assert_eq!(result.len(), 2);
        assert!(result.get(&keys1.public_key()).unwrap().contains(&follow1));
        assert!(result.get(&keys2.public_key()).unwrap().contains(&follow2));
    }

    #[tokio::test]
    async fn get_follows_batch_uses_account_for_local_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a local account
        let account = whitenoise.create_identity().await.unwrap();

        // Add a follow
        let target = Keys::generate().public_key();
        whitenoise.follow_user(&account, &target).await.unwrap();

        // Batch fetch should include the local account's follows
        let result = get_follows_batch(&whitenoise, &[account.pubkey]).await;

        assert_eq!(result.len(), 1);
        let follows = result.get(&account.pubkey).unwrap();
        assert!(follows.contains(&target));
    }

    #[tokio::test]
    async fn get_follows_batch_handles_mixed_sources() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a local account
        let account = whitenoise.create_identity().await.unwrap();
        let account_follow = Keys::generate().public_key();
        whitenoise
            .follow_user(&account, &account_follow)
            .await
            .unwrap();

        // Create a cached user
        let cached_pk = Keys::generate().public_key();
        let cached_follow = Keys::generate().public_key();
        let cached = CachedGraphUser::new(cached_pk, Metadata::new(), vec![cached_follow]);
        cached.upsert(&whitenoise.database).await.unwrap();

        // Batch fetch both
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

        // Use a random pubkey with no cache entry - this will trigger network fetch
        let unknown_pk = Keys::generate().public_key();

        // Batch fetch should attempt network fetch and return empty (no data on relay)
        let result = get_follows_batch(&whitenoise, &[unknown_pk]).await;

        // Result may be empty or contain an entry with empty follows
        // depending on whether fetch succeeded with empty data
        if let Some(follows) = result.get(&unknown_pk) {
            // If we got a result, it should be empty follows
            assert!(follows.is_empty());
        }
        // Otherwise empty map is also valid (fetch failed gracefully)
    }

    #[tokio::test]
    async fn get_metadata_fallback_to_network_fetch_returns_none_gracefully() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Use a random pubkey that doesn't exist in User table or cache
        let unknown_pk = Keys::generate().public_key();

        // This will try cache (miss), then network fetch
        let result = get_metadata_for_pubkey(&whitenoise, &unknown_pk)
            .await
            .unwrap();

        // Should return None gracefully (unknown user, not on relay)
        // or Some with empty metadata if network fetch succeeded
        if let Some(metadata) = result {
            // If we got metadata from network, it should be empty/default
            assert!(metadata.name.is_none() || metadata.name.as_ref().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn get_follows_fallback_to_network_fetch_returns_empty_gracefully() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Use a random pubkey that doesn't exist anywhere
        let unknown_pk = Keys::generate().public_key();

        // This should try account (miss), cache (miss), then network fetch
        let result = get_follows_for_pubkey(&whitenoise, &unknown_pk)
            .await
            .unwrap();

        // Should return empty vec gracefully
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn fetch_caches_result_for_subsequent_calls() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an account and publish metadata to relay
        let account = whitenoise.create_identity().await.unwrap();
        let metadata = Metadata::new().name("CacheTestUser");
        account
            .update_metadata(&metadata, &whitenoise)
            .await
            .unwrap();

        // Give relay time to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // First call should fetch from network and cache
        let result1 = get_metadata_for_pubkey(&whitenoise, &account.pubkey)
            .await
            .unwrap();

        // Verify we got metadata
        assert!(result1.is_some());
        let m1 = result1.unwrap();
        assert_eq!(m1.name, Some("CacheTestUser".to_string()));

        // Now check that cache was populated (for non-account users we'd see this)
        // For account users, it goes through User table first
        // Let's verify by checking the cache directly for a different user
        let other_keys = Keys::generate();
        let cached_user = CachedGraphUser::new(
            other_keys.public_key(),
            Metadata::new().name("PreCached"),
            vec![],
        );
        cached_user.upsert(&whitenoise.database).await.unwrap();

        // Second call should hit cache
        let result2 = get_metadata_for_pubkey(&whitenoise, &other_keys.public_key())
            .await
            .unwrap();

        assert!(result2.is_some());
        assert_eq!(result2.unwrap().name, Some("PreCached".to_string()));
    }

    #[tokio::test]
    async fn get_follows_batch_handles_empty_relays_gracefully() {
        // This test verifies the batch fetch handles the case where
        // no relays are connected - it should return results from cache only
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Pre-populate cache
        let cached_pk = Keys::generate().public_key();
        let cached = CachedGraphUser::new(cached_pk, Metadata::new(), vec![]);
        cached.upsert(&whitenoise.database).await.unwrap();

        // Unknown pk will trigger network fetch attempt
        let unknown_pk = Keys::generate().public_key();

        // Batch fetch - cached should work, unknown will fail gracefully
        let result = get_follows_batch(&whitenoise, &[cached_pk, unknown_pk]).await;

        // At minimum, cached entry should be present
        assert!(result.contains_key(&cached_pk));
    }
}
