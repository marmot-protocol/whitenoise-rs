use dashmap::DashMap;
use nostr_sdk::PublicKey;
use tokio::sync::broadcast;

use super::types::UserUpdate;

const BUFFER_SIZE: usize = 100;

pub(crate) struct UserStreamManager {
    streams: DashMap<PublicKey, broadcast::Sender<UserUpdate>>,
}

impl UserStreamManager {
    pub fn new() -> Self {
        Self {
            streams: DashMap::new(),
        }
    }

    pub fn subscribe(&self, pubkey: &PublicKey) -> broadcast::Receiver<UserUpdate> {
        self.streams
            .entry(*pubkey)
            .or_insert_with(|| broadcast::channel(BUFFER_SIZE).0)
            .subscribe()
    }

    pub fn emit(&self, pubkey: &PublicKey, update: UserUpdate) {
        if let Some(sender) = self.streams.get(pubkey)
            && sender.send(update).is_err()
        {
            drop(sender);
            if self
                .streams
                .remove_if(pubkey, |_, sender| sender.receiver_count() == 0)
                .is_some()
            {
                tracing::debug!(
                    target: "whitenoise::user_streaming",
                    "Cleaned up stream for user {} (no active receivers)",
                    pubkey.to_hex(),
                );
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn has_subscribers(&self, pubkey: &PublicKey) -> bool {
        self.streams
            .get(pubkey)
            .map(|sender| sender.receiver_count() > 0)
            .unwrap_or(false)
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

    fn make_test_pubkey() -> PublicKey {
        Keys::generate().public_key()
    }

    fn make_test_user(seed: u8) -> User {
        User {
            id: Some(i64::from(seed)),
            pubkey: make_test_pubkey(),
            metadata: Metadata::new().name(format!("User {seed}")),
            created_at: Utc::now(),
            metadata_known_at: Some(Utc::now()),
            updated_at: Utc::now(),
        }
    }

    fn make_test_update(trigger: UserUpdateTrigger, seed: u8) -> UserUpdate {
        UserUpdate {
            trigger,
            user: make_test_user(seed),
        }
    }

    #[test]
    fn subscribe_creates_stream() {
        let manager = UserStreamManager::new();
        let pubkey = make_test_pubkey();

        assert!(!manager.streams.contains_key(&pubkey));

        let _rx = manager.subscribe(&pubkey);

        assert!(manager.streams.contains_key(&pubkey));
    }

    #[test]
    fn multiple_subscribers_share_sender() {
        let manager = UserStreamManager::new();
        let pubkey = make_test_pubkey();

        let _rx1 = manager.subscribe(&pubkey);
        let _rx2 = manager.subscribe(&pubkey);

        assert_eq!(manager.streams.len(), 1);

        let sender = manager.streams.get(&pubkey).unwrap();
        assert_eq!(sender.receiver_count(), 2);
    }

    #[tokio::test]
    async fn emit_reaches_all_subscribers() {
        let manager = UserStreamManager::new();
        let pubkey = make_test_pubkey();

        let mut rx1 = manager.subscribe(&pubkey);
        let mut rx2 = manager.subscribe(&pubkey);

        let update = make_test_update(UserUpdateTrigger::MetadataChanged, 1);
        manager.emit(&pubkey, update);

        let received1 = rx1.try_recv().expect("rx1 should receive update");
        let received2 = rx2.try_recv().expect("rx2 should receive update");

        assert_eq!(received1.trigger, UserUpdateTrigger::MetadataChanged);
        assert_eq!(received2.trigger, UserUpdateTrigger::MetadataChanged);
    }

    #[test]
    fn cleanup_happens_when_receivers_are_dropped() {
        let manager = UserStreamManager::new();
        let pubkey = make_test_pubkey();

        let rx = manager.subscribe(&pubkey);
        drop(rx);

        assert!(manager.streams.contains_key(&pubkey));

        let update = make_test_update(UserUpdateTrigger::LocalMetadataChanged, 2);
        manager.emit(&pubkey, update);

        assert!(!manager.streams.contains_key(&pubkey));
    }

    #[test]
    fn different_pubkeys_get_separate_streams() {
        let manager = UserStreamManager::new();
        let pubkey1 = make_test_pubkey();
        let pubkey2 = make_test_pubkey();

        let _rx1 = manager.subscribe(&pubkey1);
        let _rx2 = manager.subscribe(&pubkey2);

        assert_eq!(manager.streams.len(), 2);
        assert!(manager.streams.contains_key(&pubkey1));
        assert!(manager.streams.contains_key(&pubkey2));
    }

    #[test]
    fn emit_without_subscribers_is_noop() {
        let manager = UserStreamManager::new();
        let pubkey = make_test_pubkey();

        let update = make_test_update(UserUpdateTrigger::UserCreated, 3);
        manager.emit(&pubkey, update);

        assert!(!manager.streams.contains_key(&pubkey));
    }

    #[test]
    fn has_subscribers_returns_true_when_active() {
        let manager = UserStreamManager::new();
        let pubkey = make_test_pubkey();

        assert!(!manager.has_subscribers(&pubkey));

        let _rx = manager.subscribe(&pubkey);

        assert!(manager.has_subscribers(&pubkey));
    }

    #[test]
    fn has_subscribers_returns_false_after_all_dropped() {
        let manager = UserStreamManager::new();
        let pubkey = make_test_pubkey();

        let rx = manager.subscribe(&pubkey);
        assert!(manager.has_subscribers(&pubkey));

        drop(rx);

        assert!(!manager.has_subscribers(&pubkey));
    }
}
