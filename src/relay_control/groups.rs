use std::collections::HashMap;

use nostr_sdk::RelayUrl;
use nostr_sdk::prelude::*;
use tokio::sync::RwLock;

use super::{
    RelayPlane, SubscriptionStream, hash_pubkey_for_subscription_id,
    sessions::{
        RelaySession, RelaySessionAuthPolicy, RelaySessionConfig, RelaySessionReconnectPolicy,
    },
};
use crate::{nostr_manager::Result, types::ProcessableEvent};

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
        }
    }

    pub(crate) async fn update_account(
        &self,
        pubkey: PublicKey,
        relays: &[RelayUrl],
        group_ids: &[String],
        since: Option<Timestamp>,
    ) -> Result<()> {
        let subscription_id =
            SubscriptionId::new(format!("{}_mls_messages", self.pubkey_hash(&pubkey)));

        self.session.unsubscribe(&subscription_id).await;

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

        self.session.ensure_relays_connected(relays).await?;
        self.session
            .subscribe_with_id_to(
                relays,
                subscription_id,
                filter,
                SubscriptionStream::GroupMessages,
                Some(pubkey),
            )
            .await?;

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
        let subscription_id =
            SubscriptionId::new(format!("{}_mls_messages", self.pubkey_hash(pubkey)));
        self.session.unsubscribe(&subscription_id).await;
        self.accounts.write().await.remove(pubkey);
    }

    #[allow(dead_code)]
    pub(crate) async fn has_account(&self, pubkey: &PublicKey) -> bool {
        self.accounts.read().await.contains_key(pubkey)
    }

    fn pubkey_hash(&self, pubkey: &PublicKey) -> String {
        hash_pubkey_for_subscription_id(&self.session_salt, pubkey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
