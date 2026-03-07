use nostr_sdk::{PublicKey, RelayUrl};

use super::{
    RelayPlane,
    sessions::{RelaySessionAuthPolicy, RelaySessionConfig, RelaySessionReconnectPolicy},
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
            connect_timeout: RelaySessionConfig::new(RelayPlane::AccountInbox).connect_timeout,
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
