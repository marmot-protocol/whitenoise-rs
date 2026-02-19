//! Graph traversal helpers for user search.
//!
//! This module provides functions for fetching follows and metadata with a
//! multi-tier cache hierarchy to minimize network requests:
//!
//! 1. User table (Account records with metadata)
//! 2. CachedGraphUser table (recently fetched graph data)
//! 3. Network fetch from default relays (single attempt, queue-based retries in consumer)
//! 4. NIP-65 relay list discovery (single attempt, queue-based retries in consumer)
//! 5. Metadata fetch from user's preferred relays (single attempt, queue-based retries)

use std::collections::{HashMap, HashSet};

use nostr_sdk::{Event, Filter, Kind, Metadata, PublicKey, RelayUrl, TagKind};

/// Maximum authors to include in a single relay filter query.
/// Empirical testing shows major relays handle 500+ authors per filter,
/// but we cap at 200 to match PUBKEY_BATCH_SIZE and avoid unnecessary splits.
const MAX_AUTHORS_PER_FILTER: usize = 200;

/// Timeout for relay requests during search graph traversal.
/// Longer than the default 5s because search fetches large batches of authors
/// that compete with background sync for relay connections.
const SEARCH_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Number of times to retry a failed relay request before giving up.
/// Only used by `fetch_events_with_retries` (follows producer).
/// Metadata tiers use queue-based retries managed by their consumers.
const NETWORK_FETCH_RETRIES: usize = 3;

use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::cached_graph_user::CachedGraphUser;
use crate::whitenoise::users::User;

/// Collect all relay URLs the client is connected to.
async fn connected_relays(whitenoise: &Whitenoise) -> Vec<RelayUrl> {
    whitenoise.nostr.client.relays().await.into_keys().collect()
}

// ---------------------------------------------------------------------------
// Metadata tiers
// ---------------------------------------------------------------------------

