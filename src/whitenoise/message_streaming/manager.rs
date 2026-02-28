//! Message stream manager for per-group broadcast channels.
//!
//! Manages broadcast channels for real-time message updates, with lazy stream
//! creation and automatic cleanup when all receivers are dropped.

use dashmap::DashMap;
use mdk_core::prelude::GroupId;
use tokio::sync::broadcast;

use super::types::MessageUpdate;

const BUFFER_SIZE: usize = 100;

pub struct MessageStreamManager {
    streams: DashMap<GroupId, broadcast::Sender<MessageUpdate>>,
}

impl MessageStreamManager {
    pub fn new() -> Self {
        Self {
            streams: DashMap::new(),
        }
    }

    pub fn subscribe(&self, group_id: &GroupId) -> broadcast::Receiver<MessageUpdate> {
        self.streams
            .entry(group_id.clone())
            .or_insert_with(|| broadcast::channel(BUFFER_SIZE).0)
            .subscribe()
    }

    pub fn emit(&self, group_id: &GroupId, update: MessageUpdate) {
        if let Some(sender) = self.streams.get(group_id)
            && sender.send(update).is_err()
        {
            drop(sender);
            // Atomically check and remove to avoid race with concurrent subscribe()
            if self
                .streams
                .remove_if(group_id, |_, s| s.receiver_count() == 0)
                .is_some()
            {
                tracing::debug!(
                    target: "whitenoise::message_streaming",
                    "Cleaned up stream for group {} (no active receivers)",
                    hex::encode(group_id.as_slice()),
                );
            }
        }
    }
}

impl Default for MessageStreamManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::prelude::*;

    use super::*;
    use crate::whitenoise::message_aggregator::{ChatMessage, ReactionSummary};

    fn make_test_group_id(seed: u8) -> GroupId {
        GroupId::from_slice(&[seed; 32])
    }

    fn make_test_message(id: &str) -> ChatMessage {
        ChatMessage {
            id: id.to_string(),
            author: Keys::generate().public_key(),
            content: "test message".to_string(),
            created_at: Timestamp::now(),
            tags: Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: ReactionSummary::default(),
            kind: 9,
            media_attachments: vec![],
            delivery_status: None,
        }
    }

    fn make_test_update(trigger: super::super::UpdateTrigger, id: &str) -> MessageUpdate {
        MessageUpdate {
            trigger,
            message: make_test_message(id),
        }
    }

    #[test]
    fn subscribe_creates_new_stream() {
        let manager = MessageStreamManager::new();
        let group_id = make_test_group_id(1);

        // Stream should not exist before subscribe
        assert!(!manager.streams.contains_key(&group_id));

        let _rx = manager.subscribe(&group_id);

        // Stream should exist after subscribe
        assert!(manager.streams.contains_key(&group_id));
    }

    #[test]
    fn multiple_subscribes_share_sender() {
        let manager = MessageStreamManager::new();
        let group_id = make_test_group_id(2);

        let _rx1 = manager.subscribe(&group_id);
        let _rx2 = manager.subscribe(&group_id);

        // Should still only have one entry
        assert_eq!(manager.streams.len(), 1);

        // Both should receive from same sender (receiver_count should be 2)
        let sender = manager.streams.get(&group_id).unwrap();
        assert_eq!(sender.receiver_count(), 2);
    }

    #[tokio::test]
    async fn emit_delivers_to_receivers() {
        let manager = MessageStreamManager::new();
        let group_id = make_test_group_id(3);

        let mut rx = manager.subscribe(&group_id);

        let update = make_test_update(super::super::UpdateTrigger::NewMessage, "msg1");
        manager.emit(&group_id, update.clone());

        let received = rx.try_recv().expect("should receive update");
        assert_eq!(received.message.id, "msg1");
    }

    #[test]
    fn emit_without_subscribers_is_noop() {
        let manager = MessageStreamManager::new();
        let group_id = make_test_group_id(4);

        // No stream exists, emit should not panic
        let update = make_test_update(super::super::UpdateTrigger::NewMessage, "msg2");
        manager.emit(&group_id, update);

        // No stream should be created
        assert!(!manager.streams.contains_key(&group_id));
    }

    #[test]
    fn emit_cleans_up_when_all_receivers_dropped() {
        let manager = MessageStreamManager::new();
        let group_id = make_test_group_id(5);

        // Subscribe then drop the receiver
        let rx = manager.subscribe(&group_id);
        drop(rx);

        // Stream still exists (cleanup happens on emit)
        assert!(manager.streams.contains_key(&group_id));

        // Emit triggers cleanup since receiver was dropped
        let update = make_test_update(super::super::UpdateTrigger::NewMessage, "msg3");
        manager.emit(&group_id, update);

        // Stream should be cleaned up
        assert!(!manager.streams.contains_key(&group_id));
    }

    #[test]
    fn different_groups_have_separate_streams() {
        let manager = MessageStreamManager::new();
        let group1 = make_test_group_id(6);
        let group2 = make_test_group_id(7);

        let _rx1 = manager.subscribe(&group1);
        let _rx2 = manager.subscribe(&group2);

        assert_eq!(manager.streams.len(), 2);
        assert!(manager.streams.contains_key(&group1));
        assert!(manager.streams.contains_key(&group2));
    }

    #[test]
    fn default_creates_empty_manager() {
        let manager = MessageStreamManager::default();
        assert!(manager.streams.is_empty());
    }

    #[tokio::test]
    async fn emit_delivers_to_all_subscribers() {
        let manager = MessageStreamManager::new();
        let group_id = make_test_group_id(8);

        let mut rx1 = manager.subscribe(&group_id);
        let mut rx2 = manager.subscribe(&group_id);

        let update = make_test_update(super::super::UpdateTrigger::NewMessage, "msg_broadcast");
        manager.emit(&group_id, update);

        let received1 = rx1.try_recv().expect("rx1 should receive update");
        let received2 = rx2.try_recv().expect("rx2 should receive update");

        assert_eq!(received1.message.id, "msg_broadcast");
        assert_eq!(received2.message.id, "msg_broadcast");
    }
}
