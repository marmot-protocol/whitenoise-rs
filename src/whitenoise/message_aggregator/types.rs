use std::collections::HashMap;

use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::nostr_manager::parser::SerializableToken;
use crate::whitenoise::media_files::MediaFile;

/// Represents an aggregated chat message ready for frontend display
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    /// Unique identifier of the message
    pub id: String,

    /// Public key of the message author
    pub author: PublicKey,

    /// Message content (empty if deleted)
    pub content: String,

    /// Timestamp when the message was created
    pub created_at: Timestamp,

    /// Tags from the original Nostr event
    pub tags: Tags,

    /// Whether this message is a reply to another message
    pub is_reply: bool,

    /// ID of the message this is replying to (if is_reply is true)
    pub reply_to_id: Option<String>,

    /// Whether this message has been deleted
    pub is_deleted: bool,

    /// Parsed tokens from the message content (mentions, hashtags, etc.)
    pub content_tokens: Vec<SerializableToken>,

    /// Aggregated reactions on this message
    pub reactions: ReactionSummary,

    /// The kind of the original Nostr event
    pub kind: u16,

    /// Media files attached to this message
    pub media_attachments: Vec<MediaFile>,
}

/// Lightweight message summary for previews (chat list).
///
/// This is a subset of `ChatMessage` optimized for display contexts where full
/// message data isn't needed. Uses `DateTime<Utc>` for database consistency.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessageSummary {
    /// The MLS group this message belongs to
    pub mls_group_id: GroupId,

    /// Public key of the message author
    pub author: PublicKey,

    /// Author's display name (populated by caller after user lookup, None initially)
    pub author_display_name: Option<String>,

    /// Message content preview
    pub content: String,

    /// When the message was sent
    pub created_at: DateTime<Utc>,

    /// Number of media attachments
    pub media_attachment_count: usize,
}

/// Summary of reactions on a message
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ReactionSummary {
    /// Map of emoji to reaction details
    pub by_emoji: HashMap<String, EmojiReaction>,

    /// List of all users who have reacted and with what
    pub user_reactions: Vec<UserReaction>,
}

/// Details for a specific emoji reaction
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EmojiReaction {
    /// The emoji or reaction symbol
    pub emoji: String,

    /// Count of users who used this reaction
    pub count: usize,

    /// List of users who used this reaction
    pub users: Vec<PublicKey>,
}

/// Individual user's reaction
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserReaction {
    /// User who made the reaction
    pub user: PublicKey,

    /// The emoji they reacted with
    pub emoji: String,

    /// Timestamp of the reaction
    pub created_at: Timestamp,
}

/// Configuration for the message aggregator
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AggregatorConfig {
    /// Whether to normalize emoji (treat skin tone variants as same base emoji)
    pub normalize_emoji: bool,

    /// Whether to enable detailed logging of processing steps
    pub enable_debug_logging: bool,
}

impl Default for AggregatorConfig {
    fn default() -> Self {
        Self {
            normalize_emoji: true,
            enable_debug_logging: false,
        }
    }
}

/// Statistics about a group's message processing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupStatistics {
    pub message_count: usize,
    pub reaction_count: usize,
    pub deleted_message_count: usize,
    pub memory_usage_bytes: usize,
    pub last_processed_at: Option<Timestamp>,
}

/// Errors that can occur during message processing
#[derive(Debug, thiserror::Error)]
pub enum ProcessingError {
    #[error("Invalid reaction content")]
    InvalidReaction,

    #[error("Missing required e-tag in message")]
    MissingETag,

    #[error("Invalid tag format")]
    InvalidTag,

    #[error("Invalid timestamp")]
    InvalidTimestamp,

    #[error("Failed to fetch messages from mdk: {0}")]
    FetchFailed(String),

    #[error("Internal processing error: {0}")]
    Internal(String),
}
