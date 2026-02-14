//! Types for the message streaming feature.
//!
//! These types enable real-time message updates to be pushed to subscribers
//! as events are processed, without requiring polling.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::whitenoise::message_aggregator::ChatMessage;

/// What triggered a message update.
///
/// The accompanying `message` field in [`MessageUpdate`] always contains
/// the complete, up-to-date state of the affected message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateTrigger {
    /// A new message was added to the group.
    NewMessage,

    /// A reaction was added to this message.
    ReactionAdded,

    /// A reaction was removed from this message (via deletion event).
    ReactionRemoved,

    /// The message itself was marked as deleted.
    MessageDeleted,
}

/// Represents a single update to be sent to subscribers.
///
/// Always contains the complete, current state of the affected message.
/// The `message` field is always the BASE message (kind 9), never a reaction
/// or deletion event directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageUpdate {
    /// What triggered this update.
    pub trigger: UpdateTrigger,

    /// The complete, current state of the affected message.
    pub message: ChatMessage,
}

/// Result of subscribing to group messages.
///
/// Contains both the initial snapshot and a receiver for real-time updates.
/// The initial snapshot is already deduplicated with any updates that arrived
/// during the fetch operation, ensuring no race conditions.
pub struct GroupMessageSubscription {
    /// All current messages in the group at subscription time.
    pub initial_messages: Vec<ChatMessage>,

    /// Receiver for real-time updates after the initial snapshot.
    pub updates: broadcast::Receiver<MessageUpdate>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_trigger_derives_clone_and_eq() {
        let trigger = UpdateTrigger::NewMessage;
        let cloned = trigger.clone();
        assert_eq!(trigger, cloned);

        let trigger2 = UpdateTrigger::ReactionAdded;
        assert_ne!(trigger, trigger2);
    }

    #[test]
    fn update_trigger_serialization_roundtrip() {
        let triggers = [
            UpdateTrigger::NewMessage,
            UpdateTrigger::ReactionAdded,
            UpdateTrigger::ReactionRemoved,
            UpdateTrigger::MessageDeleted,
        ];

        for trigger in triggers {
            let serialized = serde_json::to_string(&trigger).expect("serialize");
            let deserialized: UpdateTrigger =
                serde_json::from_str(&serialized).expect("deserialize");
            assert_eq!(trigger, deserialized);
        }
    }

    #[test]
    fn update_trigger_debug_output() {
        // Verify Debug is implemented and produces reasonable output
        let debug_str = format!("{:?}", UpdateTrigger::NewMessage);
        assert!(debug_str.contains("NewMessage"));
    }
}
