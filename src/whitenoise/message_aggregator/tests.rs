//! Comprehensive test suite for the message aggregator
//!
//! This module contains integration tests for the complete message aggregation
//! pipeline, focusing on the logic and configuration without requiring
//! complex Message struct creation.

#[cfg(test)]
mod integration_tests {
    use super::super::*;
    use crate::marmot::{Message, MessageState};
    use nostr_sdk::prelude::*;

    #[tokio::test]
    async fn test_empty_messages_integration() {
        let aggregator = MessageAggregator::new();
        let group_id = GroupId::from_slice(&[1; 32]);
        let pubkey = Keys::generate().public_key();

        let result = aggregator
            .aggregate_messages_for_group(&pubkey, &group_id, vec![], vec![])
            .await
            .unwrap();

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn process_single_message_accepts_marmot_message_projection() {
        let aggregator = MessageAggregator::new();
        let keys = Keys::generate();
        let event_id = EventId::from_slice(&[1; 32]).unwrap();
        let wrapper_event_id = EventId::from_slice(&[2; 32]).unwrap();
        let created_at = Timestamp::from(100);
        let processed_at = Timestamp::from(101);
        let event = UnsignedEvent {
            id: Some(event_id),
            pubkey: keys.public_key(),
            created_at,
            kind: Kind::Custom(9),
            tags: Tags::new(),
            content: "hello from marmot".to_string(),
        };
        let message = Message {
            id: event_id,
            pubkey: keys.public_key(),
            kind: Kind::Custom(9),
            mls_group_id: GroupId::from_slice(&[3; 32]),
            created_at,
            processed_at,
            content: "hello from marmot".to_string(),
            tags: Tags::new(),
            event,
            wrapper_event_id,
            epoch: Some(7),
            state: MessageState::Processed,
        };

        let chat_message = aggregator
            .process_single_message(&message, vec![])
            .await
            .unwrap();

        assert_eq!(chat_message.id, event_id.to_string());
        assert_eq!(chat_message.author, keys.public_key());
        assert_eq!(chat_message.content, "hello from marmot");
        assert_eq!(chat_message.kind, 9);
    }

    #[test]
    fn test_module_integration_points() {
        // Test the integration points between modules

        // Test that we can create all the types that would be passed between modules
        let config = AggregatorConfig::default();
        let _aggregator = MessageAggregator::with_config(config);

        // Test tag creation for the pure functions
        let mut tags = Tags::new();
        tags.push(Tag::parse(vec!["e", "test_id"]).unwrap());

        // These pure functions should work with the tags
        let target_ids = super::super::processor::extract_deletion_target_ids(&tags);
        assert_eq!(target_ids.len(), 1);
        assert_eq!(target_ids[0], "test_id");
    }
}
