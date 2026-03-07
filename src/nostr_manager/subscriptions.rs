//! Legacy `NostrManager` subscription orchestration has been retired in favor of
//! relay-control planes. This module keeps only the test helper needed by
//! remaining compatibility tests.

#[cfg(test)]
use nostr_sdk::PublicKey;
#[cfg(test)]
use sha2::{Digest, Sha256};

#[cfg(test)]
use crate::nostr_manager::NostrManager;

#[cfg(test)]
impl NostrManager {
    /// Create a short hash from a pubkey for use in legacy test subscription IDs.
    pub(crate) fn create_pubkey_hash(&self, pubkey: &PublicKey) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.session_salt());
        hasher.update(pubkey.to_bytes());
        let hash = hasher.finalize();
        format!("{:x}", hash)[..12].to_string()
    }
}