/// Tier 1: Batch check User table for metadata.
///
/// Returns (found metadata map, remaining pubkeys not found or with empty metadata).
/// Pubkeys with empty metadata (background sync not yet completed) are included in remaining.
pub(super) async fn check_user_table_metadata(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> (HashMap<PublicKey, Metadata>, Vec<PublicKey>) {
    let mut found: HashMap<PublicKey, Metadata> = HashMap::new();

    let users = User::find_by_pubkeys(pubkeys, &whitenoise.database)
        .await
        .unwrap_or_default();

    for user in users {
        if user.metadata != Metadata::new() {
            found.insert(user.pubkey, user.metadata);
        }
    }

    let remaining = pubkeys
        .iter()
        .filter(|pk| !found.contains_key(pk))
        .copied()
        .collect();

    (found, remaining)
}

/// Tier 2: Batch check CachedGraphUser table for metadata.
///
/// Returns (found metadata map, remaining pubkeys not found or with None/empty metadata).
pub(super) async fn check_cache_metadata(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> (HashMap<PublicKey, Metadata>, Vec<PublicKey>) {
    let mut found: HashMap<PublicKey, Metadata> = HashMap::new();
    // Track pubkeys with any cached entry (even empty metadata) so we don't
    // re-forward confirmed-absent users to the network tiers.
    // None = not yet fetched → falls through to T3.
    // Some(Metadata::new()) = fetched, confirmed absent → stop here.
    // Some(real) = fetched with data → matched above.
    let mut cached_pks: HashSet<PublicKey> = HashSet::new();

    if let Ok(cached_users) =
        CachedGraphUser::find_fresh_metadata_batch(pubkeys, &whitenoise.database).await
    {
        for cached in &cached_users {
            if let Some(ref m) = cached.metadata {
                if *m != Metadata::new() {
                    found.insert(cached.pubkey, m.clone());
                }
                // Whether real or empty, this pubkey has been fetched — don't forward.
                cached_pks.insert(cached.pubkey);
            }
            // metadata = None → not yet fetched, will fall through to T3.
        }
    }

    let remaining = pubkeys
        .iter()
        .filter(|pk| !found.contains_key(pk) && !cached_pks.contains(pk))
        .copied()
        .collect();

    (found, remaining)
}

/// Tier 3: Single-attempt metadata fetch from default relays.
///
/// Caches found metadata via partial upsert (preserving follows).
/// Does NOT cache empty defaults — remaining pubkeys flow to tier 4.
///
/// Returns `Ok((found, remaining))` on success (relay responded),
/// or `Err(())` on network error (caller should requeue or forward).
pub(super) async fn try_fetch_network_metadata(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> Result<(HashMap<PublicKey, Metadata>, Vec<PublicKey>), ()> {
    let all_relays = connected_relays(whitenoise).await;
    let filter = Filter::new()
        .authors(pubkeys.to_vec())
        .kinds([Kind::Metadata]);

    let events = whitenoise
        .nostr
        .client
        .fetch_events_from(&all_relays, filter, SEARCH_FETCH_TIMEOUT)
        .await
        .map_err(|e| {
            tracing::debug!(
                target: "whitenoise::user_search::graph",
                "Metadata fetch failed: {}",
                e
            );
        })?;

    let mut found = HashMap::new();
    let mut latest_by_author: HashMap<PublicKey, &Event> = HashMap::new();
    for event in events.iter() {
        latest_by_author
            .entry(event.pubkey)
            .and_modify(|existing| {
                if event.created_at > existing.created_at {
                    *existing = event;
                }
            })
            .or_insert(event);
    }

    for (pk, event) in latest_by_author {
        if let Some(metadata) = serde_json::from_str::<Metadata>(&event.content)
            .ok()
            .filter(|m| *m != Metadata::new())
        {
            let _ =
                CachedGraphUser::upsert_metadata_only(&pk, &metadata, &whitenoise.database).await;
            found.insert(pk, metadata);
        }
    }

    let remaining = pubkeys
        .iter()
        .filter(|pk| !found.contains_key(pk))
        .copied()
        .collect();

    Ok((found, remaining))
}

/// Tier 4: Single-attempt NIP-65 relay list fetch from default relays.
///
/// Returns `Ok(map)` where the map contains pubkey → write relay URLs.
/// Pubkeys without relay list events are absent from the map.
/// Returns `Err(())` on network error (caller should requeue).
pub(super) async fn try_fetch_relay_lists(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> Result<HashMap<PublicKey, Vec<RelayUrl>>, ()> {
    let all_relays = connected_relays(whitenoise).await;
    let filter = Filter::new()
        .authors(pubkeys.to_vec())
        .kinds([Kind::RelayList]);

    let events = whitenoise
        .nostr
        .client
        .fetch_events_from(&all_relays, filter, SEARCH_FETCH_TIMEOUT)
        .await
        .map_err(|e| {
            tracing::debug!(
                target: "whitenoise::user_search::graph",
                "Relay list fetch failed: {}",
                e
            );
        })?;

    let mut latest_by_author: HashMap<PublicKey, &Event> = HashMap::new();
    for event in events.iter() {
        latest_by_author
            .entry(event.pubkey)
            .and_modify(|existing| {
                if event.created_at > existing.created_at {
                    *existing = event;
                }
            })
            .or_insert(event);
    }

    let mut results = HashMap::new();
    for (pk, event) in latest_by_author {
        let relay_urls: Vec<RelayUrl> = parse_write_relays_from_event(event)
            .into_iter()
            .filter_map(|url| RelayUrl::parse(&url).ok())
            .collect();
        if !relay_urls.is_empty() {
            results.insert(pk, relay_urls);
        }
    }

    Ok(results)
}

/// Result of a single-attempt metadata fetch from a user's preferred relays.
pub(super) enum UserRelayResult {
    /// Metadata found on at least one relay.
    Found(Box<Metadata>),
    /// All relays responded but no metadata found (confirmed absence).
    Eose,
    /// All relays failed (network error).
    Error,
}

/// Tier 5: Single-attempt metadata fetch from a user's preferred relays.
///
/// Queries the given relays for Kind 0 metadata for a single pubkey.
/// Caches found metadata. Does NOT cache EOSE or error — the consumer decides.
pub(super) async fn try_fetch_user_relay_metadata(
    whitenoise: &Whitenoise,
    pubkey: &PublicKey,
    relays: &[RelayUrl],
) -> UserRelayResult {
    let filter = Filter::new().authors([*pubkey]).kinds([Kind::Metadata]);

    match whitenoise
        .nostr
        .client
        .fetch_events_from(relays, filter, SEARCH_FETCH_TIMEOUT)
        .await
    {
        Ok(events) => {
            if let Some(metadata) = events
                .iter()
                .max_by_key(|e| e.created_at)
                .and_then(|e| serde_json::from_str::<Metadata>(&e.content).ok())
                .filter(|m| *m != Metadata::new())
            {
                let _ =
                    CachedGraphUser::upsert_metadata_only(pubkey, &metadata, &whitenoise.database)
                        .await;
                UserRelayResult::Found(Box::new(metadata))
            } else {
                UserRelayResult::Eose
            }
        }
        Err(e) => {
            tracing::debug!(
                target: "whitenoise::user_search::graph",
                "User relay metadata fetch failed for {}: {}",
                pubkey,
                e
            );
            UserRelayResult::Error
        }
    }
}

/// Batch fetch metadata for multiple pubkeys (tiers 1-3 combined).
///
/// Convenience function that runs tiers sequentially.
/// Used by tests; the pipeline uses individual tier functions directly.
#[cfg(test)]
pub(super) async fn get_metadata_batch(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> HashMap<PublicKey, Metadata> {
    let (mut results, remaining) = check_user_table_metadata(whitenoise, pubkeys).await;
    if remaining.is_empty() {
        return results;
    }

    let (cache_results, remaining) = check_cache_metadata(whitenoise, &remaining).await;
    results.extend(cache_results);
    if remaining.is_empty() {
        return results;
    }

    if let Ok((network_results, _remaining)) =
        try_fetch_network_metadata(whitenoise, &remaining).await
    {
        results.extend(network_results);
    }
    results
}

// ---------------------------------------------------------------------------
// Follows tiers
// ---------------------------------------------------------------------------

/// Tiers 1+2: Batch check local accounts and cache for follows.
///
/// Returns (found follows map, remaining pubkeys needing network fetch).
pub(super) async fn check_cached_follows_batch(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> (HashMap<PublicKey, Vec<PublicKey>>, Vec<PublicKey>) {
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
        return (results, vec![]);
    }

    // 2. Batch query cache (skip entries where follows is None — not yet fetched)
    let remaining_vec: Vec<PublicKey> = remaining.iter().copied().collect();
    if let Ok(cached_users) =
        CachedGraphUser::find_fresh_follows_batch(&remaining_vec, &whitenoise.database).await
    {
        for cached in cached_users {
            if let Some(follows) = cached.follows {
                results.insert(cached.pubkey, follows);
                remaining.remove(&cached.pubkey);
            }
        }
    }

    let remaining_vec: Vec<PublicKey> = remaining.into_iter().collect();
    (results, remaining_vec)
}

/// Tier 3: Fetch follows from network and cache them.
///
/// Caches ALL pubkeys including empty defaults ("follows nobody") since
/// the contact list is authoritative — absence on relays means no follows.
/// Uses immediate retries (not queue-based) since the follows producer has no queue.
pub(super) async fn fetch_network_follows(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> HashMap<PublicKey, Vec<PublicKey>> {
    if pubkeys.is_empty() {
        return HashMap::new();
    }

    let all_relays = connected_relays(whitenoise).await;
    let (events_by_author, _errored) =
        fetch_events_with_retries(whitenoise, pubkeys, Kind::ContactList, &all_relays).await;

    // Cache all pubkeys: found follows or empty default.
    // Unlike metadata, empty follows is authoritative — it means "follows nobody."
    let mut results = HashMap::new();
    for pk in pubkeys {
        let follows = events_by_author
            .get(pk)
            .and_then(|events| events.iter().max_by_key(|e| e.created_at))
            .map(parse_follows_from_event)
            .unwrap_or_default();

        let _ = CachedGraphUser::upsert_follows_only(pk, &follows, &whitenoise.database).await;
        results.insert(*pk, follows);
    }

    results
}

/// Batch fetch follows for multiple pubkeys (all tiers combined).
///
/// Convenience function used by tests; the pipeline uses individual tier functions.
#[cfg(test)]
pub(super) async fn get_follows_batch(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
) -> HashMap<PublicKey, Vec<PublicKey>> {
    let (mut results, remaining) = check_cached_follows_batch(whitenoise, pubkeys).await;
    if remaining.is_empty() {
        return results;
    }

    results.extend(fetch_network_follows(whitenoise, &remaining).await);
    results
}

// ---------------------------------------------------------------------------
// Event parsing helpers
// ---------------------------------------------------------------------------

/// Extract followed pubkeys from a contact list event.
///
/// Parses all `p` tags from the event, returning the pubkeys of followed users.
fn parse_follows_from_event(event: &Event) -> Vec<PublicKey> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            if tag.kind() == TagKind::p() {
                tag.content().and_then(|c| PublicKey::parse(c).ok())
            } else {
                None
            }
        })
        .collect()
}

/// Extract write relay URLs from a NIP-65 relay list event (Kind 10002).
///
/// Returns relay URLs where the user publishes events (write or read+write).
/// Read-only relays are excluded since they won't have the user's metadata.
fn parse_write_relays_from_event(event: &Event) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|tag| {
            if tag.kind() != TagKind::custom("r") {
                return None;
            }
            let url = tag.content()?.to_string();
            // Third element is the optional marker: "read", "write", or absent (= read+write)
            let marker = tag.as_slice().get(2).map(|s| s.as_str());
            match marker {
                None | Some("write") => Some(url),
                _ => None,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Network fetch helpers (follows producer only)
// ---------------------------------------------------------------------------

/// Core relay fetch with immediate retry logic.
///
/// Chunks pubkeys by `MAX_AUTHORS_PER_FILTER`, fetches the specified kind from
/// the given relays, and retries failed chunks up to `NETWORK_FETCH_RETRIES` times.
///
/// Only used by the follows producer, which has no queue for deferred retries.
/// Metadata tiers use single-attempt functions with queue-based retries in their consumers.
async fn fetch_events_with_retries(
    whitenoise: &Whitenoise,
    pubkeys: &[PublicKey],
    kind: Kind,
    relays: &[RelayUrl],
) -> (HashMap<PublicKey, Vec<Event>>, HashSet<PublicKey>) {
    let mut events_by_author: HashMap<PublicKey, Vec<Event>> = HashMap::new();

    // Build initial chunks
    let mut pending_chunks: Vec<Vec<PublicKey>> = pubkeys
        .chunks(MAX_AUTHORS_PER_FILTER)
        .map(|c| c.to_vec())
        .collect();

    for attempt in 0..=NETWORK_FETCH_RETRIES {
        if pending_chunks.is_empty() {
            break;
        }

        let mut failed_chunks = Vec::new();

        for chunk in &pending_chunks {
            let filter = Filter::new().authors(chunk.clone()).kinds([kind]);
            match whitenoise
                .nostr
                .client
                .fetch_events_from(relays, filter, SEARCH_FETCH_TIMEOUT)
                .await
            {
                Ok(events) => {
                    for event in events.iter() {
                        events_by_author
                            .entry(event.pubkey)
                            .or_default()
                            .push(event.clone());
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        target: "whitenoise::user_search::graph",
                        "Chunk fetch failed (attempt {}/{}): {}",
                        attempt + 1,
                        NETWORK_FETCH_RETRIES + 1,
                        e
                    );
                    failed_chunks.push(chunk.clone());
                }
            }
        }

        pending_chunks = failed_chunks;
    }

    // Any chunks still pending after all retries are errored
    let errored: HashSet<PublicKey> = pending_chunks.into_iter().flatten().collect();

    (events_by_author, errored)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use nostr_sdk::{EventBuilder, Keys, Tag, Timestamp};

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

    // --- check_cache_metadata tests ---

    #[tokio::test]
    async fn check_cache_metadata_returns_real_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let pk = Keys::generate().public_key();
        let cached = CachedGraphUser::new(pk, Some(Metadata::new().name("CacheHit")), Some(vec![]));
        cached.upsert(&whitenoise.database).await.unwrap();

        let (found, remaining) = check_cache_metadata(&whitenoise, &[pk]).await;

        assert_eq!(found.len(), 1);
        assert_eq!(found.get(&pk).unwrap().name, Some("CacheHit".to_string()));
        assert!(remaining.is_empty());
    }

    #[tokio::test]
    async fn check_cache_metadata_stops_confirmed_absent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let pk = Keys::generate().public_key();
        // Simulate confirmed-absent: Some(Metadata::new()) means "fetched, nothing there"
        let cached = CachedGraphUser::new(pk, Some(Metadata::new()), Some(vec![]));
        cached.upsert(&whitenoise.database).await.unwrap();

        let (found, remaining) = check_cache_metadata(&whitenoise, &[pk]).await;

        // Should NOT be in found (empty metadata)
        assert!(found.is_empty());
        // Should NOT be in remaining (it's confirmed absent, not a cache miss)
        assert!(
            remaining.is_empty(),
            "Confirmed-absent pubkey should not be forwarded to tier 3"
        );
    }

    #[tokio::test]
    async fn check_cache_metadata_forwards_none_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let pk = Keys::generate().public_key();
        // metadata = None means "not yet fetched" — should forward to tier 3
        let cached = CachedGraphUser::new(pk, None, Some(vec![]));
        cached.upsert(&whitenoise.database).await.unwrap();

        let (found, remaining) = check_cache_metadata(&whitenoise, &[pk]).await;

        assert!(found.is_empty());
        assert_eq!(remaining.len(), 1, "None metadata should forward to tier 3");
        assert_eq!(remaining[0], pk);
    }

    #[tokio::test]
    async fn check_cache_metadata_unknown_pubkey_forwards() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let pk = Keys::generate().public_key();

        let (found, remaining) = check_cache_metadata(&whitenoise, &[pk]).await;

        assert!(found.is_empty());
        assert_eq!(remaining.len(), 1);
    }

    // --- check_user_table_metadata tests ---

    #[tokio::test]
    async fn check_user_table_metadata_skips_empty_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let pk = Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: pk,
            metadata: Metadata::new(), // empty
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        user.save(&whitenoise.database).await.unwrap();

        let (found, remaining) = check_user_table_metadata(&whitenoise, &[pk]).await;

        assert!(found.is_empty(), "Empty metadata should not be in found");
        assert_eq!(remaining.len(), 1, "Empty metadata should forward");
    }

    #[tokio::test]
    async fn check_user_table_metadata_returns_populated_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let pk = Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: pk,
            metadata: Metadata::new().name("HasMeta"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        user.save(&whitenoise.database).await.unwrap();

        let (found, remaining) = check_user_table_metadata(&whitenoise, &[pk]).await;

        assert_eq!(found.len(), 1);
        assert_eq!(found.get(&pk).unwrap().name, Some("HasMeta".to_string()));
        assert!(remaining.is_empty());
    }

    // --- parse_write_relays_from_event tests ---

    #[test]
    fn parse_write_relays_includes_unmarked_relays() {
        let keys = Keys::generate();
        // Build a NIP-65 relay list event manually
        let tags = vec![
            Tag::parse(["r", "wss://relay1.example.com"]).unwrap(),
            Tag::parse(["r", "wss://relay2.example.com", "write"]).unwrap(),
            Tag::parse(["r", "wss://relay3.example.com", "read"]).unwrap(),
        ];
        let event = EventBuilder::new(Kind::RelayList, "")
            .tags(tags)
            .custom_created_at(Timestamp::now())
            .sign_with_keys(&keys)
            .unwrap();

        let relays = parse_write_relays_from_event(&event);

        // Unmarked (read+write) and explicitly "write" should be included
        assert!(relays.contains(&"wss://relay1.example.com".to_string()));
        assert!(relays.contains(&"wss://relay2.example.com".to_string()));
        // "read" only should be excluded
        assert!(!relays.contains(&"wss://relay3.example.com".to_string()));
    }

    #[test]
    fn parse_write_relays_empty_event() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::RelayList, "")
            .custom_created_at(Timestamp::now())
            .sign_with_keys(&keys)
            .unwrap();

        let relays = parse_write_relays_from_event(&event);
        assert!(relays.is_empty());
    }

    // --- parse_follows_from_event tests ---

    #[test]
    fn parse_follows_extracts_p_tags() {
        let keys = Keys::generate();
        let target1 = Keys::generate().public_key();
        let target2 = Keys::generate().public_key();

        let tags = vec![Tag::public_key(target1), Tag::public_key(target2)];
        let event = EventBuilder::new(Kind::ContactList, "")
            .tags(tags)
            .custom_created_at(Timestamp::now())
            .sign_with_keys(&keys)
            .unwrap();

        let follows = parse_follows_from_event(&event);

        assert_eq!(follows.len(), 2);
        assert!(follows.contains(&target1));
        assert!(follows.contains(&target2));
    }

    #[test]
    fn parse_follows_ignores_non_p_tags() {
        let keys = Keys::generate();
        let target = Keys::generate().public_key();

        let tags = vec![
            Tag::public_key(target),
            Tag::parse(["e", "someeventid"]).unwrap(),
            Tag::parse(["t", "nostr"]).unwrap(),
        ];
        let event = EventBuilder::new(Kind::ContactList, "")
            .tags(tags)
            .custom_created_at(Timestamp::now())
            .sign_with_keys(&keys)
            .unwrap();

        let follows = parse_follows_from_event(&event);

        assert_eq!(follows.len(), 1);
        assert!(follows.contains(&target));
    }

    #[test]
    fn parse_follows_empty_event() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::ContactList, "")
            .custom_created_at(Timestamp::now())
            .sign_with_keys(&keys)
            .unwrap();

        let follows = parse_follows_from_event(&event);
        assert!(follows.is_empty());
    }
}
