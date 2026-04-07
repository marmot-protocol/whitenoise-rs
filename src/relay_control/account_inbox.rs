use nostr_sdk::prelude::*;
use nostr_sdk::{PublicKey, RelayUrl};

use super::{
    RelayPlane, hash_pubkey_for_subscription_id,
    sessions::{
        RelaySession, RelaySessionAuthPolicy, RelaySessionConfig, RelaySessionReconnectPolicy,
    },
};
use crate::{
    nostr_manager::{Result, utils::adjust_since_for_giftwrap},
    perf_instrument,
    relay_control::SubscriptionStream,
    types::{AccountInboxPlaneStateSnapshot, ProcessableEvent},
};

/// Configuration for the per-account inbox plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountInboxPlaneConfig {
    pub(crate) account_pubkey: PublicKey,
    pub(crate) inbox_relays: Vec<RelayUrl>,
    pub(crate) auth_policy: RelaySessionAuthPolicy,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
}

impl AccountInboxPlaneConfig {
    pub(crate) fn new(account_pubkey: PublicKey, inbox_relays: Vec<RelayUrl>) -> Self {
        Self {
            account_pubkey,
            inbox_relays,
            auth_policy: RelaySessionAuthPolicy::Allowed,
            reconnect_policy: RelaySessionReconnectPolicy::Conservative,
        }
    }

    pub(crate) fn session_config(&self) -> RelaySessionConfig {
        let mut config = RelaySessionConfig::new(RelayPlane::AccountInbox);
        config.telemetry_account_pubkey = Some(self.account_pubkey);
        config.auth_policy = self.auth_policy;
        config.reconnect_policy = self.reconnect_policy;
        config.min_connected_relays = Some(2);
        config
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AccountInboxPlane {
    session: RelaySession,
    config: AccountInboxPlaneConfig,
    session_salt: [u8; 16],
}

impl AccountInboxPlane {
    pub(crate) fn new(
        config: AccountInboxPlaneConfig,
        event_sender: tokio::sync::mpsc::Sender<ProcessableEvent>,
        session_salt: [u8; 16],
    ) -> Self {
        Self {
            session: RelaySession::new(config.session_config(), event_sender),
            config,
            session_salt,
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn activate(
        &self,
        inbox_relays: &[RelayUrl],
        since: Option<Timestamp>,
        signer: std::sync::Arc<dyn NostrSigner>,
    ) -> Result<()> {
        self.session.set_signer(signer).await;

        self.session.ensure_relays_connected(inbox_relays).await?;
        self.subscribe_giftwrap(inbox_relays, since).await?;

        Ok(())
    }

    #[perf_instrument("relay")]
    pub(crate) async fn deactivate(&self) {
        self.session
            .unsubscribe(&SubscriptionId::new(format!(
                "{}_giftwrap",
                self.pubkey_hash()
            )))
            .await;
        self.session.unset_signer().await;
        self.session.shutdown().await;
    }

    pub(crate) async fn has_connected_relay(&self) -> bool {
        self.session
            .has_any_relay_connected(&self.config.inbox_relays)
            .await
    }

    pub(crate) fn telemetry(
        &self,
    ) -> tokio::sync::broadcast::Receiver<super::observability::RelayTelemetry> {
        self.session.telemetry()
    }

    #[perf_instrument("relay")]
    async fn subscribe_giftwrap(
        &self,
        inbox_relays: &[RelayUrl],
        since: Option<Timestamp>,
    ) -> Result<()> {
        let mut filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(self.config.account_pubkey);

        if let Some(adjusted_since) = adjust_since_for_giftwrap(since) {
            filter = filter.since(adjusted_since);
        }

        self.session
            .subscribe_with_id_to(
                inbox_relays,
                SubscriptionId::new(format!("{}_giftwrap", self.pubkey_hash())),
                filter,
                SubscriptionStream::AccountInboxGiftwraps,
                Some(self.config.account_pubkey),
                &[],
            )
            .await
    }

    fn pubkey_hash(&self) -> String {
        hash_pubkey_for_subscription_id(&self.session_salt, &self.config.account_pubkey)
    }

    pub(crate) async fn snapshot(&self) -> AccountInboxPlaneStateSnapshot {
        AccountInboxPlaneStateSnapshot {
            account_pubkey: self.config.account_pubkey.to_hex(),
            subscription_id: format!("{}_giftwrap", self.pubkey_hash()),
            relay_count: self.config.inbox_relays.len(),
            session: self.session.snapshot(&self.config.inbox_relays).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;

    use super::*;

    #[test]
    fn test_new_sets_account_inbox_defaults() {
        let config = AccountInboxPlaneConfig::new(Keys::generate().public_key(), Vec::new());

        assert_eq!(config.auth_policy, RelaySessionAuthPolicy::Allowed);
        assert_eq!(
            config.reconnect_policy,
            RelaySessionReconnectPolicy::Conservative
        );
        assert_eq!(config.session_config().plane, RelayPlane::AccountInbox);
    }
}
