//! User search functionality based on social graph traversal.
//!
//! This module provides streaming user search that traverses the social graph
//! (web of trust) to find users matching a query, prioritized by social distance.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use nostr_sdk::{Metadata, PublicKey};
use tokio::sync::{broadcast, mpsc};

mod graph;
pub mod matcher;
mod types;

use crate::whitenoise::Whitenoise;
use crate::whitenoise::error::Result;

/// Maximum pubkeys to process per radius level (prevents graph explosion from supernodes).
const MAX_PUBKEYS_PER_RADIUS: usize = 10_000;

/// Timeout for fetching data at each radius level (seconds).
const RADIUS_FETCH_TIMEOUT_SECS: u64 = 30;

/// Batch size for processing pubkeys.
const PUBKEY_BATCH_SIZE: usize = 250;

/// Capacity for the candidate channel between follows-producer and tier 1 consumer.
const CANDIDATE_CHANNEL_CAPACITY: usize = 500;

/// Capacity for inter-tier channels (tier1→tier2, tier2→tier3).
/// Smaller than the producer channel since each tier resolves some pubkeys.
const TIER_CHANNEL_CAPACITY: usize = 100;

pub use matcher::{MatchQuality, MatchResult, MatchedField, match_metadata};
pub(crate) use types::SEARCH_CHANNEL_BUFFER_SIZE;
pub use types::{
    SearchUpdateTrigger, UserSearchParams, UserSearchResult, UserSearchSubscription,
    UserSearchUpdate,
};

impl Whitenoise {
    /// Search for users by name, username, or description within a social radius.
    ///
    /// Results stream via the returned subscription as they're found during graph traversal.
    /// Cancel by dropping the receiver (implicit cancellation).
    ///
    /// # Arguments
    /// * `params` - Search parameters including query, searcher pubkey, and radius range
    ///
    /// # Returns
    /// * `UserSearchSubscription` with update receiver
    ///
    /// # Incremental Search (Infinite Scroll)
    ///
    /// For infinite scroll UX, call multiple times with increasing radius ranges:
    /// ```ignore
    /// // First request
    /// let sub1 = whitenoise.search_users(UserSearchParams {
    ///     query: "jack".to_string(),
    ///     searcher_pubkey,
    ///     radius_start: 0,
    ///     radius_end: 2,
    /// }).await?;
    /// // ... receive results, user scrolls to bottom ...
    ///
    /// // Second request (continues where first left off)
    /// let sub2 = whitenoise.search_users(UserSearchParams {
    ///     query: "jack".to_string(),
    ///     searcher_pubkey,
    ///     radius_start: 3,
    ///     radius_end: 4,
    /// }).await?;
    /// ```
    ///
    /// The cache makes subsequent calls efficient - no redundant network requests.
    /// Frontend should deduplicate results across requests.
    ///
    /// # Update Triggers
    /// - `RadiusStarted` - Starting to search a new radius level
    /// - `ResultsFound` - Batch of results found (can be multiple per radius)
    /// - `RadiusCompleted` - Finished searching a radius level
    /// - `RadiusCapped` - Radius was capped due to too many pubkeys
    /// - `RadiusTimeout` - Radius fetch timed out
    /// - `SearchCompleted` - Search finished (all radii searched)
    /// - `Error` - Error occurred (search continues with partial results)
    ///
    /// # Cancellation
    /// Search cancels automatically when all update receivers are dropped.
    ///
    /// # Note
    /// This method does NOT create User records for search results.
    /// Only when the app explicitly interacts with a result (follow, message, etc.)
    /// should a User record be created via `find_or_create_user_by_pubkey`.
    pub async fn search_users(&self, params: UserSearchParams) -> Result<UserSearchSubscription> {
        if params.radius_start > params.radius_end {
            return Err(crate::whitenoise::error::WhitenoiseError::InvalidInput(
                format!(
                    "radius_start ({}) must be <= radius_end ({})",
                    params.radius_start, params.radius_end
                ),
            ));
        }

        // Create broadcast channel directly - no manager needed for ephemeral searches
        let (tx, rx) = broadcast::channel(SEARCH_CHANNEL_BUFFER_SIZE);

        // Clone/copy what we need for the spawned task
        let query = params.query.clone();
        let searcher_pubkey = params.searcher_pubkey;
        let radius_start = params.radius_start;
        let radius_end = params.radius_end;

        tokio::spawn(async move {
            // Get singleton instance inside spawned task (follows existing pattern in groups.rs)
            let whitenoise = match Self::get_instance() {
                Ok(wn) => wn,
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::user_search",
                        "Failed to get Whitenoise instance: {}",
                        e
                    );
                    let _ = tx.send(UserSearchUpdate {
                        trigger: SearchUpdateTrigger::Error {
                            message: "Internal error: failed to get application instance"
                                .to_string(),
                        },
                        new_results: vec![],
                        total_result_count: 0,
                    });
                    return;
                }
            };
            search_task(
                whitenoise,
                tx,
                query,
                searcher_pubkey,
                radius_start,
                radius_end,
            )
            .await;
        });

        Ok(UserSearchSubscription { updates: rx })
    }
}

