use dashmap::DashMap;
use nostr_sdk::PublicKey;
use tokio::sync::broadcast;

use super::types::ChatListUpdate;

const BUFFER_SIZE: usize = 100;

pub(crate) struct ChatListStreamManager {
    streams: DashMap<PublicKey, broadcast::Sender<ChatListUpdate>>,
}

impl ChatListStreamManager {
    pub fn new() -> Self {
        Self {
            streams: DashMap::new(),
        }
    }

    pub fn subscribe(&self, account_pubkey: &PublicKey) -> broadcast::Receiver<ChatListUpdate> {
        self.streams
            .entry(*account_pubkey)
            .or_insert_with(|| broadcast::channel(BUFFER_SIZE).0)
            .subscribe()
    }

    pub fn emit(&self, account_pubkey: &PublicKey, update: ChatListUpdate) {
        if let Some(sender) = self.streams.get(account_pubkey)
            && sender.send(update).is_err()
        {
            drop(sender);
            if self
                .streams
                .remove_if(account_pubkey, |_, s| s.receiver_count() == 0)
                .is_some()
            {
                tracing::debug!(
                    target: "whitenoise::chat_list_streaming",
                    "Cleaned up stream for account {} (no active receivers)",
                    account_pubkey.to_hex(),
                );
            }
        }
    }

    pub fn has_subscribers(&self, account_pubkey: &PublicKey) -> bool {
        self.streams
            .get(account_pubkey)
            .map(|sender| sender.receiver_count() > 0)
            .unwrap_or(false)
    }
}

impl Default for ChatListStreamManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use mdk_core::prelude::GroupId;
    use nostr_sdk::Keys;

    use super::*;
    use crate::whitenoise::chat_list::ChatListItem;
    use crate::whitenoise::chat_list_streaming::ChatListUpdateTrigger;
    use crate::whitenoise::group_information::GroupType;

    fn make_test_pubkey() -> PublicKey {
        Keys::generate().public_key()
    }

    fn make_test_item(seed: u8) -> ChatListItem {
        ChatListItem {
            mls_group_id: GroupId::from_slice(&[seed; 32]),
            name: Some(format!("Group {}", seed)),
            group_type: GroupType::Group,
            created_at: Utc::now(),
            group_image_path: Some(PathBuf::from("/test/image.png")),
            group_image_url: None,
            last_message: None,
            pending_confirmation: false,
            welcomer_pubkey: None,
            unread_count: 0,
            pin_order: None,
            dm_peer_pubkey: None,
        }
    }

    fn make_test_update(trigger: ChatListUpdateTrigger, seed: u8) -> ChatListUpdate {
        ChatListUpdate {
            trigger,
            item: make_test_item(seed),
        }
    }

    #[test]
    fn subscribe_creates_new_stream() {
        let manager = ChatListStreamManager::new();
        let pubkey = make_test_pubkey();

        assert!(!manager.streams.contains_key(&pubkey));

        let _rx = manager.subscribe(&pubkey);

        assert!(manager.streams.contains_key(&pubkey));
    }

    #[test]
    fn multiple_subscribes_share_sender() {
        let manager = ChatListStreamManager::new();
        let pubkey = make_test_pubkey();

        let _rx1 = manager.subscribe(&pubkey);
        let _rx2 = manager.subscribe(&pubkey);

        assert_eq!(manager.streams.len(), 1);

        let sender = manager.streams.get(&pubkey).unwrap();
        assert_eq!(sender.receiver_count(), 2);
    }

    #[tokio::test]
    async fn emit_delivers_to_receivers() {
        let manager = ChatListStreamManager::new();
        let pubkey = make_test_pubkey();

        let mut rx = manager.subscribe(&pubkey);

        let update = make_test_update(ChatListUpdateTrigger::NewGroup, 1);
        manager.emit(&pubkey, update);

        let received = rx.try_recv().expect("should receive update");
        assert_eq!(received.trigger, ChatListUpdateTrigger::NewGroup);
    }

    #[test]
    fn emit_without_subscribers_is_noop() {
        let manager = ChatListStreamManager::new();
        let pubkey = make_test_pubkey();

        let update = make_test_update(ChatListUpdateTrigger::NewLastMessage, 2);
        manager.emit(&pubkey, update);

        assert!(!manager.streams.contains_key(&pubkey));
    }

    #[test]
    fn emit_cleans_up_when_all_receivers_dropped() {
        let manager = ChatListStreamManager::new();
        let pubkey = make_test_pubkey();

        let rx = manager.subscribe(&pubkey);
        drop(rx);

        assert!(manager.streams.contains_key(&pubkey));

        let update = make_test_update(ChatListUpdateTrigger::LastMessageDeleted, 3);
        manager.emit(&pubkey, update);

        assert!(!manager.streams.contains_key(&pubkey));
    }

    #[test]
    fn different_accounts_have_separate_streams() {
        let manager = ChatListStreamManager::new();
        let pubkey1 = make_test_pubkey();
        let pubkey2 = make_test_pubkey();

        let _rx1 = manager.subscribe(&pubkey1);
        let _rx2 = manager.subscribe(&pubkey2);

        assert_eq!(manager.streams.len(), 2);
        assert!(manager.streams.contains_key(&pubkey1));
        assert!(manager.streams.contains_key(&pubkey2));
    }

    #[test]
    fn default_creates_empty_manager() {
        let manager = ChatListStreamManager::default();
        assert!(manager.streams.is_empty());
    }

    #[tokio::test]
    async fn emit_delivers_to_all_subscribers() {
        let manager = ChatListStreamManager::new();
        let pubkey = make_test_pubkey();

        let mut rx1 = manager.subscribe(&pubkey);
        let mut rx2 = manager.subscribe(&pubkey);

        let update = make_test_update(ChatListUpdateTrigger::NewGroup, 4);
        manager.emit(&pubkey, update);

        let received1 = rx1.try_recv().expect("rx1 should receive update");
        let received2 = rx2.try_recv().expect("rx2 should receive update");

        assert_eq!(received1.trigger, ChatListUpdateTrigger::NewGroup);
        assert_eq!(received2.trigger, ChatListUpdateTrigger::NewGroup);
    }

    #[test]
    fn has_subscribers_returns_true_when_active() {
        let manager = ChatListStreamManager::new();
        let pubkey = make_test_pubkey();

        assert!(!manager.has_subscribers(&pubkey));

        let _rx = manager.subscribe(&pubkey);

        assert!(manager.has_subscribers(&pubkey));
    }

    #[test]
    fn has_subscribers_returns_false_when_empty() {
        let manager = ChatListStreamManager::new();
        let pubkey = make_test_pubkey();

        assert!(!manager.has_subscribers(&pubkey));
    }

    #[test]
    fn has_subscribers_returns_false_after_all_dropped() {
        let manager = ChatListStreamManager::new();
        let pubkey = make_test_pubkey();

        let rx = manager.subscribe(&pubkey);
        assert!(manager.has_subscribers(&pubkey));

        drop(rx);

        assert!(!manager.has_subscribers(&pubkey));
    }
}
