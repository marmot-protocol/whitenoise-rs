//! Message stream manager for per-group broadcast channels.
//!
//! Thin wrapper around [`BroadcastHub`] keyed by group ID.

use mdk_core::prelude::GroupId;
use tokio::sync::broadcast;

use super::types::MessageUpdate;
use crate::whitenoise::broadcast_hub::BroadcastHub;

pub struct MessageStreamManager(BroadcastHub<GroupId, MessageUpdate>);

impl MessageStreamManager {
    pub fn new() -> Self {
        Self(BroadcastHub::new("message"))
    }

    pub fn subscribe(&self, group_id: &GroupId) -> broadcast::Receiver<MessageUpdate> {
        self.0.subscribe(group_id)
    }

    pub fn emit(&self, group_id: &GroupId, update: MessageUpdate) {
        self.0.emit(group_id, update);
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
    use crate::whitenoise::message_streaming::UpdateTrigger;

    #[tokio::test]
    async fn smoke_subscribe_and_emit() {
        let manager = MessageStreamManager::new();
        let group_id = GroupId::from_slice(&[1u8; 32]);

        let mut rx = manager.subscribe(&group_id);

        let update = MessageUpdate {
            trigger: UpdateTrigger::NewMessage,
            message: ChatMessage {
                id: "msg1".to_string(),
                author: Keys::generate().public_key(),
                content: "test".to_string(),
                created_at: Timestamp::now(),
                tags: Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                is_blocked: false,
                content_tokens: whitenoise_markdown::Document::default(),
                reactions: ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            },
        };

        manager.emit(&group_id, update);

        let received = rx.try_recv().expect("should receive update");
        assert_eq!(received.message.id, "msg1");
    }
}
