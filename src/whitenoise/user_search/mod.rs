//! User search functionality based on social graph traversal.
//!
//! This module provides streaming user search that traverses the social graph
//! (web of trust) to find users matching a query, prioritized by social distance.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use nostr_sdk::{Metadata, PublicKey, RelayUrl};
use tokio::sync::{broadcast, mpsc};

mod graph;
pub mod matcher;
mod metrics;
mod types;

use crate::whitenoise::Whitenoise;
use crate::whitenoise::cached_graph_user::CachedGraphUser;
use crate::whitenoise::error::Result;

/// Maximum pubkeys to process per radius level (prevents graph explosion from supernodes).
const MAX_PUBKEYS_PER_RADIUS: usize = 25_000;

/// Timeout for fetching data at each radius level (seconds).
const RADIUS_FETCH_TIMEOUT_SECS: u64 = 300;

/// Batch size for streaming pubkeys through the pipeline.
/// Tested up to 500 authors per filter across 9 relays — all respond in under 2.5s.
/// 200 balances fewer fetch rounds (5 batches for 1000 pubkeys) with reasonable
/// per-batch latency (~1.5s).
const PUBKEY_BATCH_SIZE: usize = 200;

/// Maximum chunks fetched concurrently from relays in tier 3.
/// Each chunk is one relay request (PUBKEY_BATCH_SIZE pubkeys).
const MAX_CONCURRENT_NETWORK_FETCHES: usize = 5;

/// Maximum concurrent relay list fetches in tier 4.
const MAX_CONCURRENT_RELAY_LIST_FETCHES: usize = 2;

/// Maximum concurrent per-user relay metadata fetches in tier 5.
const MAX_CONCURRENT_USER_RELAY_FETCHES: usize = 2;

/// Maximum queue-based retries before forwarding to the next tier or dropping.
const MAX_QUEUE_RETRIES: u8 = 3;

/// Capacity for the candidate channel between follows-producer and tier 1 consumer.
const CANDIDATE_CHANNEL_CAPACITY: usize = 500;

/// Capacity for inter-tier channels.
const TIER_CHANNEL_CAPACITY: usize = 500;

/// Well-connected pubkey injected as a graph entrypoint when the searcher's
/// social graph is exhausted (e.g., new account with no follows).
///
/// npub1zuuajd7u3sx8xu92yav9jwxpr839cs0kc3q6t56vd5u9q033xmhsk6c2uc
const FALLBACK_SEED_PUBKEY: &str =
    "1739d937dc8c0c7370aa27585938c119e25c41f6c441a5d34c6d38503e3136ef";

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
    /// Pubkeys that failed all tier 3 retries (network errors, not EOSE).
    /// Tier 4 uses this to decide caching: no relay list + failed → don't cache.
    FailedCandidates(Vec<PublicKey>),
    /// Sentinel indicating a radius level is complete.
    RadiusComplete { total_pubkeys: usize },
}

/// A batch of (pubkey, relay list) pairs sent from tier 4 to tier 5.
struct UserRelayBatch {
    radius: u8,
    kind: UserRelayBatchKind,
}

enum UserRelayBatchKind {
    /// Pubkeys with their discovered relay lists for metadata lookup.
    Candidates(Vec<(PublicKey, Vec<RelayUrl>)>),
    /// Sentinel indicating a radius level is complete.
    RadiusComplete { total_pubkeys: usize },
}

/// Result of a tier 3 network metadata fetch for a single chunk.
struct ChunkFetchResult {
    found: HashMap<PublicKey, Metadata>,
    /// Pubkeys not resolved — forwarded to tier 4.
    remaining: Vec<PublicKey>,
    /// All pubkeys in the original chunk.
    all_pubkeys: Vec<PublicKey>,
    radius: u8,
    /// Retry attempt number (0-based).
    attempt: u8,
    /// True if the fetch failed due to network error.
    errored: bool,
    /// How long the relay request took.
    duration: Duration,
}

/// Result of a tier 4 relay list fetch for a single chunk.
struct RelayListFetchResult {
    relay_map: HashMap<PublicKey, Vec<RelayUrl>>,
    pubkeys: Vec<PublicKey>,
    radius: u8,
    attempt: u8,
    /// Whether these pubkeys came from a failed tier 3 (affects caching policy).
    tier3_failed: bool,
    errored: bool,
    duration: Duration,
}

/// Result of a tier 5 per-user relay metadata fetch.
struct UserRelayFetchResult {
    pubkey: PublicKey,
    relays: Vec<RelayUrl>,
    radius: u8,
    attempt: u8,
    result: graph::UserRelayResult,
    duration: Duration,
}

/// Background search task that orchestrates a multi-tier pipeline.
///
/// ```text
/// Producer → T1 (User table) → T2 (Cache) → T3 (Network) → T4 (Relay lists) → T5 (User relays)
///               ↓ emit            ↓ emit        ↓ emit          ↓ emit              ↓ emit
///           ResultsFound      ResultsFound   ResultsFound    ResultsFound        ResultsFound
///                                                                              RadiusCompleted
///                                                                              SearchCompleted
/// ```
///
/// Each tier emits matches immediately and passes misses to the next tier.
/// Tiers 3, 4, and 5 use queue-based retries: failed chunks go to the end of a
/// `VecDeque` (up to `MAX_QUEUE_RETRIES` times), allowing other work to proceed
/// while a relay recovers.
async fn search_task(
    whitenoise: &Whitenoise,
    tx: broadcast::Sender<UserSearchUpdate>,
    query: String,
    searcher_pubkey: PublicKey,
    radius_start: u8,
    radius_end: u8,
) {
    let total_results = AtomicUsize::new(0);
    let metrics = metrics::PipelineMetrics::new();

    let (tx_candidates, rx_candidates) = mpsc::channel(CANDIDATE_CHANNEL_CAPACITY);
    let (tx_tier2, rx_tier2) = mpsc::channel(TIER_CHANNEL_CAPACITY);
    let (tx_tier3, rx_tier3) = mpsc::channel(TIER_CHANNEL_CAPACITY);
    let (tx_tier4, rx_tier4) = mpsc::channel(TIER_CHANNEL_CAPACITY);
    let (tx_tier5, rx_tier5) = mpsc::channel(TIER_CHANNEL_CAPACITY);

    // Run producer + 5 tier consumers concurrently.
    // Channel drops cascade: producer → tier1 → … → tier5 → SearchCompleted.
    tokio::join!(
        follows_producer_task(
            whitenoise,
            tx.clone(),
            tx_candidates,
            searcher_pubkey,
            radius_start,
            radius_end,
            &metrics,
        ),
        tier1_user_table_consumer(
            whitenoise,
            &tx,
            rx_candidates,
            tx_tier2,
            &query,
            &total_results,
            &metrics,
        ),
        tier2_cache_consumer(
            whitenoise,
            &tx,
            rx_tier2,
            tx_tier3,
            &query,
            &total_results,
            &metrics,
        ),
        tier3_network_consumer(
            whitenoise,
            &tx,
            rx_tier3,
            tx_tier4,
            &query,
            &total_results,
            &metrics,
        ),
        tier4_relay_list_consumer(
            whitenoise,
            &tx,
            rx_tier4,
            tx_tier5,
            &query,
            &total_results,
            &metrics,
        ),
        tier5_user_relay_consumer(
            whitenoise,
            &tx,
            rx_tier5,
            &query,
            &total_results,
            radius_end,
            &metrics,
        ),
    );
}

/// Apply the MAX_PUBKEYS_PER_RADIUS cap to a layer. Returns (capped layer, was_capped).
fn apply_cap(layer: HashSet<PublicKey>) -> (HashSet<PublicKey>, bool) {
    if layer.len() > MAX_PUBKEYS_PER_RADIUS {
        let capped: HashSet<PublicKey> = layer.into_iter().take(MAX_PUBKEYS_PER_RADIUS).collect();
        (capped, true)
    } else {
        (layer, false)
    }
}

/// Push candidate batches to the channel, chunked by PUBKEY_BATCH_SIZE.
async fn push_candidates(
    tx: &mpsc::Sender<CandidateBatch>,
    pubkeys: &HashSet<PublicKey>,
    radius: u8,
    metrics: &metrics::PipelineMetrics,
) -> std::result::Result<(), mpsc::error::SendError<CandidateBatch>> {
    metrics
        .producer_pubkeys
        .fetch_add(pubkeys.len(), Ordering::Relaxed);
    let pubkeys_vec: Vec<PublicKey> = pubkeys.iter().copied().collect();
    for batch in pubkeys_vec.chunks(PUBKEY_BATCH_SIZE) {
        tx.send(CandidateBatch {
            radius,
            kind: CandidateBatchKind::Candidates(batch.to_vec()),
        })
        .await?;
    }
    Ok(())
}

