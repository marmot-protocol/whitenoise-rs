use std::collections::{BTreeMap, BTreeSet, HashMap};

use nostr_sdk::RelayUrl;
use nostr_sdk::prelude::*;
use tokio::sync::{Mutex, RwLock, broadcast};

use super::{
    RelayPlane, SubscriptionStream, hash_pubkey_for_subscription_id,
    sessions::{
        RelaySession, RelaySessionAuthPolicy, RelaySessionConfig, RelaySessionReconnectPolicy,
    },
};
use crate::{
    nostr_manager::Result,
    perf_instrument,
    types::{GroupPlaneGroupStateSnapshot, GroupPlaneStateSnapshot, ProcessableEvent},
};

/// Configuration for the long-lived group-message plane.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct GroupPlaneConfig {
    pub(crate) relays: Vec<RelayUrl>,
    pub(crate) group_ids: Vec<String>,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
}

impl Default for GroupPlaneConfig {
    fn default() -> Self {
        Self {
            relays: Vec::new(),
            group_ids: Vec::new(),
            reconnect_policy: RelaySessionReconnectPolicy::FreshnessBiased,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupSubscriptionSpec {
    pub(crate) group_id: String,
    pub(crate) relays: Vec<RelayUrl>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupRelaySetSubscription {
    subscription_index: usize,
    relays: Vec<RelayUrl>,
    group_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct GroupAccountState {
    groups: Vec<GroupSubscriptionSpec>,
    subscriptions: Vec<GroupRelaySetSubscription>,
}

#[derive(Debug)]
pub(crate) struct GroupPlane {
    session: RelaySession,
    session_salt: [u8; 16],
    accounts: RwLock<HashMap<PublicKey, GroupAccountState>>,
    update_lock: Mutex<()>,
}

impl GroupPlane {
    pub(crate) fn new(
        event_sender: tokio::sync::mpsc::Sender<ProcessableEvent>,
        session_salt: [u8; 16],
    ) -> Self {
        let mut config = RelaySessionConfig::new(RelayPlane::Group);
        config.auth_policy = RelaySessionAuthPolicy::Disabled;
        config.reconnect_policy = RelaySessionReconnectPolicy::FreshnessBiased;
        config.min_connected_relays = Some(2);

        Self {
            session: RelaySession::new(config, event_sender),
            session_salt,
            accounts: RwLock::new(HashMap::new()),
            update_lock: Mutex::new(()),
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn update_account(
        &self,
        pubkey: PublicKey,
        group_specs: &[GroupSubscriptionSpec],
        since: Option<Timestamp>,
    ) -> Result<()> {
        let _update_guard = self.update_lock.lock().await;

        if let Some(previous_state) = self.accounts.read().await.get(&pubkey).cloned() {
            for subscription in previous_state.subscriptions {
                self.session
                    .unsubscribe(&self.subscription_id(&pubkey, subscription.subscription_index))
                    .await;
            }
        }

        if group_specs.is_empty() {
            // Keep the account in the map with empty state so health checks
            // (has_account_subscriptions) can distinguish "activated with no
            // groups" from "never activated at all".
            self.accounts
                .write()
                .await
                .insert(pubkey, GroupAccountState::default());
            return Ok(());
        }

        let normalized_groups = Self::normalize_group_specs(group_specs);
        let subscriptions = Self::build_relay_set_subscriptions(&normalized_groups);

        // On any failure below, remove the accounts entry so that
        // has_active_subscription() returns false and recovery is triggered.
        // (The unsubscribe above already tore down the previous subscription,
        // so leaving stale relay info in the map would make the health check
        // falsely report healthy with no live subscription.)
        let mut installed_subscription_indices = Vec::new();
        for subscription in &subscriptions {
            if subscription.relays.is_empty() || subscription.group_ids.is_empty() {
                continue;
            }

            let mut filter = Filter::new().kind(Kind::MlsGroupMessage).custom_tags(
                SingleLetterTag::lowercase(Alphabet::H),
                subscription.group_ids.iter(),
            );

            if let Some(since) = since {
                filter = filter.since(since);
            }

            if let Err(e) = self
                .session
                .ensure_relays_connected(&subscription.relays)
                .await
            {
                self.unsubscribe_indices(&pubkey, &installed_subscription_indices)
                    .await;
                self.accounts.write().await.remove(&pubkey);
                return Err(e);
            }
            if let Err(e) = self
                .session
                .subscribe_with_id_to(
                    &subscription.relays,
                    self.subscription_id(&pubkey, subscription.subscription_index),
                    filter,
                    SubscriptionStream::GroupMessages,
                    Some(pubkey),
                    &subscription.group_ids,
                )
                .await
            {
                self.unsubscribe_indices(&pubkey, &installed_subscription_indices)
                    .await;
                self.accounts.write().await.remove(&pubkey);
                return Err(e);
            }
            installed_subscription_indices.push(subscription.subscription_index);
        }

        self.accounts.write().await.insert(
            pubkey,
            GroupAccountState {
                groups: normalized_groups,
                subscriptions,
            },
        );

        Ok(())
    }

    #[perf_instrument("relay")]
    pub(crate) async fn remove_account(&self, pubkey: &PublicKey) {
        let _update_guard = self.update_lock.lock().await;
        if let Some(state) = self.accounts.write().await.remove(pubkey) {
            let subscription_indices = state
                .subscriptions
                .into_iter()
                .map(|subscription| subscription.subscription_index)
                .collect::<Vec<_>>();
            self.unsubscribe_indices(pubkey, &subscription_indices)
                .await;
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn account_state(
        &self,
        pubkey: &PublicKey,
    ) -> Option<Vec<GroupSubscriptionSpec>> {
        self.accounts
            .read()
            .await
            .get(pubkey)
            .map(|state| state.groups.clone())
    }

    #[allow(dead_code)]
    #[perf_instrument("relay")]
    pub(crate) async fn has_account(&self, pubkey: &PublicKey) -> bool {
        self.accounts.read().await.contains_key(pubkey)
    }

    /// Returns `true` if the account is active and its group plane is healthy.
    ///
    /// - Accounts with no groups: entry present in the map is sufficient (empty
    ///   `relays` is the canonical "activated, nothing to subscribe to" state).
    /// - Accounts with groups: at least one group relay must be connected.
    #[perf_instrument("relay")]
    pub(crate) async fn has_active_subscription(&self, pubkey: &PublicKey) -> bool {
        let state = self.accounts.read().await;
        match state.get(pubkey) {
            None => false,
            Some(account_state) => {
                let relays = Self::distinct_relays(account_state.groups.iter());
                if relays.is_empty() {
                    true
                } else {
                    self.session.has_any_relay_connected(&relays).await
                }
            }
        }
    }

    pub(crate) fn telemetry(&self) -> broadcast::Receiver<super::observability::RelayTelemetry> {
        self.session.telemetry()
    }

    fn pubkey_hash(&self, pubkey: &PublicKey) -> String {
        hash_pubkey_for_subscription_id(&self.session_salt, pubkey)
    }

    fn subscription_id(&self, pubkey: &PublicKey, subscription_index: usize) -> SubscriptionId {
        SubscriptionId::new(format!(
            "{}_mls_messages_{}",
            self.pubkey_hash(pubkey),
            subscription_index
        ))
    }

    fn normalize_group_specs(group_specs: &[GroupSubscriptionSpec]) -> Vec<GroupSubscriptionSpec> {
        let mut merged: HashMap<String, BTreeSet<RelayUrl>> = HashMap::new();

        for spec in group_specs {
            let entry = merged.entry(spec.group_id.clone()).or_default();
            entry.extend(spec.relays.iter().cloned());
        }

        let mut normalized = merged
            .into_iter()
            .map(|(group_id, relays)| GroupSubscriptionSpec {
                group_id,
                relays: relays.into_iter().collect(),
            })
            .collect::<Vec<_>>();
        normalized.sort_unstable_by(|left, right| left.group_id.cmp(&right.group_id));
        normalized
    }

    fn distinct_relays<'a, I>(groups: I) -> Vec<RelayUrl>
    where
        I: IntoIterator<Item = &'a GroupSubscriptionSpec>,
    {
        let mut relays = BTreeSet::new();
        for group in groups {
            relays.extend(group.relays.iter().cloned());
        }
        relays.into_iter().collect()
    }

    fn build_relay_set_subscriptions(
        groups: &[GroupSubscriptionSpec],
    ) -> Vec<GroupRelaySetSubscription> {
        let mut relay_buckets: BTreeMap<Vec<RelayUrl>, Vec<String>> = BTreeMap::new();

        for group in groups {
            relay_buckets
                .entry(group.relays.clone())
                .or_default()
                .push(group.group_id.clone());
        }

        relay_buckets
            .into_iter()
            .enumerate()
            .filter_map(|(subscription_index, (relays, group_ids))| {
                if relays.is_empty() || group_ids.is_empty() {
                    None
                } else {
                    Some(GroupRelaySetSubscription {
                        subscription_index,
                        relays,
                        group_ids,
                    })
                }
            })
            .collect()
    }

    #[perf_instrument("relay")]
    async fn unsubscribe_indices(&self, pubkey: &PublicKey, subscription_indices: &[usize]) {
        for subscription_index in subscription_indices {
            self.session
                .unsubscribe(&self.subscription_id(pubkey, *subscription_index))
                .await;
        }
    }

    pub(crate) async fn snapshot(&self) -> GroupPlaneStateSnapshot {
        let account_states = self
            .accounts
            .read()
            .await
            .iter()
            .map(|(pubkey, state)| (*pubkey, state.clone()))
            .collect::<Vec<_>>();

        let mut distinct_group_relays = BTreeSet::new();
        let mut groups = Vec::new();

        for (pubkey, account_state) in &account_states {
            let group_subscription_ids = account_state
                .subscriptions
                .iter()
                .flat_map(|subscription| {
                    subscription.group_ids.iter().map(move |group_id| {
                        (
                            group_id.clone(),
                            self.subscription_id(pubkey, subscription.subscription_index)
                                .to_string(),
                        )
                    })
                })
                .collect::<HashMap<_, _>>();

            for group in &account_state.groups {
                for relay_url in &group.relays {
                    distinct_group_relays.insert(relay_url.clone());
                }

                groups.push(GroupPlaneGroupStateSnapshot {
                    account_pubkey: pubkey.to_hex(),
                    group_id: group.group_id.clone(),
                    subscription_id: group_subscription_ids
                        .get(&group.group_id)
                        .cloned()
                        .unwrap_or_default(),
                    relay_count: group.relays.len(),
                    relay_urls: group.relays.iter().map(ToString::to_string).collect(),
                });
            }
        }

        groups.sort_unstable_by(|left, right| {
            left.account_pubkey
                .cmp(&right.account_pubkey)
                .then(left.group_id.cmp(&right.group_id))
        });
        let known_relays = distinct_group_relays.into_iter().collect::<Vec<_>>();

        GroupPlaneStateSnapshot {
            group_count: groups.len(),
            groups,
            session: self.session.snapshot(&known_relays).await,
        }
    }

    #[cfg(feature = "integration-tests")]
    #[perf_instrument("relay")]
    pub(crate) async fn reset(&self) {
        let pubkeys = self
            .accounts
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();

        for pubkey in pubkeys {
            self.remove_account(&pubkey).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use futures::future::join_all;
    use tokio::sync::mpsc;

    #[test]
    fn test_default_uses_explicit_group_reconnect_policy() {
        let config = GroupPlaneConfig::default();
        assert_eq!(config.relays, Vec::<RelayUrl>::new());
        assert_eq!(config.group_ids, Vec::<String>::new());
        assert_eq!(
            config.reconnect_policy,
            RelaySessionReconnectPolicy::FreshnessBiased
        );
    }

    #[tokio::test]
    async fn test_group_plane_hash_is_stable_for_account() {
        let (sender, _) = mpsc::channel(8);
        let plane = GroupPlane::new(sender, [7; 16]);
        let pubkey = Keys::generate().public_key();

        assert_eq!(plane.pubkey_hash(&pubkey), plane.pubkey_hash(&pubkey));
    }

    #[tokio::test]
    async fn test_concurrent_update_same_account_keeps_single_state() {
        let (sender, _) = mpsc::channel(8);
        let plane = Arc::new(GroupPlane::new(sender, [42; 16]));
        let pubkey = Keys::generate().public_key();

        // Use the empty-state path so the test stays unit-scoped and does not
        // depend on live relays. The concurrency invariant we care about is
        // that same-account updates serialize cleanly and leave one stable
        // account entry behind rather than racing unsubscribe/resubscribe.
        let update_tasks = (0..10)
            .map(|_| {
                let plane = plane.clone();
                tokio::spawn(async move { plane.update_account(pubkey, &[], None).await })
            })
            .collect::<Vec<_>>();

        let results = join_all(update_tasks).await;
        for result in results {
            result.unwrap().unwrap();
        }

        assert!(plane.has_account(&pubkey).await);
        assert!(plane.has_active_subscription(&pubkey).await);

        let accounts = plane.accounts.read().await;
        assert_eq!(accounts.len(), 1);

        let account_state = accounts.get(&pubkey).unwrap();
        assert!(account_state.groups.is_empty());
    }

    #[tokio::test]
    async fn test_snapshot_keeps_per_group_relays_separate() {
        let (sender, _) = mpsc::channel(8);
        let plane = GroupPlane::new(sender, [9; 16]);
        let pubkey = Keys::generate().public_key();
        let relay_a = RelayUrl::parse("wss://relay-a.example.com").unwrap();
        let relay_b = RelayUrl::parse("wss://relay-b.example.com").unwrap();
        let relay_c = RelayUrl::parse("wss://relay-c.example.com").unwrap();

        let groups = vec![
            GroupSubscriptionSpec {
                group_id: "group-a".to_string(),
                relays: vec![relay_a.clone(), relay_b.clone()],
            },
            GroupSubscriptionSpec {
                group_id: "group-b".to_string(),
                relays: vec![relay_c.clone()],
            },
        ];
        let subscriptions = GroupPlane::build_relay_set_subscriptions(&groups);

        plane.accounts.write().await.insert(
            pubkey,
            GroupAccountState {
                groups,
                subscriptions,
            },
        );

        let snapshot = plane.snapshot().await;
        assert_eq!(snapshot.group_count, 2);

        let group_a = snapshot
            .groups
            .iter()
            .find(|group| group.group_id == "group-a")
            .unwrap();
        assert_eq!(
            group_a.relay_urls,
            vec![relay_a.to_string(), relay_b.to_string()]
        );

        let group_b = snapshot
            .groups
            .iter()
            .find(|group| group.group_id == "group-b")
            .unwrap();
        assert_eq!(group_b.relay_urls, vec![relay_c.to_string()]);
    }
}
