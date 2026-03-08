use std::collections::{BTreeSet, HashMap};

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
    types::{GroupPlaneGroupStateSnapshot, GroupPlaneStateSnapshot, ProcessableEvent},
};

/// Configuration for the long-lived group-message plane.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
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
struct GroupAccountState {
    relays: Vec<RelayUrl>,
    group_ids: Vec<String>,
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

        Self {
            session: RelaySession::new(config, event_sender),
            session_salt,
            accounts: RwLock::new(HashMap::new()),
            update_lock: Mutex::new(()),
        }
    }

    pub(crate) async fn update_account(
        &self,
        pubkey: PublicKey,
        relays: &[RelayUrl],
        group_ids: &[String],
        since: Option<Timestamp>,
    ) -> Result<()> {
        let _update_guard = self.update_lock.lock().await;
        let subscription_id =
            SubscriptionId::new(format!("{}_mls_messages", self.pubkey_hash(&pubkey)));

        if self.accounts.read().await.contains_key(&pubkey) {
            self.session.unsubscribe(&subscription_id).await;
        }

        if group_ids.is_empty() || relays.is_empty() {
            // Keep the account in the map with empty state so health checks
            // (has_account_subscriptions) can distinguish "activated with no
            // groups" from "never activated at all".
            self.accounts.write().await.insert(
                pubkey,
                GroupAccountState {
                    relays: vec![],
                    group_ids: vec![],
                },
            );
            return Ok(());
        }

        let mut filter = Filter::new()
            .kind(Kind::MlsGroupMessage)
            .custom_tags(SingleLetterTag::lowercase(Alphabet::H), group_ids);

        if let Some(since) = since {
            filter = filter.since(since);
        }

        // On any failure below, remove the accounts entry so that
        // has_active_subscription() returns false and recovery is triggered.
        // (The unsubscribe above already tore down the previous subscription,
        // so leaving stale relay info in the map would make the health check
        // falsely report healthy with no live subscription.)
        if let Err(e) = self.session.ensure_relays_connected(relays).await {
            self.accounts.write().await.remove(&pubkey);
            return Err(e);
        }
        if let Err(e) = self
            .session
            .subscribe_with_id_to(
                relays,
                subscription_id,
                filter,
                SubscriptionStream::GroupMessages,
                Some(pubkey),
            )
            .await
        {
            self.accounts.write().await.remove(&pubkey);
            return Err(e);
        }

        self.accounts.write().await.insert(
            pubkey,
            GroupAccountState {
                relays: relays.to_vec(),
                group_ids: group_ids.to_vec(),
            },
        );

        Ok(())
    }

    pub(crate) async fn remove_account(&self, pubkey: &PublicKey) {
        let _update_guard = self.update_lock.lock().await;
        let subscription_id =
            SubscriptionId::new(format!("{}_mls_messages", self.pubkey_hash(pubkey)));
        if self.accounts.write().await.remove(pubkey).is_some() {
            self.session.unsubscribe(&subscription_id).await;
        }
    }

    #[allow(dead_code)]
    pub(crate) async fn has_account(&self, pubkey: &PublicKey) -> bool {
        self.accounts.read().await.contains_key(pubkey)
    }

    /// Returns `true` if the account is active and its group plane is healthy.
    ///
    /// - Accounts with no groups: entry present in the map is sufficient (empty
    ///   `relays` is the canonical "activated, nothing to subscribe to" state).
    /// - Accounts with groups: at least one group relay must be connected.
    pub(crate) async fn has_active_subscription(&self, pubkey: &PublicKey) -> bool {
        let state = self.accounts.read().await;
        match state.get(pubkey) {
            None => false,
            Some(account_state) => {
                if account_state.relays.is_empty() {
                    true
                } else {
                    self.session
                        .has_any_relay_connected(&account_state.relays)
                        .await
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
            for relay_url in &account_state.relays {
                distinct_group_relays.insert(relay_url.clone());
            }

            let relay_urls = account_state
                .relays
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();

            for group_id in &account_state.group_ids {
                groups.push(GroupPlaneGroupStateSnapshot {
                    account_pubkey: pubkey.to_hex(),
                    group_id: group_id.clone(),
                    subscription_id: format!("{}_mls_messages", self.pubkey_hash(pubkey)),
                    relay_count: relay_urls.len(),
                    relay_urls: relay_urls.clone(),
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
                tokio::spawn(async move { plane.update_account(pubkey, &[], &[], None).await })
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
        assert!(account_state.relays.is_empty());
        assert!(account_state.group_ids.is_empty());
    }
}