/// Push a RadiusComplete sentinel to the channel.
async fn push_radius_complete(
    tx: &mpsc::Sender<CandidateBatch>,
    radius: u8,
    total_pubkeys: usize,
) -> std::result::Result<(), mpsc::error::SendError<CandidateBatch>> {
    tx.send(CandidateBatch {
        radius,
        kind: CandidateBatchKind::RadiusComplete { total_pubkeys },
    })
    .await
}

/// Follows-producer: traverses the social graph and pushes candidate batches.
///
/// Uses two-phase follows fetching per radius:
/// 1. Cached follows (instant) → push partial candidates immediately
/// 2. Network follows (slow) → push additional candidates while consumers are busy
async fn follows_producer_task(
    whitenoise: &Whitenoise,
    tx: broadcast::Sender<UserSearchUpdate>,
    tx_candidates: mpsc::Sender<CandidateBatch>,
    searcher_pubkey: PublicKey,
    radius_start: u8,
    radius_end: u8,
    metrics: &metrics::PipelineMetrics,
) {
    let mut seen_pubkeys: HashSet<PublicKey> = HashSet::new();
    let mut previous_layer_pubkeys: HashSet<PublicKey> = HashSet::new();
    let mut fallback_used = false;

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

        if radius == 0 {
            let layer_pubkeys = HashSet::from([searcher_pubkey]);
            seen_pubkeys.extend(layer_pubkeys.iter().copied());

            if in_requested_range {
                let _ = tx.send(UserSearchUpdate {
                    trigger: SearchUpdateTrigger::RadiusStarted { radius },
                    new_results: vec![],
                    total_result_count: 0,
                });

                if push_candidates(&tx_candidates, &layer_pubkeys, radius, metrics)
                    .await
                    .is_err()
                {
                    return;
                }

                if push_radius_complete(&tx_candidates, radius, layer_pubkeys.len())
                    .await
                    .is_err()
                {
                    return;
                }
            }

            previous_layer_pubkeys = layer_pubkeys;
            continue;
        }

        // When every user in the previous layer has an empty follows list,
        // the graph can't expand further. Inject a well-connected seed pubkey
        // so the search has an entrypoint into the broader network.
        let fallback_seed = if previous_layer_pubkeys.is_empty() && !fallback_used {
            match PublicKey::parse(FALLBACK_SEED_PUBKEY) {
                Ok(seed) if !seen_pubkeys.contains(&seed) => {
                    previous_layer_pubkeys.insert(seed);
                    fallback_used = true;
                    tracing::info!(
                        target: "whitenoise::user_search",
                        "Social graph exhausted at radius {} — injecting fallback seed",
                        radius,
                    );
                    Some(seed)
                }
                _ => None,
            }
        } else {
            None
        };

        // Phase 1: Build partial layer from cached follows (instant)
        let (partial_layer, need_fetch) = match tokio::time::timeout(
            std::time::Duration::from_secs(RADIUS_FETCH_TIMEOUT_SECS),
            build_cached_layer_from_follows(whitenoise, &previous_layer_pubkeys, &seen_pubkeys),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                if in_requested_range {
                    let _ = tx.send(UserSearchUpdate {
                        trigger: SearchUpdateTrigger::RadiusTimeout { radius },
                        new_results: vec![],
                        total_result_count: 0,
                    });
                }
                previous_layer_pubkeys = HashSet::new();
                continue;
            }
        };

        // Apply cap to partial layer
        let (mut partial_layer, partial_capped) = apply_cap(partial_layer);

        // Include the seed itself so it's findable via metadata search
        if let Some(seed) = fallback_seed {
            partial_layer.insert(seed);
        }

        // Inject group co-members at radius 1 so users you share groups with
        // are immediately discoverable without needing to be followed.
        // Intentionally placed after apply_cap — group co-members are high-value
        // local data and should not compete with follows for cap space.
        if radius == 1 {
            let group_members =
                graph::get_group_co_member_pubkeys(whitenoise, &searcher_pubkey).await;
            for pk in group_members {
                if !seen_pubkeys.contains(&pk) {
                    partial_layer.insert(pk);
                }
            }
        }

        seen_pubkeys.extend(partial_layer.iter().copied());

        if in_requested_range {
            let _ = tx.send(UserSearchUpdate {
                trigger: SearchUpdateTrigger::RadiusStarted { radius },
                new_results: vec![],
                total_result_count: 0,
            });

            // Push partial layer candidates immediately — consumers start processing
            if push_candidates(&tx_candidates, &partial_layer, radius, metrics)
                .await
                .is_err()
            {
                return;
            }
        }

        // Phase 2: Fetch remaining follows from network (slow — consumers are busy)
        let additional_layer =
            if !need_fetch.is_empty() && partial_layer.len() < MAX_PUBKEYS_PER_RADIUS {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(RADIUS_FETCH_TIMEOUT_SECS),
                    build_network_layer_from_follows(
                        whitenoise,
                        &need_fetch,
                        &seen_pubkeys,
                        &partial_layer,
                    ),
                )
                .await
                {
                    Ok(additional) => {
                        // Apply remaining cap budget
                        let budget = MAX_PUBKEYS_PER_RADIUS - partial_layer.len();
                        if additional.len() > budget {
                            additional.into_iter().take(budget).collect()
                        } else {
                            additional
                        }
                    }
                    Err(_) => {
                        if in_requested_range {
                            let _ = tx.send(UserSearchUpdate {
                                trigger: SearchUpdateTrigger::RadiusTimeout { radius },
                                new_results: vec![],
                                total_result_count: 0,
                            });
                        }
                        HashSet::new()
                    }
                }
            } else {
                HashSet::new()
            };

        seen_pubkeys.extend(additional_layer.iter().copied());

        if in_requested_range {
            // Push additional candidates from network follows
            if !additional_layer.is_empty()
                && push_candidates(&tx_candidates, &additional_layer, radius, metrics)
                    .await
                    .is_err()
            {
                return;
            }

            let total = partial_layer.len() + additional_layer.len();
            let was_capped =
                partial_capped || (!additional_layer.is_empty() && total >= MAX_PUBKEYS_PER_RADIUS);

            if was_capped {
                let _ = tx.send(UserSearchUpdate {
                    trigger: SearchUpdateTrigger::RadiusCapped {
                        radius,
                        cap: MAX_PUBKEYS_PER_RADIUS,
                        actual: total,
                    },
                    new_results: vec![],
                    total_result_count: 0,
                });
                tracing::debug!(
                    target: "whitenoise::user_search",
                    "Radius {} was capped at {} pubkeys",
                    radius,
                    MAX_PUBKEYS_PER_RADIUS
                );
            }

            if push_radius_complete(&tx_candidates, radius, total)
                .await
                .is_err()
            {
                return;
            }
        }

        // Combine both phases for next radius
        let mut full_layer = partial_layer;
        full_layer.extend(additional_layer);
        previous_layer_pubkeys = full_layer;
    }
    // tx_candidates is dropped here, signaling completion to tier 1
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
        if let (Some(quality), Some(best_field)) = (match_result.quality, match_result.best_field) {
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
    metrics: &metrics::PipelineMetrics,
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
                metrics
                    .t1_received
                    .fetch_add(pubkeys.len(), Ordering::Relaxed);
                let (found, remaining) =
                    graph::check_user_table_metadata(whitenoise, &pubkeys).await;

                metrics.t1_found.fetch_add(found.len(), Ordering::Relaxed);
                metrics
                    .t1_forwarded
                    .fetch_add(remaining.len(), Ordering::Relaxed);
                match_and_emit(tx, &found, &pubkeys, query, batch.radius, total_results);

                // Forward cache misses to tier 2
                if !remaining.is_empty()
                    && tx_next
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
            CandidateBatchKind::FailedCandidates(_) | CandidateBatchKind::RadiusComplete { .. } => {
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
    metrics: &metrics::PipelineMetrics,
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
                metrics
                    .t2_received
                    .fetch_add(pubkeys.len(), Ordering::Relaxed);
                let (found, remaining) = graph::check_cache_metadata(whitenoise, &pubkeys).await;

                metrics.t2_found.fetch_add(found.len(), Ordering::Relaxed);
                metrics
                    .t2_forwarded
                    .fetch_add(remaining.len(), Ordering::Relaxed);
                match_and_emit(tx, &found, &pubkeys, query, batch.radius, total_results);

                // Forward cache misses to tier 3
                if !remaining.is_empty()
                    && tx_next
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
            CandidateBatchKind::FailedCandidates(_) | CandidateBatchKind::RadiusComplete { .. } => {
                if tx_next.send(batch).await.is_err() {
                    return;
                }
            }
        }
    }
    // tx_next dropped here → tier 3 channel closes
}

