//! Group state stream manager for per-account, per-group broadcast channels.
//!
//! Mirrors [`crate::whitenoise::message_streaming::MessageStreamManager`] in
//! shape, but keys streams by `(account_pubkey, group_id)` rather than
//! `group_id` alone. Group state events are first-person — "I left" or "I was
//! removed" — and one account leaving a group does not change another
//! account's membership in the same group, so the stream must isolate them.

use crate::marmot::GroupId;
use dashmap::DashMap;
use nostr_sdk::PublicKey;
use tokio::sync::broadcast;

use super::types::GroupStateUpdate;

const BUFFER_SIZE: usize = 100;

type StreamKey = (PublicKey, GroupId);

pub struct GroupStateStreamManager {
    streams: DashMap<StreamKey, broadcast::Sender<GroupStateUpdate>>,
}

impl GroupStateStreamManager {
    pub fn new() -> Self {
        Self {
            streams: DashMap::new(),
        }
    }

    pub fn subscribe(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
    ) -> broadcast::Receiver<GroupStateUpdate> {
        self.streams
            .entry((*account_pubkey, group_id.clone()))
            .or_insert_with(|| broadcast::channel(BUFFER_SIZE).0)
            .subscribe()
    }

    pub fn emit(&self, account_pubkey: &PublicKey, group_id: &GroupId, update: GroupStateUpdate) {
        let key = (*account_pubkey, group_id.clone());
        if let Some(sender) = self.streams.get(&key)
            && sender.send(update).is_err()
        {
            drop(sender);
            // Atomically check and remove to avoid race with concurrent subscribe()
            if self
                .streams
                .remove_if(&key, |_, s| s.receiver_count() == 0)
                .is_some()
            {
                tracing::debug!(
                    target: "whitenoise::group_state_streaming",
                    "Cleaned up stream for account {} group {} (no active receivers)",
                    account_pubkey.to_hex(),
                    hex::encode(group_id.as_slice()),
                );
            }
        }
    }
}

impl Default for GroupStateStreamManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;

    use super::*;

    fn make_test_pubkey() -> PublicKey {
        Keys::generate().public_key()
    }

    fn make_test_group_id(seed: u8) -> GroupId {
        GroupId::from_slice(&[seed; 32])
    }

    #[test]
    fn subscribe_creates_new_stream() {
        let manager = GroupStateStreamManager::new();
        let pubkey = make_test_pubkey();
        let group_id = make_test_group_id(1);
        let key = (pubkey, group_id.clone());

        assert!(!manager.streams.contains_key(&key));

        let _rx = manager.subscribe(&pubkey, &group_id);

        assert!(manager.streams.contains_key(&key));
    }

    #[test]
    fn multiple_subscribes_share_sender() {
        let manager = GroupStateStreamManager::new();
        let pubkey = make_test_pubkey();
        let group_id = make_test_group_id(2);

        let _rx1 = manager.subscribe(&pubkey, &group_id);
        let _rx2 = manager.subscribe(&pubkey, &group_id);

        assert_eq!(manager.streams.len(), 1);

        let sender = manager.streams.get(&(pubkey, group_id)).unwrap();
        assert_eq!(sender.receiver_count(), 2);
    }

    #[tokio::test]
    async fn emit_delivers_to_receivers() {
        let manager = GroupStateStreamManager::new();
        let pubkey = make_test_pubkey();
        let group_id = make_test_group_id(3);

        let mut rx = manager.subscribe(&pubkey, &group_id);

        manager.emit(&pubkey, &group_id, GroupStateUpdate::LeftGroup);

        let received = rx.try_recv().expect("should receive update");
        assert_eq!(received, GroupStateUpdate::LeftGroup);
    }

    #[test]
    fn emit_without_subscribers_is_noop() {
        let manager = GroupStateStreamManager::new();
        let pubkey = make_test_pubkey();
        let group_id = make_test_group_id(4);

        manager.emit(&pubkey, &group_id, GroupStateUpdate::RemovedFromGroup);

        assert!(manager.streams.is_empty());
    }

    #[test]
    fn emit_cleans_up_when_all_receivers_dropped() {
        let manager = GroupStateStreamManager::new();
        let pubkey = make_test_pubkey();
        let group_id = make_test_group_id(5);

        let rx = manager.subscribe(&pubkey, &group_id);
        drop(rx);

        assert!(manager.streams.contains_key(&(pubkey, group_id.clone())));

        manager.emit(&pubkey, &group_id, GroupStateUpdate::LeftGroup);

        assert!(!manager.streams.contains_key(&(pubkey, group_id)));
    }

    #[test]
    fn different_groups_have_separate_streams() {
        let manager = GroupStateStreamManager::new();
        let pubkey = make_test_pubkey();
        let g1 = make_test_group_id(6);
        let g2 = make_test_group_id(7);

        let _rx1 = manager.subscribe(&pubkey, &g1);
        let _rx2 = manager.subscribe(&pubkey, &g2);

        assert_eq!(manager.streams.len(), 2);
    }

    #[test]
    fn different_accounts_in_same_group_have_separate_streams() {
        let manager = GroupStateStreamManager::new();
        let pubkey_a = make_test_pubkey();
        let pubkey_b = make_test_pubkey();
        let group_id = make_test_group_id(8);

        let _rx_a = manager.subscribe(&pubkey_a, &group_id);
        let _rx_b = manager.subscribe(&pubkey_b, &group_id);

        assert_eq!(manager.streams.len(), 2);
    }

    #[tokio::test]
    async fn emit_to_one_account_does_not_reach_another() {
        let manager = GroupStateStreamManager::new();
        let pubkey_a = make_test_pubkey();
        let pubkey_b = make_test_pubkey();
        let group_id = make_test_group_id(9);

        let mut rx_a = manager.subscribe(&pubkey_a, &group_id);
        let mut rx_b = manager.subscribe(&pubkey_b, &group_id);

        manager.emit(&pubkey_b, &group_id, GroupStateUpdate::LeftGroup);

        assert!(rx_a.try_recv().is_err(), "account A must not see B's event");
        assert_eq!(
            rx_b.try_recv()
                .expect("account B should receive its own event"),
            GroupStateUpdate::LeftGroup,
        );
    }

    #[test]
    fn default_creates_empty_manager() {
        let manager = GroupStateStreamManager::default();
        assert!(manager.streams.is_empty());
    }

    #[tokio::test]
    async fn emit_delivers_to_all_subscribers() {
        let manager = GroupStateStreamManager::new();
        let pubkey = make_test_pubkey();
        let group_id = make_test_group_id(10);

        let mut rx1 = manager.subscribe(&pubkey, &group_id);
        let mut rx2 = manager.subscribe(&pubkey, &group_id);

        manager.emit(&pubkey, &group_id, GroupStateUpdate::RemovedFromGroup);

        assert_eq!(
            rx1.try_recv().expect("rx1 should receive"),
            GroupStateUpdate::RemovedFromGroup,
        );
        assert_eq!(
            rx2.try_recv().expect("rx2 should receive"),
            GroupStateUpdate::RemovedFromGroup,
        );
    }
}
