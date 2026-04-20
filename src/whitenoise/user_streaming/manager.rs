//! User stream manager for per-user broadcast channels.
//!
//! Thin wrapper around [`BroadcastHub`] keyed by user public key.

use nostr_sdk::PublicKey;
use tokio::sync::broadcast;

use super::types::UserUpdate;
use crate::whitenoise::broadcast_hub::BroadcastHub;

pub(crate) struct UserStreamManager(BroadcastHub<PublicKey, UserUpdate>);

impl UserStreamManager {
    pub fn new() -> Self {
        Self(BroadcastHub::new("user"))
    }

    pub fn subscribe(&self, pubkey: &PublicKey) -> broadcast::Receiver<UserUpdate> {
        self.0.subscribe(pubkey)
    }

    pub fn emit(&self, pubkey: &PublicKey, update: UserUpdate) {
        self.0.emit(pubkey, update);
    }

    #[cfg(test)]
    pub fn has_subscribers(&self, pubkey: &PublicKey) -> bool {
        self.0.has_subscribers(pubkey)
    }
}

impl Default for UserStreamManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use nostr_sdk::{Keys, Metadata};

    use super::*;
    use crate::whitenoise::user_streaming::UserUpdateTrigger;
    use crate::whitenoise::users::User;

    #[tokio::test]
    async fn smoke_subscribe_and_emit() {
        let manager = UserStreamManager::new();
        let pubkey = Keys::generate().public_key();

        let mut rx = manager.subscribe(&pubkey);

        let update = UserUpdate {
            trigger: UserUpdateTrigger::MetadataChanged,
            user: User {
                id: Some(1),
                pubkey,
                metadata: Metadata::new().name("Test User"),
                created_at: Utc::now(),
                metadata_known_at: Some(Utc::now()),
                updated_at: Utc::now(),
            },
        };

        manager.emit(&pubkey, update);

        let received = rx.try_recv().expect("should receive update");
        assert_eq!(received.trigger, UserUpdateTrigger::MetadataChanged);
    }

    #[test]
    fn has_subscribers_lifecycle() {
        let manager = UserStreamManager::new();
        let pubkey = Keys::generate().public_key();

        assert!(!manager.has_subscribers(&pubkey));

        let rx = manager.subscribe(&pubkey);
        assert!(manager.has_subscribers(&pubkey));

        drop(rx);
        assert!(!manager.has_subscribers(&pubkey));
    }
}
