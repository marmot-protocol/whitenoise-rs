//! Message Aggregation Module
//!
//! This module provides functionality to aggregate raw Nostr MLS messages into structured
//! ChatMessage objects suitable for frontend display. It handles message types including
//! regular chat messages, reactions, deletions, and replies.

pub(crate) mod emoji_utils;
mod processor;
pub(crate) mod reaction_handler;
mod types;
// mod state;  // Future: For Phase 2 stateful implementation

#[cfg(test)]
mod tests;

pub use types::{
    AggregatorConfig, ChatMessage, ChatMessageSummary, EmojiReaction, GroupStatistics,
    ProcessingError, ReactionSummary, UserReaction,
};

use mdk_core::prelude::message_types::Message;
use mdk_core::prelude::*;
use nostr_sdk::PublicKey;

use crate::nostr_manager::parser::Parser;
use crate::whitenoise::media_files::MediaFile;

/// Main message aggregator - designed to be a singleton per Whitenoise instance
/// Group-aware to ensure proper isolation between different group conversations
pub struct MessageAggregator {
    config: AggregatorConfig,
    // Future: state management for stateful mode, keyed by GroupId
    // state: Arc<tokio::sync::RwLock<HashMap<GroupId, AggregatorState>>>,
}

impl MessageAggregator {
    /// Create a new message aggregator with default configuration
    /// This should typically be called only during Whitenoise initialization
    pub fn new() -> Self {
        Self::with_config(AggregatorConfig::default())
    }

    /// Create a new message aggregator with custom configuration
    /// This should typically be called only during Whitenoise initialization
    pub fn with_config(config: AggregatorConfig) -> Self {
        Self {
            config,
            // state: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }

    /// Fetch and aggregate messages for a specific group
    /// This is the main entry point that handles the complete pipeline:
    /// 1. Fetch raw messages from mdk
    /// 2. Parse content tokens for each message
    /// 3. Aggregate reactions, replies, and deletions
    /// 4. Return structured ChatMessage objects
    ///
    /// # Arguments
    /// * `pubkey` - The public key of the user requesting messages (for account access)
    /// * `group_id` - The group to fetch and aggregate messages for
    /// * `messages` - The raw messages to process (from mdk.get_messages())
    /// * `parser` - Reference to the nostr parser for tokenizing message content
    /// * `media_files` - Vector of MediaFile records for linking media to messages
    pub async fn aggregate_messages_for_group(
        &self,
        pubkey: &PublicKey,
        group_id: &GroupId,
        messages: Vec<Message>,
        parser: &dyn Parser,
        media_files: Vec<MediaFile>,
    ) -> Result<Vec<ChatMessage>, ProcessingError> {
        if self.config.enable_debug_logging {
            tracing::debug!(
                "Aggregating {} messages for group {} (user: {})",
                messages.len(),
                hex::encode(group_id.as_slice()),
                pubkey.to_hex()
            );
        }

        // Use the processor module to handle the actual processing
        processor::process_messages(messages, parser, &self.config, media_files).await
    }

    /// Process a single message (kind 9) into a ChatMessage
    /// Used by the event processor to cache messages in real-time as they arrive
    ///
    /// # Arguments
    /// * `message` - The raw message to process (must be kind 9)
    /// * `parser` - Reference to the nostr parser for tokenizing message content
    /// * `media_files` - Vector of MediaFile records for linking media to this message
    pub(crate) async fn process_single_message(
        &self,
        message: &Message,
        parser: &dyn Parser,
        media_files: Vec<MediaFile>,
    ) -> Result<ChatMessage, ProcessingError> {
        // Build media files lookup map
        let media_files_map: std::collections::HashMap<String, MediaFile> = media_files
            .into_iter()
            .filter_map(|mf| {
                if let Some(hash) = &mf.original_file_hash {
                    Some((hex::encode(hash), mf))
                } else {
                    None
                }
            })
            .collect();

        // Process the message using the core processor logic
        processor::process_regular_message(message, parser, &media_files_map).await
    }

    /// Get the current configuration
    pub fn config(&self) -> &AggregatorConfig {
        &self.config
    }

    // Future APIs for stateful implementation - all async for lock management:

    // Update the aggregated state with new messages for a specific group
    // This will be used when we transition to stateful processing
    // pub async fn update_group_with_new_messages(
    //     &self,
    //     group_id: &GroupId,
    //     messages: Vec<Message>
    // ) -> Result<(), ProcessingError>

    // Get the current aggregated messages for a specific group
    // pub async fn get_aggregated_messages_for_group(&self, group_id: &GroupId) -> Option<Vec<ChatMessage>>

    // Persist state for a specific group to disk
    // pub async fn persist_group_state(&self, group_id: &GroupId) -> Result<(), StateError>

    // Load persisted state for a specific group from disk
    // pub async fn load_group_state(&self, group_id: &GroupId) -> Result<(), StateError>

    // Clear all cached/persisted state for a specific group.
    // Useful when a user leaves a group or wants to reset message history.
    // pub async fn clear_group_state(&self, group_id: &GroupId) -> Result<(), StateError>
}

impl Default for MessageAggregator {
    fn default() -> Self {
        Self::new()
    }
}
