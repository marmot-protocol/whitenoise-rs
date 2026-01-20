use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::prelude::*;

/// A lightweight representation of a cached event from the aggregated_messages table.
///
/// This type contains the core fields needed for event handling (reactions, deletions, etc.)
/// without the full processing that `ChatMessage` provides.
#[derive(Debug, Clone)]
pub struct AggregatedMessage {
    /// Database row ID
    pub id: i64,
    /// The event ID
    pub event_id: EventId,
    /// The MLS group this event belongs to
    pub mls_group_id: GroupId,
    /// The author of the event
    pub author: PublicKey,
    /// The event content
    pub content: String,
    /// When the event was created
    pub created_at: DateTime<Utc>,
    /// Tags from the event
    pub tags: Tags,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aggregated_message_clone() {
        let event_id =
            EventId::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let pubkey =
            PublicKey::from_hex("0000000000000000000000000000000000000000000000000000000000000002")
                .unwrap();

        let msg = AggregatedMessage {
            id: 1,
            event_id,
            mls_group_id: GroupId::from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]),
            author: pubkey,
            content: "Hello world".to_string(),
            created_at: Utc::now(),
            tags: Tags::new(),
        };

        let cloned = msg.clone();
        assert_eq!(cloned.id, msg.id);
        assert_eq!(cloned.event_id, msg.event_id);
        assert_eq!(cloned.author, msg.author);
        assert_eq!(cloned.content, msg.content);
    }

    #[test]
    fn test_aggregated_message_debug() {
        let event_id =
            EventId::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let pubkey =
            PublicKey::from_hex("0000000000000000000000000000000000000000000000000000000000000002")
                .unwrap();

        let msg = AggregatedMessage {
            id: 42,
            event_id,
            mls_group_id: GroupId::from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]),
            author: pubkey,
            content: "Test content".to_string(),
            created_at: Utc::now(),
            tags: Tags::new(),
        };

        let debug_str = format!("{:?}", msg);
        assert!(debug_str.contains("AggregatedMessage"));
        assert!(debug_str.contains("42")); // id
        assert!(debug_str.contains("Test content"));
    }
}
