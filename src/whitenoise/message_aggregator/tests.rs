//! Comprehensive test suite for the message aggregator
//!
//! This module contains integration tests for the complete message aggregation
//! pipeline, focusing on the logic and configuration without requiring
//! complex Message struct creation.

#[cfg(test)]
mod integration_tests {
    use super::super::*;
    use crate::nostr_manager::parser::MockParser;
    use nostr_sdk::prelude::*;

    #[tokio::test]
    async fn test_empty_messages_integration() {
        let aggregator = MessageAggregator::new();
        let parser = MockParser::new();
        let group_id = GroupId::from_slice(&[1; 32]);
        let pubkey = Keys::generate().public_key();

        let result = aggregator
            .aggregate_messages_for_group(&pubkey, &group_id, vec![], &parser, vec![])
            .await
            .unwrap();

        assert!(result.is_empty());
    }

    #[test]
    fn test_aggregator_creation() {
        let aggregator = MessageAggregator::new();

        // Check default configuration
        let config = aggregator.config();
        assert!(config.normalize_emoji);
        assert!(!config.enable_debug_logging);
    }

    #[test]
    fn test_aggregator_with_custom_config() {
        let config = AggregatorConfig {
            normalize_emoji: false,
            enable_debug_logging: true,
        };

        let aggregator = MessageAggregator::with_config(config.clone());

        let retrieved_config = aggregator.config();
        assert!(!retrieved_config.normalize_emoji);
        assert!(retrieved_config.enable_debug_logging);
    }

    #[test]
    fn test_aggregator_default_trait() {
        let aggregator1 = MessageAggregator::new();
        let aggregator2 = MessageAggregator::default();

        // Both should have the same default configuration
        assert_eq!(
            aggregator1.config().normalize_emoji,
            aggregator2.config().normalize_emoji
        );
        assert_eq!(
            aggregator1.config().enable_debug_logging,
            aggregator2.config().enable_debug_logging
        );
    }

