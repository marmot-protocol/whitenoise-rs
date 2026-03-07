use std::collections::HashSet;

use nostr_sdk::prelude::*;
use nostr_sdk::{PublicKey, RelayUrl};
use sha2::{Digest, Sha256};

use super::{
    RelayPlane,
    sessions::{
        RelaySession, RelaySessionAuthPolicy, RelaySessionConfig, RelaySessionReconnectPolicy,
    },
};
use crate::{
    nostr_manager::{Result, utils::adjust_since_for_giftwrap},
    relay_control::SubscriptionStream,
    types::ProcessableEvent,
};

/// Configuration for the per-account inbox plane.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountInboxPlaneConfig {
    pub(crate) account_pubkey: PublicKey,
    pub(crate) inbox_relays: Vec<RelayUrl>,
    pub(crate) auth_policy: RelaySessionAuthPolicy,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
}

#[allow(dead_code)]
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
        RelaySessionConfig {
            plane: RelayPlane::AccountInbox,
            auth_policy: self.auth_policy,
            reconnect_policy: self.reconnect_policy,
            relay_policy: RelaySessionConfig::new(RelayPlane::AccountInbox).relay_policy,
            connect_timeout: RelaySessionConfig::new(RelayPlane::AccountInbox).connect_timeout,
        }
    }
}

#[derive(Debug)]
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

    pub(crate) async fn activate(
        &self,
        inbox_relays: &[RelayUrl],
        since: Option<Timestamp>,
        signer: std::sync::Arc<dyn NostrSigner>,
    ) -> Result<()> {
        self.session.set_signer(signer).await;

        let all_relays: Vec<RelayUrl> = inbox_relays
            .iter()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        self.session.ensure_relays_connected(&all_relays).await?;
        self.subscribe_giftwrap(inbox_relays, since).await?;

        Ok(())
    }

    pub(crate) async fn deactivate(&self) {
        let pubkey_hash = self.pubkey_hash();
        self.session
            .unsubscribe(&SubscriptionId::new(format!(
                "{pubkey_hash}_user_follow_list"
            )))
            .await;
        self.session
            .unsubscribe(&SubscriptionId::new(format!("{pubkey_hash}_giftwrap")))
            .await;
        self.session.unset_signer().await;
    }

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
            )
            .await
    }

    fn pubkey_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.session_salt);
        hasher.update(self.config.account_pubkey.to_bytes());
        format!("{:x}", hasher.finalize())[..12].to_string()
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
