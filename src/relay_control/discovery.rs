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
    public_subscription_ids: Vec<SubscriptionId>,
    follow_list_subscription_ids: Vec<SubscriptionId>,
    watched_user_count: usize,
    follow_list_account_count: usize,
    public_since: Option<Timestamp>,
    last_sync_at: Option<Timestamp>,
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

    #[perf_instrument("relay")]
    pub(crate) async fn sync(
        &self,
        watched_users: &[PublicKey],
        follow_list_accounts: &[(PublicKey, Option<Timestamp>)],
        public_since: Option<Timestamp>,
    ) -> Result<()> {
        // Short-circuit: no relays configured — retire any existing subscriptions.
        if self.config.relays.is_empty() {
            let stale = {
                let mut state = self.state.write().await;
                let stale = state
                    .public_subscription_ids
                    .iter()
                    .chain(state.follow_list_subscription_ids.iter())
                    .cloned()
                    .collect::<Vec<_>>();
                state.public_subscription_ids.clear();
                state.follow_list_subscription_ids.clear();
                state.watched_user_count = 0;
                state.follow_list_account_count = 0;
                state.public_since = public_since;
                state.last_sync_at = Some(Timestamp::now());
                stale
            };
            for id in stale {
                self.session.unsubscribe(&id).await;
            }
            return Ok(());
        }

        if !self
            .session
            .has_any_relay_connected(&self.config.relays)
            .await
        {
            self.start().await?;
        }

        // Discovery sync is a full replace. We build ALL new subscriptions
        // before touching existing state so that a mid-sync failure leaves the
        // old subscriptions intact (no coverage gap). Subscription IDs are
        // positional and stable across syncs (e.g. `discovery_user_data_0`),
        // so re-subscribing with the same ID replaces the filter in-place on
        // the relay rather than creating a duplicate stream.
        let mut new_public_ids: Vec<SubscriptionId> = Vec::new();
        let mut new_follow_ids: Vec<SubscriptionId> = Vec::new();

        let mut watched_users = watched_users.to_vec();
        watched_users.sort_unstable_by_key(|pubkey| pubkey.to_hex());
        watched_users.dedup();

        for (batch_index, authors) in watched_users
            .chunks(MAX_USERS_PER_PUBLIC_DISCOVERY_SUBSCRIPTION)
            .enumerate()
        {
            let mut filter = Filter::new().authors(authors.to_vec()).kinds([
                Kind::Metadata,
                Kind::RelayList,
                Kind::InboxRelays,
                Kind::MlsKeyPackageRelays,
            ]);
            if let Some(public_since) = public_since {
                filter = filter.since(public_since);
            }

            let subscription_id = SubscriptionId::new(format!("discovery_user_data_{batch_index}"));
            self.session
                .subscribe_with_id_to(
                    &self.config.relays,
                    subscription_id.clone(),
                    filter,
                    // Metadata and relay-list discovery route through the global
                    // event processor. Follow lists stay separate because they are
                    // account-scoped and feed account follow processing.
                    SubscriptionStream::DiscoveryUserData,
                    None,
                    &[],
                )
                .await?;
            new_public_ids.push(subscription_id);
        }

        let mut follow_list_accounts = follow_list_accounts.to_vec();
        follow_list_accounts.sort_unstable_by_key(|(pubkey, _)| pubkey.to_hex());

        // Deduplicate by pubkey and keep the earliest replay anchor. `None`
        // is treated as earliest because it means "full replay required".
        let mut deduped_follow_list_accounts: Vec<(PublicKey, Option<Timestamp>)> =
            Vec::with_capacity(follow_list_accounts.len());
        for (pubkey, since) in follow_list_accounts {
            match deduped_follow_list_accounts.last_mut() {
                Some((last_pubkey, last_since)) if *last_pubkey == pubkey => {
                    *last_since = match (*last_since, since) {
                        (None, _) | (_, None) => None,
                        (Some(existing), Some(candidate)) => Some(existing.min(candidate)),
                    };
                }
                _ => deduped_follow_list_accounts.push((pubkey, since)),
            }
        }

        let follow_list_account_count = deduped_follow_list_accounts.len();

        for (index, (account_pubkey, since)) in deduped_follow_list_accounts.into_iter().enumerate()
        {
            let mut filter = Filter::new().kind(Kind::ContactList).author(account_pubkey);
            if let Some(since) = since {
                filter = filter.since(since);
            }

            let subscription_id = SubscriptionId::new(format!("discovery_follow_{index}"));
            self.session
                .subscribe_with_id_to(
                    &self.config.relays,
                    subscription_id.clone(),
                    filter,
                    // Discovery follow lists are account-scoped: they run on the
                    // discovery plane, but still route through account event
                    // processing and therefore must carry the owning account pubkey.
                    SubscriptionStream::DiscoveryFollowLists,
                    Some(account_pubkey),
                    &[],
                )
                .await?;
            new_follow_ids.push(subscription_id);
        }

        // All new subscriptions established. Atomically swap state and retire
        // only the IDs that were NOT reused in the new set. IDs that appear in
        // both the old and new sets were replaced in-place above and must not
        // be unsubscribed.
        let new_id_strings: std::collections::HashSet<String> = new_public_ids
            .iter()
            .chain(new_follow_ids.iter())
            .map(|id| id.to_string())
            .collect();

        let stale_to_remove = {
            let mut state = self.state.write().await;
            let old_public = std::mem::replace(&mut state.public_subscription_ids, new_public_ids);
            let old_follow =
                std::mem::replace(&mut state.follow_list_subscription_ids, new_follow_ids);
            state.watched_user_count = watched_users.len();
            state.follow_list_account_count = follow_list_account_count;
            state.public_since = public_since;
            state.last_sync_at = Some(Timestamp::now());
            old_public
                .into_iter()
                .chain(old_follow.into_iter())
                .filter(|id| !new_id_strings.contains(&id.to_string()))
                .collect::<Vec<_>>()
        };

        for id in stale_to_remove {
            self.session.unsubscribe(&id).await;
        }

        Ok(())
    }

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

    pub(crate) async fn has_subscriptions(&self) -> bool {
        let state = self.state.read().await;
        !state.public_subscription_ids.is_empty() || !state.follow_list_subscription_ids.is_empty()
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

        DiscoveryPlaneStateSnapshot {
            watched_user_count: state.watched_user_count,
            follow_list_subscription_count: state.follow_list_account_count,
            public_subscription_ids,
            follow_list_subscription_ids,
            session: self.session.snapshot(&self.config.relays).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