    #[test]
    fn test_config_debug_clone() {
        let config = AggregatorConfig {
            normalize_emoji: false,
            enable_debug_logging: true,
        };

        let cloned_config = config.clone();

        assert_eq!(config.normalize_emoji, cloned_config.normalize_emoji);
        assert_eq!(
            config.enable_debug_logging,
            cloned_config.enable_debug_logging
        );

        // Test debug formatting doesn't panic
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("AggregatorConfig"));
    }

    #[test]
    fn test_public_types_exposed() {
        // Ensure all public types can be created and used

        let _config = AggregatorConfig::default();

        let _reaction_summary = ReactionSummary::default();

        let keys = Keys::generate();
        let _emoji_reaction = EmojiReaction {
            emoji: "👍".to_string(),
            count: 1,
            users: vec![keys.public_key()],
        };

        let _user_reaction = UserReaction {
            user: keys.public_key(),
            emoji: "❤️".to_string(),
            created_at: Timestamp::now(),
            reaction_id: EventId::all_zeros(),
        };

        let _stats = GroupStatistics {
            message_count: 10,
            reaction_count: 5,
            deleted_message_count: 1,
            memory_usage_bytes: 1024,
            last_processed_at: Some(Timestamp::now()),
        };
    }

    #[test]
    fn test_processing_error_types() {
        // Test that all error types can be created and formatted

        let _error1 = ProcessingError::InvalidReaction;
        let _error2 = ProcessingError::MissingETag;
        let _error3 = ProcessingError::InvalidTag;
        let _error4 = ProcessingError::InvalidTimestamp;
        let _error5 = ProcessingError::FetchFailed("test".to_string());
        let _error6 = ProcessingError::Internal("test".to_string());

        // Test error formatting
        let error = ProcessingError::Internal("test message".to_string());
        let error_str = format!("{}", error);
        assert!(error_str.contains("test message"));
    }

    #[test]
    fn test_reaction_summary_default() {
        let summary = ReactionSummary::default();

        assert!(summary.by_emoji.is_empty());
        assert!(summary.user_reactions.is_empty());
    }

    #[test]
    fn test_reaction_summary_operations() {
        let mut summary = ReactionSummary::default();
        let user1 = Keys::generate().public_key();

        // Add a user reaction
        summary.user_reactions.push(UserReaction {
            user: user1,
            emoji: "👍".to_string(),
            created_at: Timestamp::now(),
            reaction_id: EventId::all_zeros(),
        });

        // Add an emoji reaction
        summary.by_emoji.insert(
            "👍".to_string(),
            EmojiReaction {
                emoji: "👍".to_string(),
                count: 1,
                users: vec![user1],
            },
        );

        assert_eq!(summary.user_reactions.len(), 1);
        assert_eq!(summary.by_emoji.len(), 1);
        assert!(summary.by_emoji.contains_key("👍"));
    }

    #[tokio::test]
    async fn test_aggregator_with_debug_config() {
        let config = AggregatorConfig {
            normalize_emoji: true,
            enable_debug_logging: true,
        };

        let aggregator = MessageAggregator::with_config(config);
        let parser = MockParser::new();
        let group_id = GroupId::from_slice(&[1; 32]);
        let pubkey = Keys::generate().public_key();

        // This should work even with debug logging enabled
        let result = aggregator
            .aggregate_messages_for_group(&pubkey, &group_id, vec![], &parser, vec![])
            .await
            .unwrap();

        assert!(result.is_empty());
    }

    #[test]
    fn test_group_statistics_serialization() {
        let stats = GroupStatistics {
            message_count: 42,
            reaction_count: 15,
            deleted_message_count: 3,
            memory_usage_bytes: 2048,
            last_processed_at: Some(Timestamp::now()),
        };

        // Test that it can be serialized/deserialized (serde derives)
        let json = serde_json::to_string(&stats).unwrap();
        let deserialized: GroupStatistics = serde_json::from_str(&json).unwrap();

        assert_eq!(stats.message_count, deserialized.message_count);
        assert_eq!(stats.reaction_count, deserialized.reaction_count);
        assert_eq!(
            stats.deleted_message_count,
            deserialized.deleted_message_count
        );
        assert_eq!(stats.memory_usage_bytes, deserialized.memory_usage_bytes);
    }

    #[test]
    fn test_chat_message_serialization() {
        let keys = Keys::generate();
        let chat_message = ChatMessage {
            id: "test_id".to_string(),
            author: keys.public_key(),
            content: "Hello world".to_string(),
            created_at: Timestamp::now(),
            tags: Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: ReactionSummary::default(),
            kind: 9, // Default to MLS group chat
            media_attachments: vec![],
        };

        // Test serialization
        let json = serde_json::to_string(&chat_message).unwrap();
        let deserialized: ChatMessage = serde_json::from_str(&json).unwrap();

        assert_eq!(chat_message.id, deserialized.id);
        assert_eq!(chat_message.content, deserialized.content);
        assert_eq!(chat_message.is_reply, deserialized.is_reply);
        assert_eq!(chat_message.is_deleted, deserialized.is_deleted);
    }

    #[test]
    fn test_types_equality() {
        let keys = Keys::generate();

        let message1 = ChatMessage {
            id: "test".to_string(),
            author: keys.public_key(),
            content: "hello".to_string(),
            created_at: Timestamp::from(1000),
            tags: Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: ReactionSummary::default(),
            kind: 9, // Default to MLS group chat
            media_attachments: vec![],
        };

        let message2 = message1.clone();

        assert_eq!(message1, message2);

        // Test inequality
        let mut message3 = message1.clone();
        message3.content = "different".to_string();

        assert_ne!(message1, message3);
    }

    // Test that the module structure is correct and imports work
    #[test]
    fn test_module_access() {
        use super::super::emoji_utils;
        use super::super::types::*;

        // Test that we can access the modules and types
        let _config = AggregatorConfig::default();

        // Test emoji utils function
        let result = emoji_utils::is_valid_emoji("👍");
        assert!(result);

        let invalid_result = emoji_utils::is_valid_emoji("");
        assert!(!invalid_result);
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
