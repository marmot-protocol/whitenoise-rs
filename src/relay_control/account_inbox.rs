use nostr_sdk::{PublicKey, RelayUrl};

use super::sessions::RelaySessionAuthPolicy;

/// Configuration for the per-account inbox plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountInboxPlaneConfig {
    pub(crate) account_pubkey: PublicKey,
    pub(crate) inbox_relays: Vec<RelayUrl>,
    pub(crate) auth_policy: RelaySessionAuthPolicy,
}

impl AccountInboxPlaneConfig {
    pub(crate) fn new(account_pubkey: PublicKey, inbox_relays: Vec<RelayUrl>) -> Self {
        Self {
            account_pubkey,
            inbox_relays,
            auth_policy: RelaySessionAuthPolicy::Allowed,
        }
    }
}