/// A batch of candidate pubkeys sent from the follows-producer to the metadata-consumer.
struct CandidateBatch {
    radius: u8,
    kind: CandidateBatchKind,
}

enum CandidateBatchKind {
    /// A batch of pubkeys to fetch metadata for and match.
    Candidates(Vec<PublicKey>),
    /// Sentinel indicating a radius level is complete.
    RadiusComplete { total_pubkeys: usize },
}

/// Background search task that orchestrates a multi-tier pipeline.
///
/// ```text
/// Producer → [candidates] → Tier1 (User table) → Tier2 (Cache) → Tier3 (Network)
///                               ↓ emit              ↓ emit           ↓ emit
///                           ResultsFound         ResultsFound     ResultsFound
///                                                                 RadiusCompleted
///                                                                 SearchCompleted
/// ```
///
/// Each tier emits matches immediately and passes cache misses to the next tier.
/// Cached results appear in the output stream without waiting for network fetches.
async fn search_task(
    whitenoise: &Whitenoise,
    tx: broadcast::Sender<UserSearchUpdate>,
    query: String,
    searcher_pubkey: PublicKey,
    radius_start: u8,
    radius_end: u8,
) {
    let total_results = AtomicUsize::new(0);

    let (tx_candidates, rx_candidates) = mpsc::channel(CANDIDATE_CHANNEL_CAPACITY);
    let (tx_tier2, rx_tier2) = mpsc::channel(TIER_CHANNEL_CAPACITY);
    let (tx_tier3, rx_tier3) = mpsc::channel(TIER_CHANNEL_CAPACITY);

    // Run producer + 3 tier consumers concurrently.
    // Channel drops cascade: producer → tier1 → tier2 → tier3 → SearchCompleted.
    tokio::join!(
        follows_producer_task(
            whitenoise,
            tx.clone(),
            tx_candidates,
            searcher_pubkey,
            radius_start,
            radius_end,
        ),
        tier1_user_table_consumer(
            whitenoise,
            &tx,
            rx_candidates,
            tx_tier2,
            &query,
            &total_results,
        ),
        tier2_cache_consumer(
            whitenoise,
            &tx,
            rx_tier2,
            tx_tier3,
            &query,
            &total_results,
        ),
        tier3_network_consumer(
            whitenoise,
            &tx,
            rx_tier3,
            &query,
            &total_results,
            radius_end,
        ),
    );
}

