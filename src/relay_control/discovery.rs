use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use nostr_sdk::RelayUrl;
use nostr_sdk::prelude::*;
use tokio::sync::mpsc::Sender;
use tokio::sync::{RwLock, broadcast};

use super::{
    RelayPlane, SubscriptionStream,
    sessions::{
        RelaySession, RelaySessionAuthPolicy, RelaySessionConfig, RelaySessionReconnectPolicy,
    },
};
use crate::{
    nostr_manager::Result,
    perf_instrument,
    types::{DiscoveryPlaneStateSnapshot, ProcessableEvent},
};

const MAX_USERS_PER_PUBLIC_DISCOVERY_SUBSCRIPTION: usize = 500;

fn batch_count_for(user_count: usize) -> usize {
    user_count.div_ceil(MAX_USERS_PER_PUBLIC_DISCOVERY_SUBSCRIPTION)
}

/// Derives a stable batch index from a pubkey. Pubkeys are uniformly distributed
/// (secp256k1 curve points), so any byte range gives even distribution.
fn pubkey_batch_index(pubkey: &PublicKey, batch_count: usize) -> usize {
    let bytes = pubkey.to_bytes();
    // Use the tail bytes — vanity addresses grind the prefix, biasing leading bytes.
    let offset = bytes.len() - std::mem::size_of::<usize>();
    let n = usize::from_le_bytes(std::array::from_fn(|i| bytes[offset + i]));
    n % batch_count
}

fn assign_users_to_batches(users: &[PublicKey], batch_count: usize) -> Vec<Vec<PublicKey>> {
    let mut batches = vec![Vec::new(); batch_count];
    for &user in users {
        let batch_index = pubkey_batch_index(&user, batch_count);
        batches[batch_index].push(user);
    }
    // Sort within each batch for consistent hashing.
    for batch in &mut batches {
        batch.sort_unstable_by_key(|pk| pk.to_hex());
    }
    batches
}

fn hash_pubkey_slice(pubkeys: &[PublicKey]) -> u64 {
    let mut hasher = DefaultHasher::new();
    pubkeys.hash(&mut hasher);
    hasher.finish()
}

/// Returns subscription IDs present in `old` but not in `new`.
fn stale_ids(old: Vec<SubscriptionId>, new: &[SubscriptionId]) -> Vec<SubscriptionId> {
    let new_set: std::collections::HashSet<String> = new.iter().map(|id| id.to_string()).collect();
    old.into_iter()
        .filter(|id| !new_set.contains(&id.to_string()))
        .collect()
}

/// Configuration for the long-lived discovery plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveryPlaneConfig {
    pub(crate) relays: Vec<RelayUrl>,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
}

impl Default for DiscoveryPlaneConfig {
    fn default() -> Self {
        Self {
            relays: Self::curated_default_relays(),
            reconnect_policy: RelaySessionReconnectPolicy::Conservative,
        }
    }
}

impl DiscoveryPlaneConfig {
    pub(crate) fn new(relays: Vec<RelayUrl>) -> Self {
        Self {
            relays,
            reconnect_policy: RelaySessionReconnectPolicy::Conservative,
        }
    }

    /// Initial curated relay set from the planning doc.
    pub(crate) fn curated_default_relays() -> Vec<RelayUrl> {
        [
            "wss://index.hzrd149.com",
            "wss://indexer.coracle.social",
            "wss://relay.primal.net",
            "wss://relay.damus.io",
            "wss://relay.ditto.pub",
            "wss://nos.lol",
        ]
        .into_iter()
        .map(|relay| {
            RelayUrl::parse(relay)
                .unwrap_or_else(|error| panic!("invalid curated relay {relay}: {error}"))
        })
        .collect()
    }
}

#[derive(Debug)]
pub(crate) struct DiscoveryPlane {
    config: DiscoveryPlaneConfig,
    session: RelaySession,
    state: RwLock<DiscoveryPlaneState>,
}

#[derive(Debug, Default)]
struct DiscoveryPlaneState {
    // User-data subscriptions
    public_subscription_ids: Vec<SubscriptionId>,
    watched_user_count: usize,
    public_since: Option<Timestamp>,
    previous_user_set_hash: Option<u64>,
    previous_batch_count: Option<usize>,
    previous_batch_hashes: Vec<u64>,

