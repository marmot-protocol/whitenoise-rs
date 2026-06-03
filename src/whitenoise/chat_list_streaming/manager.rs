//! Chat list stream manager for per-account broadcast channels.
//!
//! Thin wrapper around [`BroadcastHub`] keyed by account public key.

use nostr_sdk::PublicKey;
use tokio::sync::broadcast;

use super::types::ChatListUpdate;
use crate::whitenoise::broadcast_hub::BroadcastHub;

pub(crate) struct ChatListStreamManager(BroadcastHub<PublicKey, ChatListUpdate>);

impl ChatListStreamManager {
    pub fn new() -> Self {
        Self(BroadcastHub::new("chat_list"))
    }

    pub fn subscribe(&self, account_pubkey: &PublicKey) -> broadcast::Receiver<ChatListUpdate> {
        self.0.subscribe(account_pubkey)
    }

    pub fn emit(&self, account_pubkey: &PublicKey, update: ChatListUpdate) {
        self.0.emit(account_pubkey, update);
    }

    pub fn has_subscribers(&self, account_pubkey: &PublicKey) -> bool {
        self.0.has_subscribers(account_pubkey)
    }

    pub(crate) fn subscriber_pubkeys(&self) -> Vec<PublicKey> {
        self.0.subscriber_keys()
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

    use crate::marmot::GroupId;
    use chrono::Utc;
    use nostr_sdk::Keys;

    use super::*;
    use crate::whitenoise::chat_list::ChatListItem;
    use crate::whitenoise::chat_list_streaming::ChatListUpdateTrigger;
    use crate::whitenoise::group_information::GroupType;

    #[tokio::test]
    async fn smoke_subscribe_and_emit() {
        let manager = ChatListStreamManager::new();
        let pubkey = Keys::generate().public_key();

        let mut rx = manager.subscribe(&pubkey);

        let update = ChatListUpdate {
            trigger: ChatListUpdateTrigger::NewGroup,
            item: ChatListItem {
                mls_group_id: GroupId::from_slice(&[1u8; 32]),
                name: Some("Test Group".to_string()),
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
                archived_at: None,
                removed_at: None,
                self_removed: false,
                muted_until: None,
            },
        };

        manager.emit(&pubkey, update);

        let received = rx.try_recv().expect("should receive update");
        assert_eq!(received.trigger, ChatListUpdateTrigger::NewGroup);
    }

    #[test]
    fn has_subscribers_lifecycle() {
        let manager = ChatListStreamManager::new();
        let pubkey = Keys::generate().public_key();

        assert!(!manager.has_subscribers(&pubkey));

        let rx = manager.subscribe(&pubkey);
        assert!(manager.has_subscribers(&pubkey));

        drop(rx);
        assert!(!manager.has_subscribers(&pubkey));
    }
}