/// Follows-producer: traverses the social graph and pushes candidate batches.
///
/// For each radius, fetches follows to build the layer, then pushes batches
/// of pubkeys to the candidate channel for metadata-matching.
async fn follows_producer_task(
    whitenoise: &Whitenoise,
    tx: broadcast::Sender<UserSearchUpdate>,
    tx_candidates: mpsc::Sender<CandidateBatch>,
    searcher_pubkey: PublicKey,
    radius_start: u8,
    radius_end: u8,
) {
    let mut seen_pubkeys: HashSet<PublicKey> = HashSet::new();
    let mut previous_layer_pubkeys: HashSet<PublicKey> = HashSet::new();

    for radius in 0..=radius_end {
        // Check if receivers still exist (implicit cancellation)
        if tx.receiver_count() == 0 {
            tracing::debug!(
                target: "whitenoise::user_search",
                "Search cancelled - no receivers (producer)"
            );
            return;
        }

        let in_requested_range = radius >= radius_start;

        // Build this radius layer
        let layer_pubkeys: HashSet<PublicKey> = if radius == 0 {
            HashSet::from([searcher_pubkey])
        } else {
            // Fetch follows with timeout (graph explosion mitigation)
            match tokio::time::timeout(
                std::time::Duration::from_secs(RADIUS_FETCH_TIMEOUT_SECS),
                build_layer_from_follows(whitenoise, &previous_layer_pubkeys, &seen_pubkeys),
            )
            .await
            {
                Ok(pubkeys) => pubkeys,
                Err(_) => {
                    // Timeout occurred - only notify for in-range radii
                    if in_requested_range {
                        let _ = tx.send(UserSearchUpdate {
                            trigger: SearchUpdateTrigger::RadiusTimeout { radius },
                            new_results: vec![],
                            total_result_count: 0,
                        });
                    }
                    // Continue with empty layer for this radius
                    HashSet::new()
                }
            }
        };

        // Apply cap after deduplication
        let (layer_pubkeys, was_capped) = if layer_pubkeys.len() > MAX_PUBKEYS_PER_RADIUS {
            let actual = layer_pubkeys.len();
            let capped: HashSet<PublicKey> = layer_pubkeys
                .into_iter()
                .take(MAX_PUBKEYS_PER_RADIUS)
                .collect();
            if in_requested_range {
                let _ = tx.send(UserSearchUpdate {
                    trigger: SearchUpdateTrigger::RadiusCapped {
                        radius,
                        cap: MAX_PUBKEYS_PER_RADIUS,
                        actual,
                    },
                    new_results: vec![],
                    total_result_count: 0,
                });
            }
            (capped, true)
        } else {
            (layer_pubkeys, false)
        };

        // Add to seen set
        seen_pubkeys.extend(layer_pubkeys.iter().copied());

        // Only push candidates for requested radius range
        if in_requested_range {
            // Emit RadiusStarted
            let _ = tx.send(UserSearchUpdate {
                trigger: SearchUpdateTrigger::RadiusStarted { radius },
                new_results: vec![],
                total_result_count: 0,
            });

            let pubkeys_vec: Vec<PublicKey> = layer_pubkeys.iter().copied().collect();
            let total_pubkeys_in_layer = pubkeys_vec.len();

            // Push batches to candidate channel
            for batch in pubkeys_vec.chunks(PUBKEY_BATCH_SIZE) {
                if tx_candidates
                    .send(CandidateBatch {
                        radius,
                        kind: CandidateBatchKind::Candidates(batch.to_vec()),
                    })
                    .await
                    .is_err()
                {
                    // Consumer dropped — search cancelled
                    return;
                }
            }

            // Push end-of-radius sentinel
            if tx_candidates
                .send(CandidateBatch {
                    radius,
                    kind: CandidateBatchKind::RadiusComplete {
                        total_pubkeys: total_pubkeys_in_layer,
                    },
                })
                .await
                .is_err()
            {
                return;
            }

            if was_capped {
                tracing::debug!(
                    target: "whitenoise::user_search",
                    "Radius {} was capped at {} pubkeys",
                    radius,
                    MAX_PUBKEYS_PER_RADIUS
                );
            }
        }

        previous_layer_pubkeys = layer_pubkeys;
    }
    // tx_candidates is dropped here, signaling completion to consumer
}

/// Match metadata against query and emit results via broadcast.
fn match_and_emit(
    tx: &broadcast::Sender<UserSearchUpdate>,
    metadata_map: &HashMap<PublicKey, Metadata>,
    pubkeys: &[PublicKey],
    query: &str,
    radius: u8,
    total_results: &AtomicUsize,
) {
    let mut batch_results = Vec::new();

    for pk in pubkeys {
        let metadata = match metadata_map.get(pk) {
            Some(m) => m,
            None => continue,
        };

        let match_result = match_metadata(metadata, query);
        if let (Some(quality), Some(best_field)) =
            (match_result.quality, match_result.best_field)
        {
            batch_results.push(UserSearchResult {
                pubkey: *pk,
                metadata: metadata.clone(),
                radius,
                match_quality: quality,
                best_field,
                matched_fields: match_result.matched_fields,
            });
        }
    }

    if !batch_results.is_empty() {
        batch_results.sort_by_key(|r| r.sort_key());
        let count = batch_results.len();
        let total = total_results.fetch_add(count, Ordering::Relaxed) + count;

        let _ = tx.send(UserSearchUpdate {
            trigger: SearchUpdateTrigger::ResultsFound,
            new_results: batch_results,
            total_result_count: total,
        });
    }
}