    // Follow-list subscriptions
    follow_list_subscription_ids: Vec<SubscriptionId>,
    follow_list_account_count: usize,
    previous_follow_set_hash: Option<u64>,

    // Mute-list subscriptions (NIP-51 kind 10000)
    mute_list_subscription_ids: Vec<SubscriptionId>,
    mute_list_account_count: usize,
    previous_mute_set_hash: Option<u64>,
}

impl DiscoveryPlane {
    pub(crate) fn new(
        config: DiscoveryPlaneConfig,
        event_sender: Sender<ProcessableEvent>,
    ) -> Self {
        let mut session_config = RelaySessionConfig::new(RelayPlane::Discovery);
        session_config.auth_policy = RelaySessionAuthPolicy::Disabled;
        session_config.reconnect_policy = config.reconnect_policy;
        session_config.min_connected_relays = Some(2);

        Self {
            config,
            session: RelaySession::new(session_config, event_sender),
            state: RwLock::new(DiscoveryPlaneState::default()),
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn start(&self) -> Result<()> {
        self.session
            .ensure_relays_connected(&self.config.relays)
            .await
    }

    // ── Watched-user subscriptions ───────────────────────────────────────

    /// Syncs discovery subscriptions for watched users (metadata, relay lists,
    /// inbox relays, key package relays). Skips relay work when the user set
    /// is unchanged; when it did change, only re-subscribes the affected
    /// batches via hash-based assignment.
    #[perf_instrument("relay")]
    pub(crate) async fn sync_watched_users(
        &self,
        watched_users: &[PublicKey],
        public_since: Option<Timestamp>,
    ) -> Result<()> {
        let mut watched_users = watched_users.to_vec();
        watched_users.sort_unstable_by_key(|pubkey| pubkey.to_hex());
        watched_users.dedup();

        let user_set_hash = hash_pubkey_slice(&watched_users);

        let (hash_unchanged, previous_batch_count, previous_batch_hashes) = {
            let state = self.state.read().await;
            (
                state.previous_user_set_hash == Some(user_set_hash),
                state.previous_batch_count,
                state.previous_batch_hashes.clone(),
            )
        };

        if hash_unchanged {
            tracing::debug!(
                target: "whitenoise::relay_control::discovery",
                watched_users = watched_users.len(),
                "Watched-user sync skipped (user set unchanged)"
            );
            return Ok(());
        }

        if !self
            .session
            .has_any_relay_connected(&self.config.relays)
            .await
        {
            self.start().await?;
        }

        let (new_ids, new_batch_hashes) = self
            .subscribe_watched_user_batches(
                &watched_users,
                public_since,
                previous_batch_count,
                &previous_batch_hashes,
            )
            .await?;

        let stale = {
            let mut state = self.state.write().await;
            let old_ids = std::mem::replace(&mut state.public_subscription_ids, new_ids);
            state.watched_user_count = watched_users.len();
            state.public_since = public_since;
            state.previous_user_set_hash = Some(user_set_hash);
            state.previous_batch_count = Some(batch_count_for(watched_users.len()));
            state.previous_batch_hashes = new_batch_hashes;
            stale_ids(old_ids, &state.public_subscription_ids)
        };
        for id in stale {
            self.session.unsubscribe(&id).await;
        }

        Ok(())
    }

    /// Assigns users to batches via hash and subscribes only the changed batches.
    async fn subscribe_watched_user_batches(
        &self,
        watched_users: &[PublicKey],
        public_since: Option<Timestamp>,
        previous_batch_count: Option<usize>,
        previous_batch_hashes: &[u64],
    ) -> Result<(Vec<SubscriptionId>, Vec<u64>)> {
        let batch_count = batch_count_for(watched_users.len());
        let batches = if batch_count > 0 {
            assign_users_to_batches(watched_users, batch_count)
        } else {
            Vec::new()
        };

        let new_batch_hashes: Vec<u64> = batches.iter().map(|b| hash_pubkey_slice(b)).collect();
        let batch_count_changed = previous_batch_count != Some(batch_count);

        let mut new_ids = Vec::new();
        let mut batches_skipped = 0usize;

        for (batch_index, authors) in batches.iter().enumerate() {
            let subscription_id = SubscriptionId::new(format!("discovery_user_data_{batch_index}"));

            let hash_changed = batch_count_changed
                || previous_batch_hashes
                    .get(batch_index)
                    .is_none_or(|prev| *prev != new_batch_hashes[batch_index]);

            if hash_changed {
                let mut filter = Filter::new().authors(authors.clone()).kinds([
                    Kind::Metadata,
                    Kind::RelayList,
                    Kind::InboxRelays,
                    Kind::MlsKeyPackageRelays,
                ]);
                if let Some(public_since) = public_since {
                    filter = filter.since(public_since);
                }

                self.session
                    .subscribe_with_id_to(
                        &self.config.relays,
                        subscription_id.clone(),
                        filter,
                        SubscriptionStream::DiscoveryUserData,
                        None,
                        &[],
                    )
                    .await?;
            } else {
                batches_skipped += 1;
            }

            new_ids.push(subscription_id);
        }

        if batches_skipped > 0 {
            tracing::debug!(
                target: "whitenoise::relay_control::discovery",
                batches_skipped,
                batches_updated = batch_count - batches_skipped,
                "Watched-user sync: skipped unchanged batches"
            );
        }

        Ok((new_ids, new_batch_hashes))
    }

    // ── Follow-list subscriptions ────────────────────────────────────────

    /// Syncs follow-list subscriptions for each account. Skips relay work
    /// when the account set is unchanged.
    ///
    /// Account pubkeys are unique by DB primary key constraint, so no
    /// deduplication is needed.
    #[perf_instrument("relay")]
    pub(crate) async fn sync_follow_lists(
        &self,
        follow_list_accounts: &[(PublicKey, Option<Timestamp>)],
    ) -> Result<()> {
        let mut follow_pubkeys: Vec<PublicKey> =
            follow_list_accounts.iter().map(|(pk, _)| *pk).collect();
        follow_pubkeys.sort_unstable_by_key(|pk| pk.to_hex());
        follow_pubkeys.dedup();

        let follow_set_hash = hash_pubkey_slice(&follow_pubkeys);

        let hash_unchanged = {
            let state = self.state.read().await;
            state.previous_follow_set_hash == Some(follow_set_hash)
        };

        if hash_unchanged {
            tracing::debug!(
                target: "whitenoise::relay_control::discovery",
                follow_accounts = follow_pubkeys.len(),
                "Follow list sync skipped (account set unchanged)"
            );
            return Ok(());
        }

        if !self
            .session
            .has_any_relay_connected(&self.config.relays)
            .await
        {
            self.start().await?;
        }

        let mut new_ids = Vec::new();
        for (index, (account_pubkey, since)) in follow_list_accounts.iter().enumerate() {
            let mut filter = Filter::new()
                .kind(Kind::ContactList)
                .author(*account_pubkey);
            if let Some(since) = since {
                filter = filter.since(*since);
            }

            let subscription_id = SubscriptionId::new(format!("discovery_follow_{index}"));
            self.session
                .subscribe_with_id_to(
                    &self.config.relays,
                    subscription_id.clone(),
                    filter,
                    SubscriptionStream::DiscoveryFollowLists,
                    Some(*account_pubkey),
                    &[],
                )
                .await?;
            new_ids.push(subscription_id);
        }

        let stale = {
            let mut state = self.state.write().await;
            let old_ids = std::mem::replace(&mut state.follow_list_subscription_ids, new_ids);
            state.follow_list_account_count = follow_list_accounts.len();
            state.previous_follow_set_hash = Some(follow_set_hash);
            stale_ids(old_ids, &state.follow_list_subscription_ids)
        };
        for id in stale {
            self.session.unsubscribe(&id).await;
        }

        Ok(())
    }

    // ── Mute-list subscriptions (NIP-51) ─────────────────────────────────

    /// Syncs mute-list subscriptions for each account (same `since` policy as follow lists).
    #[perf_instrument("relay")]
    pub(crate) async fn sync_mute_lists(
        &self,
        mute_list_accounts: &[(PublicKey, Option<Timestamp>)],
    ) -> Result<()> {
        let mut mute_pubkeys: Vec<PublicKey> =
            mute_list_accounts.iter().map(|(pk, _)| *pk).collect();
        mute_pubkeys.sort_unstable_by_key(|pk| pk.to_hex());
        mute_pubkeys.dedup();

        let mute_set_hash = hash_pubkey_slice(&mute_pubkeys);

        let hash_unchanged = {
            let state = self.state.read().await;
            state.previous_mute_set_hash == Some(mute_set_hash)
        };

        if hash_unchanged {
            tracing::debug!(
                target: "whitenoise::relay_control::discovery",
                mute_accounts = mute_pubkeys.len(),
                "Mute list sync skipped (account set unchanged)"
            );
            return Ok(());
        }

        if !self
            .session
            .has_any_relay_connected(&self.config.relays)
            .await
        {
            self.start().await?;
        }

        let mut new_ids = Vec::new();
        for (index, (account_pubkey, since)) in mute_list_accounts.iter().enumerate() {
            let mut filter = Filter::new().kind(Kind::MuteList).author(*account_pubkey);
            if let Some(since) = since {
                filter = filter.since(*since);
            }

            let subscription_id = SubscriptionId::new(format!("discovery_mute_{index}"));
            self.session
                .subscribe_with_id_to(
                    &self.config.relays,
                    subscription_id.clone(),
                    filter,
                    SubscriptionStream::DiscoveryMuteLists,
                    Some(*account_pubkey),
                    &[],
                )
                .await?;
            new_ids.push(subscription_id);
        }

        let stale = {
            let mut state = self.state.write().await;
            let old_ids = std::mem::replace(&mut state.mute_list_subscription_ids, new_ids);
            state.mute_list_account_count = mute_list_accounts.len();
            state.previous_mute_set_hash = Some(mute_set_hash);
            stale_ids(old_ids, &state.mute_list_subscription_ids)
        };
        for id in stale {
            self.session.unsubscribe(&id).await;
        }

        Ok(())
    }

    // ── Lifecycle ────────────────────────────────────────────────────────

    /// Tears down all discovery subscriptions and resets state.
    /// Used on logout (no remaining accounts) and test cleanup.
    pub(crate) async fn retire_all(&self) {
        let old = std::mem::take(&mut *self.state.write().await);
        for id in old
            .public_subscription_ids
            .into_iter()
            .chain(old.follow_list_subscription_ids)
            .chain(old.mute_list_subscription_ids)
        {
            self.session.unsubscribe(&id).await;
        }
    }

    // ── Queries ──────────────────────────────────────────────────────────

    pub(crate) fn relays(&self) -> &[RelayUrl] {
        &self.config.relays
    }

    pub(crate) fn telemetry(&self) -> broadcast::Receiver<super::observability::RelayTelemetry> {
        self.session.telemetry()
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_events(
        &self,
        filter: Filter,
        timeout: std::time::Duration,
    ) -> Result<Events> {
        if !self
            .session
            .has_any_relay_connected(&self.config.relays)
            .await
        {
            self.start().await?;
        }

        self.session
            .fetch_events_from(&self.config.relays, filter, timeout)
            .await
    }

    pub(crate) async fn watched_user_count(&self) -> usize {
        self.state.read().await.watched_user_count
    }

    pub(crate) async fn has_subscriptions(&self) -> bool {
        let state = self.state.read().await;
        !state.public_subscription_ids.is_empty()
            || !state.follow_list_subscription_ids.is_empty()
            || !state.mute_list_subscription_ids.is_empty()
    }

    pub(crate) async fn has_connected_relay(&self) -> bool {
        self.session
            .has_any_relay_connected(&self.config.relays)
            .await
    }

    pub(crate) async fn snapshot(&self) -> DiscoveryPlaneStateSnapshot {
        let state = self.state.read().await;

        let mut public_subscription_ids = state
            .public_subscription_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        public_subscription_ids.sort_unstable();

        let mut follow_list_subscription_ids = state
            .follow_list_subscription_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        follow_list_subscription_ids.sort_unstable();

        let mut mute_list_subscription_ids = state
            .mute_list_subscription_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        mute_list_subscription_ids.sort_unstable();

        DiscoveryPlaneStateSnapshot {
            watched_user_count: state.watched_user_count,
            follow_list_subscription_count: state.follow_list_account_count,
            mute_list_subscription_count: state.mute_list_account_count,
            public_subscription_ids,
            follow_list_subscription_ids,
            mute_list_subscription_ids,
            session: self.session.snapshot(&self.config.relays).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_for_same_sorted_input() {
        let keys: Vec<PublicKey> = (0..3).map(|_| Keys::generate().public_key()).collect();

        let hash1 = hash_pubkey_slice(&keys);
        let hash2 = hash_pubkey_slice(&keys);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn hash_differs_for_different_input() {
        let keys_a: Vec<PublicKey> = (0..3).map(|_| Keys::generate().public_key()).collect();
        let keys_b: Vec<PublicKey> = (0..3).map(|_| Keys::generate().public_key()).collect();

        assert_ne!(hash_pubkey_slice(&keys_a), hash_pubkey_slice(&keys_b));
    }

    #[test]
    fn hash_of_empty_slice_is_consistent() {
        let empty: Vec<PublicKey> = vec![];
        let hash1 = hash_pubkey_slice(&empty);
        let hash2 = hash_pubkey_slice(&empty);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn batch_count_is_zero_for_empty_set() {
        assert_eq!(batch_count_for(0), 0);
    }

    #[test]
    fn batch_count_is_one_up_to_max_batch_size() {
        assert_eq!(batch_count_for(1), 1);
        assert_eq!(batch_count_for(499), 1);
        assert_eq!(batch_count_for(500), 1);
    }

    #[test]
    fn assign_to_batches_is_deterministic() {
        let users: Vec<PublicKey> = (0..10).map(|_| Keys::generate().public_key()).collect();

        let batches_1 = assign_users_to_batches(&users, 3);
        let batches_2 = assign_users_to_batches(&users, 3);
        assert_eq!(batches_1, batches_2);
    }

    #[test]
    fn adding_one_user_changes_exactly_one_batch_hash() {
        let users: Vec<PublicKey> = (0..20).map(|_| Keys::generate().public_key()).collect();
        let batch_count = 4;

        let batches_before = assign_users_to_batches(&users, batch_count);
        let hashes_before: Vec<u64> = batches_before
            .iter()
            .map(|b| hash_pubkey_slice(b))
            .collect();

        let mut users_after = users.clone();
        users_after.push(Keys::generate().public_key());

        let batches_after = assign_users_to_batches(&users_after, batch_count);
        let hashes_after: Vec<u64> = batches_after.iter().map(|b| hash_pubkey_slice(b)).collect();

        let changed_count = hashes_before
            .iter()
            .zip(&hashes_after)
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(
            changed_count, 1,
            "Adding one user should change exactly one batch"
        );
    }

    #[test]
    fn assign_to_batches_covers_all_users() {
        let users: Vec<PublicKey> = (0..100).map(|_| Keys::generate().public_key()).collect();

        let batches = assign_users_to_batches(&users, 3);
        let total: usize = batches.iter().map(|b| b.len()).sum();
        assert_eq!(total, 100);
    }

    #[test]
    fn batch_count_grows_past_max_batch_size() {
        assert_eq!(batch_count_for(501), 2);
        assert_eq!(batch_count_for(1000), 2);
        assert_eq!(batch_count_for(1001), 3);
    }

    #[test]
    fn stale_ids_returns_old_ids_not_in_new_set() {
        let old = vec![
            SubscriptionId::new("a"),
            SubscriptionId::new("b"),
            SubscriptionId::new("c"),
        ];
        let new = vec![SubscriptionId::new("a"), SubscriptionId::new("c")];

        let stale = stale_ids(old, &new);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].to_string(), "b");
    }

    #[test]
    fn stale_ids_returns_empty_when_sets_match() {
        let old = vec![SubscriptionId::new("a"), SubscriptionId::new("b")];
        let new = vec![SubscriptionId::new("a"), SubscriptionId::new("b")];

        assert!(stale_ids(old, &new).is_empty());
    }

    #[test]
    fn stale_ids_returns_all_when_new_set_is_empty() {
        let old = vec![SubscriptionId::new("a"), SubscriptionId::new("b")];
        let new = vec![];

        let stale = stale_ids(old, &new);
        assert_eq!(stale.len(), 2);
    }

    #[test]
    fn test_curated_default_relays_match_literal_count() {
        let relays = DiscoveryPlaneConfig::curated_default_relays();
        assert_eq!(relays.len(), 6);
        assert_eq!(
            relays[0],
            RelayUrl::parse("wss://index.hzrd149.com").unwrap()
        );
        assert_eq!(relays[5], RelayUrl::parse("wss://nos.lol").unwrap());
    }

    #[test]
    fn test_new_preserves_provided_relays() {
        let relays = vec![RelayUrl::parse("ws://localhost:8080").unwrap()];
        let config = DiscoveryPlaneConfig::new(relays.clone());
        assert_eq!(config.relays, relays);
        assert_eq!(
            config.reconnect_policy,
            RelaySessionReconnectPolicy::Conservative
        );
    }
}
