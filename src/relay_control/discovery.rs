use nostr_sdk::RelayUrl;
use nostr_sdk::prelude::*;
use tokio::sync::RwLock;
use tokio::sync::mpsc::Sender;

use super::{
    RelayPlane, SubscriptionStream,
    sessions::{
        RelaySession, RelaySessionAuthPolicy, RelaySessionConfig, RelaySessionReconnectPolicy,
    },
};
use crate::{nostr_manager::Result, types::ProcessableEvent};

const MAX_USERS_PER_PUBLIC_DISCOVERY_SUBSCRIPTION: usize = 500;

/// Configuration for the long-lived discovery plane.
#[allow(dead_code)]
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

#[allow(dead_code)]
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
            "wss://purplepag.es",
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
}

impl DiscoveryPlane {
    pub(crate) fn new(
        config: DiscoveryPlaneConfig,
        event_sender: Sender<ProcessableEvent>,
    ) -> Self {
        let mut session_config = RelaySessionConfig::new(RelayPlane::Discovery);
        session_config.auth_policy = RelaySessionAuthPolicy::Disabled;
        session_config.reconnect_policy = config.reconnect_policy;

        Self {
            config,
            session: RelaySession::new(session_config, event_sender),
            state: RwLock::new(DiscoveryPlaneState::default()),
        }
    }

    pub(crate) async fn start(&self) -> Result<()> {
        self.session
            .ensure_relays_connected(&self.config.relays)
            .await
    }

    pub(crate) async fn sync(
        &self,
        watched_users: &[PublicKey],
        follow_list_accounts: &[(PublicKey, Option<Timestamp>)],
        public_since: Option<Timestamp>,
    ) -> Result<()> {
        self.start().await?;

        let stale_subscription_ids = {
            let mut state = self.state.write().await;
            let stale_subscription_ids = state
                .public_subscription_ids
                .iter()
                .chain(state.follow_list_subscription_ids.iter())
                .cloned()
                .collect::<Vec<_>>();

            state.public_subscription_ids.clear();
            state.follow_list_subscription_ids.clear();
            stale_subscription_ids
        };

        for subscription_id in stale_subscription_ids {
            self.session.unsubscribe(&subscription_id).await;
        }

        if self.config.relays.is_empty() {
            return Ok(());
        }

        let mut state = self.state.write().await;
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
                )
                .await?;
            state.public_subscription_ids.push(subscription_id);
        }

        let mut follow_list_accounts = follow_list_accounts.to_vec();
        follow_list_accounts.sort_unstable_by_key(|(pubkey, _)| pubkey.to_hex());

        for (index, (account_pubkey, since)) in follow_list_accounts.into_iter().enumerate() {
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
                    SubscriptionStream::DiscoveryFollowLists,
                    Some(account_pubkey),
                )
                .await?;
            state.follow_list_subscription_ids.push(subscription_id);
        }

        Ok(())
    }

    pub(crate) fn relays(&self) -> &[RelayUrl] {
        &self.config.relays
    }

    pub(crate) async fn has_subscriptions(&self) -> bool {
        let state = self.state.read().await;
        !state.public_subscription_ids.is_empty() || !state.follow_list_subscription_ids.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_curated_default_relays_match_literal_count() {
        let relays = DiscoveryPlaneConfig::curated_default_relays();
        assert_eq!(relays.len(), 7);
        assert_eq!(
            relays[0],
            RelayUrl::parse("wss://index.hzrd149.com").unwrap()
        );
        assert_eq!(relays[6], RelayUrl::parse("wss://nos.lol").unwrap());
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
