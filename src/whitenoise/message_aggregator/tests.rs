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