/// Tier 1 consumer: checks User table for metadata.
///
/// Emits matches immediately, forwards cache misses + sentinels to tier 2.
async fn tier1_user_table_consumer(
    whitenoise: &Whitenoise,
    tx: &broadcast::Sender<UserSearchUpdate>,
    mut rx: mpsc::Receiver<CandidateBatch>,
    tx_next: mpsc::Sender<CandidateBatch>,
    query: &str,
    total_results: &AtomicUsize,
) {
    while let Some(batch) = rx.recv().await {
        if tx.receiver_count() == 0 {
            tracing::debug!(
                target: "whitenoise::user_search",
                "Search cancelled - no receivers (tier1)"
            );
            return;
        }

        match batch.kind {
            CandidateBatchKind::Candidates(pubkeys) => {
                let (found, remaining) =
                    graph::check_user_table_metadata(whitenoise, &pubkeys).await;

                match_and_emit(tx, &found, &pubkeys, query, batch.radius, total_results);

                // Forward cache misses to tier 2
                if !remaining.is_empty() {
                    if tx_next
                        .send(CandidateBatch {
                            radius: batch.radius,
                            kind: CandidateBatchKind::Candidates(remaining),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            CandidateBatchKind::RadiusComplete { .. } => {
                // Forward sentinel to tier 2
                if tx_next.send(batch).await.is_err() {
                    return;
                }
            }
        }
    }
    // tx_next dropped here → tier 2 channel closes
}

/// Tier 2 consumer: checks CachedGraphUser table for metadata.
///
/// Emits matches immediately, forwards cache misses + sentinels to tier 3.
async fn tier2_cache_consumer(
    whitenoise: &Whitenoise,
    tx: &broadcast::Sender<UserSearchUpdate>,
    mut rx: mpsc::Receiver<CandidateBatch>,
    tx_next: mpsc::Sender<CandidateBatch>,
    query: &str,
    total_results: &AtomicUsize,
) {
    while let Some(batch) = rx.recv().await {
        if tx.receiver_count() == 0 {
            tracing::debug!(
                target: "whitenoise::user_search",
                "Search cancelled - no receivers (tier2)"
            );
            return;
        }

        match batch.kind {
            CandidateBatchKind::Candidates(pubkeys) => {
                let (found, remaining) =
                    graph::check_cache_metadata(whitenoise, &pubkeys).await;

                match_and_emit(tx, &found, &pubkeys, query, batch.radius, total_results);

                // Forward cache misses to tier 3
                if !remaining.is_empty() {
                    if tx_next
                        .send(CandidateBatch {
                            radius: batch.radius,
                            kind: CandidateBatchKind::Candidates(remaining),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            CandidateBatchKind::RadiusComplete { .. } => {
                // Forward sentinel to tier 3
                if tx_next.send(batch).await.is_err() {
                    return;
                }
            }
        }
    }
    // tx_next dropped here → tier 3 channel closes
}

/// Tier 3 consumer: fetches metadata from network relays.
///
/// This is the slowest tier — it makes network requests.
/// Emits matches, RadiusCompleted (on sentinel), and SearchCompleted (on channel close).
async fn tier3_network_consumer(
    whitenoise: &Whitenoise,
    tx: &broadcast::Sender<UserSearchUpdate>,
    mut rx: mpsc::Receiver<CandidateBatch>,
    query: &str,
    total_results: &AtomicUsize,
    radius_end: u8,
) {
    while let Some(batch) = rx.recv().await {
        if tx.receiver_count() == 0 {
            tracing::debug!(
                target: "whitenoise::user_search",
                "Search cancelled - no receivers (tier3)"
            );
            return;
        }

        match batch.kind {
            CandidateBatchKind::Candidates(pubkeys) => {
                let found = graph::fetch_network_metadata(whitenoise, &pubkeys).await;

                match_and_emit(tx, &found, &pubkeys, query, batch.radius, total_results);

                tokio::task::yield_now().await;
            }
            CandidateBatchKind::RadiusComplete { total_pubkeys } => {
                let _ = tx.send(UserSearchUpdate {
                    trigger: SearchUpdateTrigger::RadiusCompleted {
                        radius: batch.radius,
                        total_pubkeys_searched: total_pubkeys,
                    },
                    new_results: vec![],
                    total_result_count: total_results.load(Ordering::Relaxed),
                });
            }
        }
    }

    // Channel closed — all tiers done. Emit SearchCompleted.
    let total = total_results.load(Ordering::Relaxed);
    let _ = tx.send(UserSearchUpdate {
        trigger: SearchUpdateTrigger::SearchCompleted {
            final_radius: radius_end,
            total_results: total,
        },
        new_results: vec![],
        total_result_count: total,
    });
}

/// Build the set of pubkeys for the next radius layer by fetching follows.
///
/// Uses batch fetching to minimize network requests: all pubkeys from the
/// previous layer are fetched in a single operation where possible.
async fn build_layer_from_follows(
    whitenoise: &Whitenoise,
    previous_layer: &HashSet<PublicKey>,
    seen: &HashSet<PublicKey>,
) -> HashSet<PublicKey> {
    let pubkeys: Vec<PublicKey> = previous_layer.iter().copied().collect();

    // Batch fetch all follows at once
    let follows_map = graph::get_follows_batch(whitenoise, &pubkeys).await;

    // Collect unique follows not already seen
    let mut layer = HashSet::new();
    for follows in follows_map.values() {
        for follow in follows {
            if !seen.contains(follow) {
                layer.insert(*follow);
            }
            if layer.len() >= MAX_PUBKEYS_PER_RADIUS {
                return layer;
            }
        }
    }

    layer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::cached_graph_user::CachedGraphUser;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use crate::whitenoise::users::User;
    use nostr_sdk::Metadata;
    use tokio::sync::broadcast;

    /// Helper to run search_task directly and collect updates.
    /// This tests the core search logic without requiring the singleton.
    async fn run_search(
        whitenoise: &Whitenoise,
        query: &str,
        searcher_pubkey: PublicKey,
        radius_start: u8,
        radius_end: u8,
    ) -> Vec<UserSearchUpdate> {
        let (tx, mut rx) = broadcast::channel(SEARCH_CHANNEL_BUFFER_SIZE);

        // Run search task directly (not spawned)
        search_task(
            whitenoise,
            tx,
            query.to_string(),
            searcher_pubkey,
            radius_start,
            radius_end,
        )
        .await;

        // Collect all updates
        let mut updates = Vec::new();
        while let Ok(update) = rx.try_recv() {
            updates.push(update);
        }
        updates
    }

    #[tokio::test]
    async fn search_rejects_invalid_radius_range() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let params = UserSearchParams {
            query: "test".to_string(),
            searcher_pubkey: account.pubkey,
            radius_start: 5,
            radius_end: 2,
        };

        let result = whitenoise.search_users(params).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn search_emits_search_completed() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let updates = run_search(&whitenoise, "nonexistent", account.pubkey, 0, 0).await;

        let has_completed = updates
            .iter()
            .any(|u| matches!(u.trigger, SearchUpdateTrigger::SearchCompleted { .. }));
        assert!(has_completed);
    }

    #[tokio::test]
    async fn search_emits_radius_started_for_each_radius() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let updates = run_search(&whitenoise, "test", account.pubkey, 0, 1).await;

        let radius_started_count = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::RadiusStarted { .. }))
            .count();

        // Should have RadiusStarted for radius 0 and 1
        assert_eq!(radius_started_count, 2);
    }

    #[tokio::test]
    async fn radius_started_emitted_before_radius_completed() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let updates = run_search(&whitenoise, "test", account.pubkey, 0, 0).await;

        // Find positions of RadiusStarted and RadiusCompleted for radius 0
        let started_pos = updates
            .iter()
            .position(|u| matches!(u.trigger, SearchUpdateTrigger::RadiusStarted { radius: 0 }));
        let completed_pos = updates.iter().position(|u| {
            matches!(
                u.trigger,
                SearchUpdateTrigger::RadiusCompleted { radius: 0, .. }
            )
        });

        assert!(started_pos.is_some(), "RadiusStarted should be emitted");
        assert!(completed_pos.is_some(), "RadiusCompleted should be emitted");
        assert!(
            started_pos.unwrap() < completed_pos.unwrap(),
            "RadiusStarted should come before RadiusCompleted"
        );
    }

    #[tokio::test]
    async fn search_finds_matching_user_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Update account metadata to be searchable
        let metadata = Metadata::new().name("AliceTest");
        account
            .update_metadata(&metadata, &whitenoise)
            .await
            .unwrap();

        let updates = run_search(&whitenoise, "alice", account.pubkey, 0, 0).await;

        let found_results: Vec<_> = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .collect();

        assert!(!found_results.is_empty());
        assert_eq!(found_results[0].pubkey, account.pubkey);
    }

    #[tokio::test]
    async fn dropping_receiver_stops_search() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let (tx, rx) = broadcast::channel(SEARCH_CHANNEL_BUFFER_SIZE);

        // Drop the receiver immediately
        drop(rx);

        // Run search - it should detect no receivers and exit early
        search_task(
            &whitenoise,
            tx,
            "test".to_string(),
            account.pubkey,
            0,
            10, // Large radius that would take a while
        )
        .await;

        // Test passes if no panic occurred - the task should have exited cleanly
    }

    #[tokio::test]
    async fn radius_start_skips_earlier_results() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Update account metadata
        let metadata = Metadata::new().name("SkipTest");
        account
            .update_metadata(&metadata, &whitenoise)
            .await
            .unwrap();

        // Search starting from radius 1 (skipping radius 0)
        let updates = run_search(&whitenoise, "skip", account.pubkey, 1, 1).await;

        let found_radius_0_started = updates
            .iter()
            .any(|u| matches!(u.trigger, SearchUpdateTrigger::RadiusStarted { radius: 0 }));
        let found_radius_1_started = updates
            .iter()
            .any(|u| matches!(u.trigger, SearchUpdateTrigger::RadiusStarted { radius: 1 }));

        // Should NOT emit RadiusStarted for radius 0, only for radius 1
        assert!(!found_radius_0_started);
        assert!(found_radius_1_started);
    }

    #[tokio::test]
    async fn search_emits_radius_completed_with_count() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let updates = run_search(&whitenoise, "test", account.pubkey, 0, 0).await;

        let radius_completed = updates.iter().find_map(|u| {
            if let SearchUpdateTrigger::RadiusCompleted {
                radius,
                total_pubkeys_searched,
            } = u.trigger
            {
                Some((radius, total_pubkeys_searched))
            } else {
                None
            }
        });

        // Should have RadiusCompleted for radius 0 with 1 pubkey (the searcher)
        assert!(radius_completed.is_some());
        let (radius, count) = radius_completed.unwrap();
        assert_eq!(radius, 0);
        assert_eq!(count, 1); // Just the searcher at radius 0
    }

    #[tokio::test]
    async fn search_handles_no_metadata_gracefully() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Don't set any metadata - search should still complete
        let updates = run_search(&whitenoise, "anything", account.pubkey, 0, 0).await;

        let completed = updates
            .iter()
            .any(|u| matches!(u.trigger, SearchUpdateTrigger::SearchCompleted { .. }));
        assert!(completed);
    }

    #[tokio::test]
    async fn search_includes_total_result_count_in_updates() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Set metadata to be found
        let metadata = Metadata::new().name("CountTest");
        account
            .update_metadata(&metadata, &whitenoise)
            .await
            .unwrap();

        let updates = run_search(&whitenoise, "count", account.pubkey, 0, 0).await;

        // Find the last update's count
        let last_count = updates.last().map(|u| u.total_result_count).unwrap_or(0);

        // Should have found at least 1 result (the account itself)
        assert!(last_count >= 1);
    }

    #[tokio::test]
    async fn search_result_sorting_uses_sort_key() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Set metadata with the exact query as name for best match
        let metadata = Metadata::new()
            .name("sorttest")
            .about("This also contains sorttest somewhere");
        account
            .update_metadata(&metadata, &whitenoise)
            .await
            .unwrap();

        let updates = run_search(&whitenoise, "sorttest", account.pubkey, 0, 0).await;

        let results: Vec<_> = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .collect();

        // If we have results, verify they're sorted
        if !results.is_empty() {
            let first = &results[0];
            // Name match should be best field
            assert_eq!(first.best_field, MatchedField::Name);
            // Exact match on name should give best quality
            assert_eq!(first.match_quality, MatchQuality::Exact);
        }
    }

    #[tokio::test]
    async fn search_traverses_social_graph() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Create a followed user and add to database as a User so it will be found
        let followed_keys = nostr_sdk::Keys::generate();

        // Add the followed user to the User table with searchable metadata
        let user = User {
            id: None,
            pubkey: followed_keys.public_key(),
            metadata: Metadata::new().name("FollowedUserGraph"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        user.save(&whitenoise.database).await.unwrap();

        // Now follow this user
        whitenoise
            .follow_user(&account, &followed_keys.public_key())
            .await
            .unwrap();

        // Give time for follow to be processed
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Search for the followed user at radius 1
        let updates = run_search(&whitenoise, "followedusergraph", account.pubkey, 0, 1).await;

        let found_at_radius_1 = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .any(|r| r.pubkey == followed_keys.public_key() && r.radius == 1);

        assert!(found_at_radius_1, "Should find followed user at radius 1");
    }

    #[tokio::test]
    async fn build_layer_deduplicates_follows() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Create two users that both follow the same target
        let target_keys = nostr_sdk::Keys::generate();

        // Add multiple users to cache who all follow the same target
        let user1 = nostr_sdk::Keys::generate();
        let user2 = nostr_sdk::Keys::generate();

        let cached1 = CachedGraphUser::new(
            user1.public_key(),
            Some(Metadata::new().name("User1")),
            Some(vec![target_keys.public_key()]),
        );
        cached1.upsert(&whitenoise.database).await.unwrap();

        let cached2 = CachedGraphUser::new(
            user2.public_key(),
            Some(Metadata::new().name("User2")),
            Some(vec![target_keys.public_key()]),
        );
        cached2.upsert(&whitenoise.database).await.unwrap();

        // Follow both users
        whitenoise
            .follow_user(&account, &user1.public_key())
            .await
            .unwrap();
        whitenoise
            .follow_user(&account, &user2.public_key())
            .await
            .unwrap();

        // Add target to cache
        let target_cached = CachedGraphUser::new(
            target_keys.public_key(),
            Some(Metadata::new().name("DedupeTarget")),
            Some(vec![]),
        );
        target_cached.upsert(&whitenoise.database).await.unwrap();

        // Search at radius 2 should find target only once
        let updates = run_search(&whitenoise, "dedupetarget", account.pubkey, 2, 2).await;

        let target_count = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .filter(|r| r.pubkey == target_keys.public_key())
            .count();

        // Target should appear at most once (deduplication)
        assert!(target_count <= 1);
    }

    /// Search should find a followed user by name even when their User record
    /// has empty metadata, as long as the cache has real metadata.
    ///
    /// This simulates the race condition where contact list sync creates a
    /// User record with Metadata::new() before background metadata fetch
    /// completes, but the CachedGraphUser table already has the real metadata.
    #[tokio::test]
    async fn search_finds_followed_user_when_user_record_has_empty_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let followed_keys = nostr_sdk::Keys::generate();

        // Create a User record with empty metadata (simulates contact list sync
        // before background metadata fetch completes)
        let user = User {
            id: None,
            pubkey: followed_keys.public_key(),
            metadata: Metadata::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        user.save(&whitenoise.database).await.unwrap();

        // Follow the user
        whitenoise
            .follow_user(&account, &followed_keys.public_key())
            .await
            .unwrap();

        // Populate the cache with real metadata
        let cached = CachedGraphUser::new(
            followed_keys.public_key(),
            Some(Metadata::new().name("aleups").display_name("Aleups")),
            Some(vec![]),
        );
        cached.upsert(&whitenoise.database).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Search by name at radius 1 (follows)
        let updates = run_search(&whitenoise, "aleups", account.pubkey, 0, 1).await;

        let found = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .any(|r| r.pubkey == followed_keys.public_key());

        assert!(
            found,
            "Should find followed user 'aleups' by name at radius 1"
        );
    }
}