/// Tier 3 consumer: fetches metadata from network relays with bounded concurrency.
///
/// Uses `FuturesUnordered` to keep up to `MAX_CONCURRENT_NETWORK_FETCHES` relay
/// requests in flight. Failed chunks are requeued at the back of a `VecDeque`
/// (up to `MAX_QUEUE_RETRIES` times), allowing other work to proceed while a
/// relay recovers.
///
/// - Success + found: cache and emit match, forward remaining as `Candidates`
/// - Success + EOSE: forward remaining as `Candidates` (not confirmed absent yet)
/// - Error + retries left: requeue at back of pending
/// - Error + retries exhausted: forward as `FailedCandidates` (affects tier 4 caching)
async fn tier3_network_consumer(
    whitenoise: &Whitenoise,
    tx: &broadcast::Sender<UserSearchUpdate>,
    mut rx: mpsc::Receiver<CandidateBatch>,
    tx_next: mpsc::Sender<CandidateBatch>,
    query: &str,
    total_results: &AtomicUsize,
    metrics: &metrics::PipelineMetrics,
) {
    // (chunk, radius, attempt)
    let mut pending: VecDeque<(Vec<PublicKey>, u8, u8)> = VecDeque::new();
    let mut in_flight = FuturesUnordered::new();
    let mut channel_open = true;

    loop {
        // Fill available concurrency slots from pending queue
        while in_flight.len() < MAX_CONCURRENT_NETWORK_FETCHES {
            match pending.pop_front() {
                Some((chunk, radius, attempt)) => {
                    in_flight.push(fetch_chunk_metadata(whitenoise, chunk, radius, attempt));
                }
                None => break,
            }
        }

        // Done when all sources are exhausted
        if in_flight.is_empty() && pending.is_empty() && !channel_open {
            break;
        }

        if tx.receiver_count() == 0 {
            tracing::debug!(
                target: "whitenoise::user_search",
                "Search cancelled - no receivers (tier3), draining {} in-flight requests",
                in_flight.len(),
            );
            while in_flight.next().await.is_some() {}
            return;
        }

        tokio::select! {
            biased;

            // Prioritize completing in-flight work (emit results ASAP)
            Some(result) = in_flight.next() => {
                metrics.record_t3_result(
                    result.found.len(), result.remaining.len(), result.all_pubkeys.len(),
                    result.attempt, result.errored, result.duration,
                );
                if result.errored {
                    if result.attempt < MAX_QUEUE_RETRIES {
                        pending.push_back((result.all_pubkeys, result.radius, result.attempt + 1));
                    } else if tx_next
                        .send(CandidateBatch {
                            radius: result.radius,
                            kind: CandidateBatchKind::FailedCandidates(result.all_pubkeys),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                } else {
                    match_and_emit(tx, &result.found, &result.all_pubkeys, query, result.radius, total_results);

                    if !result.remaining.is_empty()
                        && tx_next
                            .send(CandidateBatch {
                                radius: result.radius,
                                kind: CandidateBatchKind::Candidates(result.remaining),
                            })
                            .await
                            .is_err()
                    {
                        return;
                    }
                }
            }

            // Accept new batches when channel is open
            batch = rx.recv(), if channel_open => {
                match batch {
                    Some(batch) => match batch.kind {
                        CandidateBatchKind::Candidates(pubkeys) => {
                            metrics.t3_received.fetch_add(pubkeys.len(), Ordering::Relaxed);
                            pending.push_back((pubkeys, batch.radius, 0));
                        }
                        CandidateBatchKind::FailedCandidates(_) => {
                            // Pass through (shouldn't arrive here, but forward defensively)
                            if tx_next.send(batch).await.is_err() {
                                return;
                            }
                        }
                        CandidateBatchKind::RadiusComplete { total_pubkeys } => {
                            // Drain all in-flight + pending before forwarding sentinel.
                            // Retried chunks are bounded by MAX_QUEUE_RETRIES so this terminates.
                            'drain: loop {
                                while in_flight.len() < MAX_CONCURRENT_NETWORK_FETCHES {
                                    match pending.pop_front() {
                                        Some((chunk, r, att)) => {
                                            in_flight.push(fetch_chunk_metadata(whitenoise, chunk, r, att));
                                        }
                                        None => break,
                                    }
                                }
                                match in_flight.next().await {
                                    Some(r) => {
                                        metrics.record_t3_result(
                                            r.found.len(), r.remaining.len(), r.all_pubkeys.len(),
                                            r.attempt, r.errored, r.duration,
                                        );
                                        if r.errored {
                                            if r.attempt < MAX_QUEUE_RETRIES {
                                                pending.push_back((r.all_pubkeys, r.radius, r.attempt + 1));
                                            } else if tx_next
                                                .send(CandidateBatch {
                                                    radius: r.radius,
                                                    kind: CandidateBatchKind::FailedCandidates(r.all_pubkeys),
                                                })
                                                .await
                                                .is_err()
                                            {
                                                return;
                                            }
                                        } else {
                                            match_and_emit(tx, &r.found, &r.all_pubkeys, query, r.radius, total_results);
                                            if !r.remaining.is_empty()
                                                && tx_next
                                                    .send(CandidateBatch {
                                                        radius: r.radius,
                                                        kind: CandidateBatchKind::Candidates(r.remaining),
                                                    })
                                                    .await
                                                    .is_err()
                                            {
                                                return;
                                            }
                                        }
                                    }
                                    None => {
                                        if pending.is_empty() { break 'drain; }
                                    }
                                }
                            }

                            if tx_next
                                .send(CandidateBatch {
                                    radius: batch.radius,
                                    kind: CandidateBatchKind::RadiusComplete { total_pubkeys },
                                })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    },
                    None => {
                        channel_open = false;
                    }
                }
            }
        }
    }
    // tx_next dropped here → tier 4 channel closes
}

/// Tier 4 consumer: discovers NIP-65 relay lists with bounded concurrency.
///
/// For pubkeys that tier 3 couldn't resolve:
/// 1. Re-checks User table (background sync may have populated since tier 1)
/// 2. Fetches NIP-65 relay lists from default relays
/// 3. Routes based on relay list presence and tier 3 failure status:
///    - Has relay list → forward (pubkey, relays) to tier 5
///    - No relay list + tier 3 EOSE → cache empty metadata (confirmed absent)
///    - No relay list + tier 3 failed → don't cache (uncertain)
///    - Error + retries left → requeue at back of pending
///    - Error + retries exhausted → drop (no relay list to try)
async fn tier4_relay_list_consumer(
    whitenoise: &Whitenoise,
    tx: &broadcast::Sender<UserSearchUpdate>,
    mut rx: mpsc::Receiver<CandidateBatch>,
    tx_next: mpsc::Sender<UserRelayBatch>,
    query: &str,
    total_results: &AtomicUsize,
    metrics: &metrics::PipelineMetrics,
) {
    // (pubkeys, radius, attempt, tier3_failed)
    let mut pending: VecDeque<(Vec<PublicKey>, u8, u8, bool)> = VecDeque::new();
    let mut in_flight = FuturesUnordered::new();
    let mut channel_open = true;

    loop {
        while in_flight.len() < MAX_CONCURRENT_RELAY_LIST_FETCHES {
            match pending.pop_front() {
                Some((chunk, radius, attempt, tier3_failed)) => {
                    in_flight.push(fetch_chunk_relay_lists(
                        whitenoise,
                        chunk,
                        radius,
                        attempt,
                        tier3_failed,
                    ));
                }
                None => break,
            }
        }

        if in_flight.is_empty() && pending.is_empty() && !channel_open {
            break;
        }

        if tx.receiver_count() == 0 {
            tracing::debug!(
                target: "whitenoise::user_search",
                "Search cancelled - no receivers (tier4)"
            );
            while in_flight.next().await.is_some() {}
            return;
        }

        tokio::select! {
            biased;

            Some(result) = in_flight.next() => {
                if process_tier4_result(
                    whitenoise, result, tx, &tx_next, query, total_results, &mut pending, metrics,
                ).await.is_err() {
                    return;
                }
            }

            batch = rx.recv(), if channel_open => {
                match batch {
                    Some(batch) => {
                        let (pubkeys, tier3_failed) = match batch.kind {
                            CandidateBatchKind::Candidates(pks) => (pks, false),
                            CandidateBatchKind::FailedCandidates(pks) => (pks, true),
                            CandidateBatchKind::RadiusComplete { total_pubkeys } => {
                                // Drain all in-flight + pending before forwarding sentinel.
                                'drain: loop {
                                    while in_flight.len() < MAX_CONCURRENT_RELAY_LIST_FETCHES {
                                        match pending.pop_front() {
                                            Some((chunk, r, att, failed)) => {
                                                in_flight.push(fetch_chunk_relay_lists(
                                                    whitenoise, chunk, r, att, failed,
                                                ));
                                            }
                                            None => break,
                                        }
                                    }
                                    match in_flight.next().await {
                                        Some(r) => {
                                            if process_tier4_result(
                                                whitenoise, r, tx, &tx_next, query, total_results, &mut pending, metrics,
                                            ).await.is_err() {
                                                return;
                                            }
                                        }
                                        None => {
                                            if pending.is_empty() { break 'drain; }
                                        }
                                    }
                                }

                                if tx_next
                                    .send(UserRelayBatch {
                                        radius: batch.radius,
                                        kind: UserRelayBatchKind::RadiusComplete { total_pubkeys },
                                    })
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                                continue;
                            }
                        };

                        metrics
                            .t4_received
                            .fetch_add(pubkeys.len(), Ordering::Relaxed);

                        // Re-check User table (background sync may have completed)
                        let (found, remaining) =
                            graph::check_user_table_metadata(whitenoise, &pubkeys).await;

                        metrics
                            .t4_user_table_found
                            .fetch_add(found.len(), Ordering::Relaxed);
                        match_and_emit(
                            tx, &found, &pubkeys, query, batch.radius, total_results,
                        );

                        if !remaining.is_empty() {
                            pending.push_back((remaining, batch.radius, 0, tier3_failed));
                        }
                    }
                    None => {
                        channel_open = false;
                    }
                }
            }
        }
    }
    // tx_next dropped here → tier 5 channel closes
}

/// Process a completed tier 4 relay list fetch result.
///
/// Routes pubkeys based on relay list presence and tier 3 failure status.
async fn process_tier4_result(
    whitenoise: &Whitenoise,
    result: RelayListFetchResult,
    tx: &broadcast::Sender<UserSearchUpdate>,
    tx_next: &mpsc::Sender<UserRelayBatch>,
    query: &str,
    total_results: &AtomicUsize,
    pending: &mut VecDeque<(Vec<PublicKey>, u8, u8, bool)>,
    metrics: &metrics::PipelineMetrics,
) -> std::result::Result<(), ()> {
    metrics.record_fetch(
        &metrics.t4_fetches,
        &metrics.t4_fetch_us,
        &metrics.t4_fetch_max_us,
        result.duration,
    );

    if result.errored {
        metrics.t4_errors.fetch_add(1, Ordering::Relaxed);
        if result.attempt < MAX_QUEUE_RETRIES {
            pending.push_back((
                result.pubkeys,
                result.radius,
                result.attempt + 1,
                result.tier3_failed,
            ));
        } else {
            metrics.t4_exhausted.fetch_add(1, Ordering::Relaxed);
        }
        // Retries exhausted → drop (no relay list to try in tier 5)
        return Ok(());
    }

    let mut to_tier5: Vec<(PublicKey, Vec<RelayUrl>)> = Vec::new();
    let mut found_metadata = HashMap::new();

    for pk in &result.pubkeys {
        if let Some(relays) = result.relay_map.get(pk) {
            to_tier5.push((*pk, relays.clone()));
        } else if !result.tier3_failed {
            // No relay list + tier 3 EOSE → confirmed absent, cache empty
            metrics.t4_no_relays_cached.fetch_add(1, Ordering::Relaxed);
            let _ =
                CachedGraphUser::upsert_metadata_only(pk, &Metadata::new(), &whitenoise.database)
                    .await;
        } else {
            // No relay list + tier 3 failed → don't cache (uncertain), just drop
            metrics.t4_no_relays_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    // Check if any of the relay-list pubkeys already have User metadata
    // (unlikely but possible from background sync)
    if !to_tier5.is_empty() {
        let tier5_pks: Vec<PublicKey> = to_tier5.iter().map(|(pk, _)| *pk).collect();
        let (found, _) = graph::check_user_table_metadata(whitenoise, &tier5_pks).await;
        found_metadata.extend(found);
    }

    if !found_metadata.is_empty() {
        metrics
            .t4_user_table_found
            .fetch_add(found_metadata.len(), Ordering::Relaxed);
        let all_pks: Vec<PublicKey> = found_metadata.keys().copied().collect();
        match_and_emit(
            tx,
            &found_metadata,
            &all_pks,
            query,
            result.radius,
            total_results,
        );

        // Remove already-found from tier 5 forwarding
        to_tier5.retain(|(pk, _)| !found_metadata.contains_key(pk));
    }

    metrics
        .t4_has_relays
        .fetch_add(to_tier5.len(), Ordering::Relaxed);

    if !to_tier5.is_empty() {
        tx_next
            .send(UserRelayBatch {
                radius: result.radius,
                kind: UserRelayBatchKind::Candidates(to_tier5),
            })
            .await
            .map_err(|_| ())?;
    }

    Ok(())
}

/// Tier 5 consumer: fetches metadata from users' preferred relays.
///
/// For each (pubkey, relay list) pair from tier 4, queries the user's relays
/// for Kind 0 metadata. Uses queue-based retries with bounded concurrency.
///
/// - Found → cache metadata, emit match
/// - EOSE → cache empty metadata (confirmed absent on all known relays)
/// - Error + retries left → requeue
/// - Error + retries exhausted → don't cache (uncertain)
///
/// Emits `RadiusCompleted` and `SearchCompleted` as the final pipeline tier.
async fn tier5_user_relay_consumer(
    whitenoise: &Whitenoise,
    tx: &broadcast::Sender<UserSearchUpdate>,
    mut rx: mpsc::Receiver<UserRelayBatch>,
    query: &str,
    total_results: &AtomicUsize,
    radius_end: u8,
    metrics: &metrics::PipelineMetrics,
) {
    // (pubkey, relays, radius, attempt)
    let mut pending: VecDeque<(PublicKey, Vec<RelayUrl>, u8, u8)> = VecDeque::new();
    let mut in_flight = FuturesUnordered::new();
    let mut channel_open = true;

    loop {
        while in_flight.len() < MAX_CONCURRENT_USER_RELAY_FETCHES {
            match pending.pop_front() {
                Some((pk, relays, radius, attempt)) => {
                    in_flight.push(fetch_user_relay_chunk(
                        whitenoise, pk, relays, radius, attempt,
                    ));
                }
                None => break,
            }
        }

        if in_flight.is_empty() && pending.is_empty() && !channel_open {
            break;
        }

        if tx.receiver_count() == 0 {
            tracing::debug!(
                target: "whitenoise::user_search",
                "Search cancelled - no receivers (tier5)"
            );
            while in_flight.next().await.is_some() {}
            return;
        }

        tokio::select! {
            biased;

            Some(result) = in_flight.next() => {
                process_tier5_result(whitenoise, result, tx, query, total_results, &mut pending, metrics);
            }

            batch = rx.recv(), if channel_open => {
                match batch {
                    Some(batch) => match batch.kind {
                        UserRelayBatchKind::Candidates(pairs) => {
                            metrics
                                .t5_received
                                .fetch_add(pairs.len(), Ordering::Relaxed);
                            for (pk, relays) in pairs {
                                pending.push_back((pk, relays, batch.radius, 0));
                            }
                        }
                        UserRelayBatchKind::RadiusComplete { total_pubkeys } => {
                            // Drain all in-flight + pending before emitting sentinel.
                            'drain: loop {
                                while in_flight.len() < MAX_CONCURRENT_USER_RELAY_FETCHES {
                                    match pending.pop_front() {
                                        Some((pk, relays, r, att)) => {
                                            in_flight.push(fetch_user_relay_chunk(
                                                whitenoise, pk, relays, r, att,
                                            ));
                                        }
                                        None => break,
                                    }
                                }
                                match in_flight.next().await {
                                    Some(r) => {
                                        process_tier5_result(
                                            whitenoise, r, tx, query, total_results, &mut pending, metrics,
                                        );
                                    }
                                    None => {
                                        if pending.is_empty() { break 'drain; }
                                    }
                                }
                            }

                            let _ = tx.send(UserSearchUpdate {
                                trigger: SearchUpdateTrigger::RadiusCompleted {
                                    radius: batch.radius,
                                    total_pubkeys_searched: total_pubkeys,
                                },
                                new_results: vec![],
                                total_result_count: total_results.load(Ordering::Relaxed),
                            });
                        }
                    },
                    None => {
                        channel_open = false;
                    }
                }
            }
        }
    }

    // Channel closed — all tiers done. Emit SearchCompleted.
    #[cfg(feature = "benchmark-tests")]
    metrics.log_summary();

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

/// Process a completed tier 5 per-user relay metadata fetch.
fn process_tier5_result(
    whitenoise: &Whitenoise,
    result: UserRelayFetchResult,
    tx: &broadcast::Sender<UserSearchUpdate>,
    query: &str,
    total_results: &AtomicUsize,
    pending: &mut VecDeque<(PublicKey, Vec<RelayUrl>, u8, u8)>,
    metrics: &metrics::PipelineMetrics,
) {
    metrics.record_fetch(
        &metrics.t5_fetches,
        &metrics.t5_fetch_us,
        &metrics.t5_fetch_max_us,
        result.duration,
    );

    match result.result {
        graph::UserRelayResult::Found(metadata) => {
            metrics.t5_found.fetch_add(1, Ordering::Relaxed);
            let map = HashMap::from([(result.pubkey, *metadata)]);
            match_and_emit(
                tx,
                &map,
                &[result.pubkey],
                query,
                result.radius,
                total_results,
            );
        }
        graph::UserRelayResult::Eose => {
            metrics.t5_eose.fetch_add(1, Ordering::Relaxed);
            // Confirmed absent on all known relays — cache empty.
            // Spawn so we don't block the select loop on a DB write.
            let pk = result.pubkey;
            let db = whitenoise.database.clone();
            tokio::spawn(async move {
                let _ = CachedGraphUser::upsert_metadata_only(&pk, &Metadata::new(), &db).await;
            });
        }
        graph::UserRelayResult::Error => {
            if result.attempt < MAX_QUEUE_RETRIES {
                metrics.t5_error_retried.fetch_add(1, Ordering::Relaxed);
                pending.push_back((
                    result.pubkey,
                    result.relays,
                    result.radius,
                    result.attempt + 1,
                ));
            } else {
                metrics.t5_error_exhausted.fetch_add(1, Ordering::Relaxed);
                // Cache as empty despite uncertainty. We've tried default relays
                // (T3), discovered this user's preferred relays (T4), and retried
                // fetching from those relays (T5) — all without finding metadata.
                // It's possible the user does have metadata on a relay that was
                // temporarily unreachable, but caching empty here prevents these
                // pubkeys from causing repeated timeouts on subsequent searches.
                // The cache entry will expire naturally, allowing a fresh lookup later.
                let pk = result.pubkey;
                let db = whitenoise.database.clone();
                tokio::spawn(async move {
                    let _ = CachedGraphUser::upsert_metadata_only(&pk, &Metadata::new(), &db).await;
                });
            }
        }
    }
}

/// Tier 3: fetch metadata for a chunk of pubkeys from default relays.
///
/// Named function (not closure) so all futures share the same concrete type
/// in FuturesUnordered — no boxing required.
async fn fetch_chunk_metadata(
    whitenoise: &Whitenoise,
    pubkeys: Vec<PublicKey>,
    radius: u8,
    attempt: u8,
) -> ChunkFetchResult {
    let start = Instant::now();
    match graph::try_fetch_network_metadata(whitenoise, &pubkeys).await {
        Ok((found, remaining)) => ChunkFetchResult {
            found,
            remaining,
            all_pubkeys: pubkeys,
            radius,
            attempt,
            errored: false,
            duration: start.elapsed(),
        },
        Err(()) => ChunkFetchResult {
            found: HashMap::new(),
            remaining: vec![],
            all_pubkeys: pubkeys,
            radius,
            attempt,
            errored: true,
            duration: start.elapsed(),
        },
    }
}

/// Tier 4: fetch NIP-65 relay lists for a chunk of pubkeys.
async fn fetch_chunk_relay_lists(
    whitenoise: &Whitenoise,
    pubkeys: Vec<PublicKey>,
    radius: u8,
    attempt: u8,
    tier3_failed: bool,
) -> RelayListFetchResult {
    let start = Instant::now();
    match graph::try_fetch_relay_lists(whitenoise, &pubkeys).await {
        Ok(relay_map) => RelayListFetchResult {
            relay_map,
            pubkeys,
            radius,
            attempt,
            tier3_failed,
            errored: false,
            duration: start.elapsed(),
        },
        Err(()) => RelayListFetchResult {
            relay_map: HashMap::new(),
            pubkeys,
            radius,
            attempt,
            tier3_failed,
            errored: true,
            duration: start.elapsed(),
        },
    }
}

/// Tier 5: fetch metadata for a single pubkey from their preferred relays.
async fn fetch_user_relay_chunk(
    whitenoise: &Whitenoise,
    pubkey: PublicKey,
    relays: Vec<RelayUrl>,
    radius: u8,
    attempt: u8,
) -> UserRelayFetchResult {
    let start = Instant::now();
    let result = graph::try_fetch_user_relay_metadata(whitenoise, &pubkey, &relays).await;
    UserRelayFetchResult {
        pubkey,
        relays,
        radius,
        attempt,
        result,
        duration: start.elapsed(),
    }
}

/// Collect unique follows not already seen into a layer set, respecting the cap.
fn collect_layer_from_follows(
    follows_map: &HashMap<PublicKey, Vec<PublicKey>>,
    seen: &HashSet<PublicKey>,
    layer: &mut HashSet<PublicKey>,
) {
    for follows in follows_map.values() {
        for follow in follows {
            if !seen.contains(follow) && !layer.contains(follow) {
                layer.insert(*follow);
            }
            if layer.len() >= MAX_PUBKEYS_PER_RADIUS {
                return;
            }
        }
    }
}

/// Phase 1: Build partial layer from cached follows (Account + CachedGraphUser).
///
/// Returns (partial layer, pubkeys needing network fetch for follows).
async fn build_cached_layer_from_follows(
    whitenoise: &Whitenoise,
    previous_layer: &HashSet<PublicKey>,
    seen: &HashSet<PublicKey>,
) -> (HashSet<PublicKey>, Vec<PublicKey>) {
    let pubkeys: Vec<PublicKey> = previous_layer.iter().copied().collect();

    let (cached_follows, need_fetch) =
        graph::check_cached_follows_batch(whitenoise, &pubkeys).await;

    let mut layer = HashSet::new();
    collect_layer_from_follows(&cached_follows, seen, &mut layer);

    (layer, need_fetch)
}

/// Phase 2: Fetch remaining follows from network and extend the layer.
///
/// Returns the additional pubkeys discovered (not in partial_layer or seen).
async fn build_network_layer_from_follows(
    whitenoise: &Whitenoise,
    need_fetch: &[PublicKey],
    seen: &HashSet<PublicKey>,
    partial_layer: &HashSet<PublicKey>,
) -> HashSet<PublicKey> {
    if need_fetch.is_empty() {
        return HashSet::new();
    }

    let network_follows = graph::fetch_network_follows(whitenoise, need_fetch).await;

    // Combine seen + partial_layer so we don't duplicate
    let mut combined_seen = seen.clone();
    combined_seen.extend(partial_layer.iter());

    let mut additional = HashSet::new();
    collect_layer_from_follows(&network_follows, &combined_seen, &mut additional);

    additional
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::cached_graph_user::CachedGraphUser;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use crate::whitenoise::users::User;
    use nostr_sdk::{Keys, Metadata};
    use tokio::sync::broadcast;

    fn random_pk() -> PublicKey {
        Keys::generate().public_key()
    }

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
        let followed_keys = Keys::generate();

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
        let target_keys = Keys::generate();

        // Add multiple users to cache who all follow the same target
        let user1 = Keys::generate();
        let user2 = Keys::generate();

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

    // --- apply_cap tests ---

    #[test]
    fn apply_cap_returns_uncapped_when_under_limit() {
        let mut set = HashSet::new();
        for _ in 0..100 {
            set.insert(random_pk());
        }
        let original_len = set.len();
        let (result, was_capped) = apply_cap(set);
        assert_eq!(result.len(), original_len);
        assert!(!was_capped);
    }

    #[test]
    fn apply_cap_caps_when_exceeding_limit() {
        let mut set = HashSet::new();
        for _ in 0..(MAX_PUBKEYS_PER_RADIUS + 500) {
            set.insert(random_pk());
        }
        assert!(set.len() > MAX_PUBKEYS_PER_RADIUS);
        let (result, was_capped) = apply_cap(set);
        assert_eq!(result.len(), MAX_PUBKEYS_PER_RADIUS);
        assert!(was_capped);
    }

    #[test]
    fn apply_cap_exact_limit_is_not_capped() {
        let mut set = HashSet::new();
        while set.len() < MAX_PUBKEYS_PER_RADIUS {
            set.insert(random_pk());
        }
        let (result, was_capped) = apply_cap(set);
        assert_eq!(result.len(), MAX_PUBKEYS_PER_RADIUS);
        assert!(!was_capped);
    }

    // --- collect_layer_from_follows tests ---

    #[test]
    fn collect_layer_from_follows_deduplicates() {
        let pk1 = random_pk();
        let pk2 = random_pk();
        let shared_target = random_pk();

        let mut follows_map = HashMap::new();
        follows_map.insert(pk1, vec![shared_target]);
        follows_map.insert(pk2, vec![shared_target]);

        let seen = HashSet::new();
        let mut layer = HashSet::new();
        collect_layer_from_follows(&follows_map, &seen, &mut layer);

        assert_eq!(layer.len(), 1);
        assert!(layer.contains(&shared_target));
    }

    #[test]
    fn collect_layer_from_follows_excludes_seen() {
        let pk1 = random_pk();
        let target = random_pk();

        let mut follows_map = HashMap::new();
        follows_map.insert(pk1, vec![target]);

        let mut seen = HashSet::new();
        seen.insert(target);

        let mut layer = HashSet::new();
        collect_layer_from_follows(&follows_map, &seen, &mut layer);

        assert!(layer.is_empty());
    }

    #[test]
    fn collect_layer_from_follows_respects_cap() {
        let pk = random_pk();
        // Create more follows than the cap
        let follows: Vec<PublicKey> = (0..MAX_PUBKEYS_PER_RADIUS + 100)
            .map(|_| random_pk())
            .collect();

        let mut follows_map = HashMap::new();
        follows_map.insert(pk, follows);

        let seen = HashSet::new();
        let mut layer = HashSet::new();
        collect_layer_from_follows(&follows_map, &seen, &mut layer);

        assert_eq!(layer.len(), MAX_PUBKEYS_PER_RADIUS);
    }

    // --- match_and_emit tests ---

    #[test]
    fn match_and_emit_emits_matching_results() {
        let (tx, mut rx) = broadcast::channel(100);
        let total_results = AtomicUsize::new(0);

        let pk = random_pk();
        let mut metadata_map = HashMap::new();
        metadata_map.insert(pk, Metadata::new().name("alice"));

        match_and_emit(&tx, &metadata_map, &[pk], "alice", 1, &total_results);

        let update = rx.try_recv().unwrap();
        assert_eq!(update.new_results.len(), 1);
        assert_eq!(update.new_results[0].pubkey, pk);
        assert_eq!(update.new_results[0].radius, 1);
        assert_eq!(update.total_result_count, 1);
        assert_eq!(total_results.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn match_and_emit_skips_non_matching() {
        let (tx, mut rx) = broadcast::channel(100);
        let total_results = AtomicUsize::new(0);

        let pk = random_pk();
        let mut metadata_map = HashMap::new();
        metadata_map.insert(pk, Metadata::new().name("bob"));

        match_and_emit(&tx, &metadata_map, &[pk], "alice", 1, &total_results);

        assert!(rx.try_recv().is_err(), "Should not emit when no match");
        assert_eq!(total_results.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn match_and_emit_skips_missing_pubkeys() {
        let (tx, mut rx) = broadcast::channel(100);
        let total_results = AtomicUsize::new(0);

        let pk = random_pk();
        let metadata_map: HashMap<PublicKey, Metadata> = HashMap::new();

        match_and_emit(&tx, &metadata_map, &[pk], "alice", 1, &total_results);

        assert!(
            rx.try_recv().is_err(),
            "Should not emit when pubkey missing from map"
        );
    }

    #[test]
    fn match_and_emit_sorts_results_by_sort_key() {
        let (tx, mut rx) = broadcast::channel(100);
        let total_results = AtomicUsize::new(0);

        let pk1 = random_pk();
        let pk2 = random_pk();
        let mut metadata_map = HashMap::new();
        // pk1: contains match on about
        metadata_map.insert(pk1, Metadata::new().about("alice is great"));
        // pk2: exact match on name
        metadata_map.insert(pk2, Metadata::new().name("alice"));

        match_and_emit(&tx, &metadata_map, &[pk1, pk2], "alice", 0, &total_results);

        let update = rx.try_recv().unwrap();
        assert_eq!(update.new_results.len(), 2);
        // Exact name match should come first
        assert_eq!(update.new_results[0].pubkey, pk2);
        assert_eq!(update.new_results[0].match_quality, MatchQuality::Exact);
    }

    #[test]
    fn match_and_emit_accumulates_total_count() {
        let (tx, _rx) = broadcast::channel(100);
        let total_results = AtomicUsize::new(5);

        let pk = random_pk();
        let mut metadata_map = HashMap::new();
        metadata_map.insert(pk, Metadata::new().name("test"));

        match_and_emit(&tx, &metadata_map, &[pk], "test", 0, &total_results);

        assert_eq!(total_results.load(Ordering::Relaxed), 6);
    }

    // --- push_candidates tests ---

    #[tokio::test]
    async fn push_candidates_batches_by_pubkey_batch_size() {
        let (tx, mut rx) = mpsc::channel(100);
        let metrics = metrics::PipelineMetrics::new();

        // Create exactly PUBKEY_BATCH_SIZE + 1 pubkeys to get 2 batches
        let mut pubkeys = HashSet::new();
        for _ in 0..PUBKEY_BATCH_SIZE + 1 {
            pubkeys.insert(random_pk());
        }

        push_candidates(&tx, &pubkeys, 1, &metrics).await.unwrap();

        // Should have received 2 batches
        let batch1 = rx.recv().await.unwrap();
        let batch2 = rx.recv().await.unwrap();

        match (&batch1.kind, &batch2.kind) {
            (CandidateBatchKind::Candidates(b1), CandidateBatchKind::Candidates(b2)) => {
                assert_eq!(b1.len() + b2.len(), pubkeys.len());
                assert!(b1.len() <= PUBKEY_BATCH_SIZE);
                assert!(b2.len() <= PUBKEY_BATCH_SIZE);
            }
            _ => panic!("Expected Candidates batches"),
        }

        assert_eq!(
            metrics.producer_pubkeys.load(Ordering::Relaxed),
            pubkeys.len()
        );
    }

    #[tokio::test]
    async fn push_radius_complete_sends_sentinel() {
        let (tx, mut rx) = mpsc::channel(10);

        push_radius_complete(&tx, 3, 42).await.unwrap();

        let batch = rx.recv().await.unwrap();
        assert_eq!(batch.radius, 3);
        match batch.kind {
            CandidateBatchKind::RadiusComplete { total_pubkeys } => {
                assert_eq!(total_pubkeys, 42);
            }
            _ => panic!("Expected RadiusComplete"),
        }
    }

    // --- metrics tests ---

    #[test]
    fn pipeline_metrics_record_fetch_tracks_timing() {
        let metrics = metrics::PipelineMetrics::new();
        let duration = Duration::from_millis(150);

        metrics.record_fetch(
            &metrics.t3_fetches,
            &metrics.t3_fetch_us,
            &metrics.t3_fetch_max_us,
            duration,
        );

        assert_eq!(metrics.t3_fetches.load(Ordering::Relaxed), 1);
        assert!(metrics.t3_fetch_us.load(Ordering::Relaxed) > 0);
        assert!(metrics.t3_fetch_max_us.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn pipeline_metrics_record_t3_result_success() {
        let metrics = metrics::PipelineMetrics::new();
        let duration = Duration::from_millis(100);

        metrics.record_t3_result(5, 3, 8, 0, false, duration);

        assert_eq!(metrics.t3_found.load(Ordering::Relaxed), 5);
        assert_eq!(metrics.t3_forwarded_ok.load(Ordering::Relaxed), 3);
        assert_eq!(metrics.t3_ok_by_attempt[0].load(Ordering::Relaxed), 1);
    }

    #[test]
    fn pipeline_metrics_record_t3_result_error_retryable() {
        let metrics = metrics::PipelineMetrics::new();
        let duration = Duration::from_millis(100);

        // Error on attempt 1 (< MAX_QUEUE_RETRIES) - should count as error but not exhausted
        metrics.record_t3_result(0, 0, 8, 1, true, duration);

        assert_eq!(metrics.t3_errors.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.t3_exhausted.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn pipeline_metrics_record_t3_result_error_exhausted() {
        let metrics = metrics::PipelineMetrics::new();
        let duration = Duration::from_millis(100);

        // Error at MAX_QUEUE_RETRIES - should be exhausted
        metrics.record_t3_result(0, 0, 8, MAX_QUEUE_RETRIES, true, duration);

        assert_eq!(metrics.t3_errors.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.t3_exhausted.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.t3_forwarded_failed.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn pipeline_metrics_record_fetch_tracks_max() {
        let metrics = metrics::PipelineMetrics::new();

        metrics.record_fetch(
            &metrics.t5_fetches,
            &metrics.t5_fetch_us,
            &metrics.t5_fetch_max_us,
            Duration::from_millis(100),
        );
        metrics.record_fetch(
            &metrics.t5_fetches,
            &metrics.t5_fetch_us,
            &metrics.t5_fetch_max_us,
            Duration::from_millis(200),
        );
        metrics.record_fetch(
            &metrics.t5_fetches,
            &metrics.t5_fetch_us,
            &metrics.t5_fetch_max_us,
            Duration::from_millis(50),
        );

        assert_eq!(metrics.t5_fetches.load(Ordering::Relaxed), 3);
        // Max should be ~200ms = 200000us
        let max_us = metrics.t5_fetch_max_us.load(Ordering::Relaxed);
        assert!(max_us >= 200_000);
    }

    // --- tier1/tier2 consumer passthrough tests ---

    #[tokio::test]
    async fn tier2_cache_consumer_forwards_confirmed_absent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a cached user with empty metadata (confirmed absent)
        let pk = random_pk();
        let cached = CachedGraphUser::new(pk, Some(Metadata::new()), Some(vec![]));
        cached.upsert(&whitenoise.database).await.unwrap();

        let (tx_broadcast, _rx_broadcast) = broadcast::channel(100);
        let (tx_in, rx_in) = mpsc::channel(10);
        let (tx_out, mut rx_out) = mpsc::channel(10);
        let total_results = AtomicUsize::new(0);
        let metrics = metrics::PipelineMetrics::new();

        // Send a batch with the confirmed-absent pubkey
        tx_in
            .send(CandidateBatch {
                radius: 1,
                kind: CandidateBatchKind::Candidates(vec![pk]),
            })
            .await
            .unwrap();
        drop(tx_in);

        tier2_cache_consumer(
            &whitenoise,
            &tx_broadcast,
            rx_in,
            tx_out,
            "anything",
            &total_results,
            &metrics,
        )
        .await;

        // The confirmed-absent pubkey should NOT be forwarded to tier 3
        // (it should be stopped at tier 2)
        let result = rx_out.try_recv();
        assert!(
            result.is_err(),
            "Confirmed-absent pubkey should not be forwarded to tier 3"
        );
    }

    #[tokio::test]
    async fn tier1_forwards_failed_candidates_passthrough() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (tx_broadcast, _rx_broadcast) = broadcast::channel(100);
        let (tx_in, rx_in) = mpsc::channel(10);
        let (tx_out, mut rx_out) = mpsc::channel(10);
        let total_results = AtomicUsize::new(0);
        let metrics = metrics::PipelineMetrics::new();

        let pk = random_pk();
        tx_in
            .send(CandidateBatch {
                radius: 2,
                kind: CandidateBatchKind::FailedCandidates(vec![pk]),
            })
            .await
            .unwrap();
        drop(tx_in);

        tier1_user_table_consumer(
            &whitenoise,
            &tx_broadcast,
            rx_in,
            tx_out,
            "test",
            &total_results,
            &metrics,
        )
        .await;

        let batch = rx_out.try_recv().unwrap();
        match batch.kind {
            CandidateBatchKind::FailedCandidates(pks) => {
                assert_eq!(pks.len(), 1);
                assert_eq!(pks[0], pk);
            }
            _ => panic!("Expected FailedCandidates passthrough"),
        }
    }

    #[tokio::test]
    async fn tier1_forwards_radius_complete_passthrough() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (tx_broadcast, _rx_broadcast) = broadcast::channel(100);
        let (tx_in, rx_in) = mpsc::channel(10);
        let (tx_out, mut rx_out) = mpsc::channel(10);
        let total_results = AtomicUsize::new(0);
        let metrics = metrics::PipelineMetrics::new();

        tx_in
            .send(CandidateBatch {
                radius: 1,
                kind: CandidateBatchKind::RadiusComplete { total_pubkeys: 42 },
            })
            .await
            .unwrap();
        drop(tx_in);

        tier1_user_table_consumer(
            &whitenoise,
            &tx_broadcast,
            rx_in,
            tx_out,
            "test",
            &total_results,
            &metrics,
        )
        .await;

        let batch = rx_out.try_recv().unwrap();
        match batch.kind {
            CandidateBatchKind::RadiusComplete { total_pubkeys } => {
                assert_eq!(total_pubkeys, 42);
            }
            _ => panic!("Expected RadiusComplete passthrough"),
        }
    }

    #[tokio::test]
    async fn tier2_forwards_failed_candidates_passthrough() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (tx_broadcast, _rx_broadcast) = broadcast::channel(100);
        let (tx_in, rx_in) = mpsc::channel(10);
        let (tx_out, mut rx_out) = mpsc::channel(10);
        let total_results = AtomicUsize::new(0);
        let metrics = metrics::PipelineMetrics::new();

        let pk = random_pk();
        tx_in
            .send(CandidateBatch {
                radius: 2,
                kind: CandidateBatchKind::FailedCandidates(vec![pk]),
            })
            .await
            .unwrap();
        drop(tx_in);

        tier2_cache_consumer(
            &whitenoise,
            &tx_broadcast,
            rx_in,
            tx_out,
            "test",
            &total_results,
            &metrics,
        )
        .await;

        let batch = rx_out.try_recv().unwrap();
        match batch.kind {
            CandidateBatchKind::FailedCandidates(pks) => {
                assert_eq!(pks[0], pk);
            }
            _ => panic!("Expected FailedCandidates passthrough"),
        }
    }

    #[tokio::test]
    async fn tier1_cancels_when_broadcast_receiver_dropped() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (tx_broadcast, rx_broadcast) = broadcast::channel(100);
        let (tx_in, rx_in) = mpsc::channel(10);
        let (tx_out, _rx_out) = mpsc::channel(10);
        let total_results = AtomicUsize::new(0);
        let metrics = metrics::PipelineMetrics::new();

        // Drop the broadcast receiver so receiver_count() == 0
        drop(rx_broadcast);

        let pk = random_pk();
        tx_in
            .send(CandidateBatch {
                radius: 0,
                kind: CandidateBatchKind::Candidates(vec![pk]),
            })
            .await
            .unwrap();

        // The consumer should detect no receivers and exit
        tier1_user_table_consumer(
            &whitenoise,
            &tx_broadcast,
            rx_in,
            tx_out,
            "test",
            &total_results,
            &metrics,
        )
        .await;
        // Test passes if no hang
    }

    #[tokio::test]
    async fn tier2_cancels_when_broadcast_receiver_dropped() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (tx_broadcast, rx_broadcast) = broadcast::channel(100);
        let (tx_in, rx_in) = mpsc::channel(10);
        let (tx_out, _rx_out) = mpsc::channel(10);
        let total_results = AtomicUsize::new(0);
        let metrics = metrics::PipelineMetrics::new();

        drop(rx_broadcast);

        let pk = random_pk();
        tx_in
            .send(CandidateBatch {
                radius: 0,
                kind: CandidateBatchKind::Candidates(vec![pk]),
            })
            .await
            .unwrap();

        tier2_cache_consumer(
            &whitenoise,
            &tx_broadcast,
            rx_in,
            tx_out,
            "test",
            &total_results,
            &metrics,
        )
        .await;
    }

    #[tokio::test]
    async fn tier1_exits_when_next_channel_closed() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (tx_broadcast, _rx_broadcast) = broadcast::channel(100);
        let (tx_in, rx_in) = mpsc::channel(10);
        let (tx_out, rx_out) = mpsc::channel(10);
        let total_results = AtomicUsize::new(0);
        let metrics = metrics::PipelineMetrics::new();

        // Drop the output receiver so the send will fail
        drop(rx_out);

        // Send a batch with an unknown pubkey so it will try to forward
        let pk = random_pk();
        tx_in
            .send(CandidateBatch {
                radius: 0,
                kind: CandidateBatchKind::Candidates(vec![pk]),
            })
            .await
            .unwrap();
        drop(tx_in);

        // Should exit when forward fails
        tier1_user_table_consumer(
            &whitenoise,
            &tx_broadcast,
            rx_in,
            tx_out,
            "test",
            &total_results,
            &metrics,
        )
        .await;
    }

    #[tokio::test]
    async fn tier2_exits_when_next_channel_closed() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (tx_broadcast, _rx_broadcast) = broadcast::channel(100);
        let (tx_in, rx_in) = mpsc::channel(10);
        let (tx_out, rx_out) = mpsc::channel(10);
        let total_results = AtomicUsize::new(0);
        let metrics = metrics::PipelineMetrics::new();

        drop(rx_out);

        let pk = random_pk();
        tx_in
            .send(CandidateBatch {
                radius: 0,
                kind: CandidateBatchKind::Candidates(vec![pk]),
            })
            .await
            .unwrap();
        drop(tx_in);

        tier2_cache_consumer(
            &whitenoise,
            &tx_broadcast,
            rx_in,
            tx_out,
            "test",
            &total_results,
            &metrics,
        )
        .await;
    }

    #[tokio::test]
    async fn tier1_finds_and_forwards_in_same_batch() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create one known user and one unknown
        let known_pk = random_pk();
        let user = User {
            id: None,
            pubkey: known_pk,
            metadata: Metadata::new().name("KnownUser"),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        user.save(&whitenoise.database).await.unwrap();

        let unknown_pk = random_pk();

        let (tx_broadcast, mut rx_broadcast) = broadcast::channel(100);
        let (tx_in, rx_in) = mpsc::channel(10);
        let (tx_out, mut rx_out) = mpsc::channel(10);
        let total_results = AtomicUsize::new(0);
        let metrics = metrics::PipelineMetrics::new();

        tx_in
            .send(CandidateBatch {
                radius: 1,
                kind: CandidateBatchKind::Candidates(vec![known_pk, unknown_pk]),
            })
            .await
            .unwrap();
        drop(tx_in);

        tier1_user_table_consumer(
            &whitenoise,
            &tx_broadcast,
            rx_in,
            tx_out,
            "knownuser",
            &total_results,
            &metrics,
        )
        .await;

        // Should have emitted a match for the known user
        let update = rx_broadcast.try_recv().unwrap();
        assert!(matches!(update.trigger, SearchUpdateTrigger::ResultsFound));
        assert_eq!(update.new_results.len(), 1);
        assert_eq!(update.new_results[0].pubkey, known_pk);

        // Should have forwarded the unknown pubkey
        let forwarded = rx_out.try_recv().unwrap();
        match forwarded.kind {
            CandidateBatchKind::Candidates(pks) => {
                assert!(pks.contains(&unknown_pk));
                assert!(!pks.contains(&known_pk));
            }
            _ => panic!("Expected Candidates"),
        }
    }

    #[tokio::test]
    async fn tier2_finds_cached_and_forwards_unknown() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create one cached user with real metadata and one unknown
        let cached_pk = random_pk();
        let cached = CachedGraphUser::new(
            cached_pk,
            Some(Metadata::new().name("CachedHit")),
            Some(vec![]),
        );
        cached.upsert(&whitenoise.database).await.unwrap();

        let unknown_pk = random_pk();

        let (tx_broadcast, mut rx_broadcast) = broadcast::channel(100);
        let (tx_in, rx_in) = mpsc::channel(10);
        let (tx_out, mut rx_out) = mpsc::channel(10);
        let total_results = AtomicUsize::new(0);
        let metrics = metrics::PipelineMetrics::new();

        tx_in
            .send(CandidateBatch {
                radius: 1,
                kind: CandidateBatchKind::Candidates(vec![cached_pk, unknown_pk]),
            })
            .await
            .unwrap();
        drop(tx_in);

        tier2_cache_consumer(
            &whitenoise,
            &tx_broadcast,
            rx_in,
            tx_out,
            "cachedhit",
            &total_results,
            &metrics,
        )
        .await;

        // Should have emitted a match for the cached user
        let update = rx_broadcast.try_recv().unwrap();
        assert!(matches!(update.trigger, SearchUpdateTrigger::ResultsFound));
        assert_eq!(update.new_results[0].pubkey, cached_pk);

        // Should have forwarded the unknown pubkey to tier 3
        let forwarded = rx_out.try_recv().unwrap();
        match forwarded.kind {
            CandidateBatchKind::Candidates(pks) => {
                assert!(pks.contains(&unknown_pk));
            }
            _ => panic!("Expected Candidates"),
        }
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

        let followed_keys = Keys::generate();

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

    /// When the searcher has no follows, the social graph is empty and can't
    /// expand. The fallback seed should be injected so its follows become
    /// searchable candidates.
    #[tokio::test]
    async fn search_injects_fallback_seed_when_graph_is_empty() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        // Account has no follows — social graph is empty.

        let seed_pk = PublicKey::parse(FALLBACK_SEED_PUBKEY).unwrap();

        // Pre-populate the seed's follows in the cache
        let discoverable_pk = random_pk();
        let cached_seed = CachedGraphUser::new(
            seed_pk,
            Some(Metadata::new().name("SeedUser")),
            Some(vec![discoverable_pk]),
        );
        cached_seed.upsert(&whitenoise.database).await.unwrap();

        // Pre-populate the discoverable user's metadata
        let cached_target = CachedGraphUser::new(
            discoverable_pk,
            Some(Metadata::new().name("FallbackTarget")),
            Some(vec![]),
        );
        cached_target.upsert(&whitenoise.database).await.unwrap();

        // Search at radius 0-2: radius 1 should be empty, triggering fallback
        let updates = run_search(&whitenoise, "fallbacktarget", account.pubkey, 0, 2).await;

        let found_target = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .any(|r| r.pubkey == discoverable_pk);

        assert!(
            found_target,
            "Should find the seed's follow via fallback injection"
        );
    }

    /// The fallback seed itself should be searchable (findable by metadata).
    #[tokio::test]
    async fn search_fallback_seed_itself_is_findable() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let seed_pk = PublicKey::parse(FALLBACK_SEED_PUBKEY).unwrap();

        let cached_seed = CachedGraphUser::new(
            seed_pk,
            Some(Metadata::new().name("SeedAccount")),
            Some(vec![]),
        );
        cached_seed.upsert(&whitenoise.database).await.unwrap();

        let updates = run_search(&whitenoise, "seedaccount", account.pubkey, 0, 2).await;

        let found_seed = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .any(|r| r.pubkey == seed_pk);

        assert!(found_seed, "The fallback seed itself should be findable");
    }

    /// When the searcher has follows, the fallback seed should NOT be injected.
    #[tokio::test]
    async fn search_does_not_inject_fallback_when_graph_has_follows() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let seed_pk = PublicKey::parse(FALLBACK_SEED_PUBKEY).unwrap();

        // Give the searcher a follow so the graph is not empty
        let followed_pk = random_pk();
        whitenoise
            .follow_user(&account, &followed_pk)
            .await
            .unwrap();

        // Pre-populate seed metadata
        let cached_seed = CachedGraphUser::new(
            seed_pk,
            Some(Metadata::new().name("SeedAccount")),
            Some(vec![]),
        );
        cached_seed.upsert(&whitenoise.database).await.unwrap();

        let updates = run_search(&whitenoise, "seedaccount", account.pubkey, 0, 2).await;

        let found_seed = updates
            .iter()
            .filter(|u| matches!(u.trigger, SearchUpdateTrigger::ResultsFound))
            .flat_map(|u| &u.new_results)
            .any(|r| r.pubkey == seed_pk);

        assert!(
            !found_seed,
            "Fallback seed should NOT appear when graph is not empty"
        );
    }
}
