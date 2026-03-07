//! Legacy `NostrManager` subscription orchestration has been retired in favor of
//! relay-control planes. This module keeps only the test helper needed by
//! remaining compatibility tests.

#[cfg(test)]
use nostr_sdk::PublicKey;

#[cfg(test)]
use crate::{nostr_manager::NostrManager, relay_control::hash_pubkey_for_subscription_id};

#[cfg(test)]
impl NostrManager {
    /// Create a short hash from a pubkey for use in legacy test subscription IDs.
    pub(crate) fn create_pubkey_hash(&self, pubkey: &PublicKey) -> String {
        hash_pubkey_for_subscription_id(self.session_salt(), pubkey)
    }
}
