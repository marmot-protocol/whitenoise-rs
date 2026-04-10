use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use mdk_core::prelude::GroupId;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::nostr_manager::parser::SerializableToken;
use crate::whitenoise::media_files::MediaFile;

/// Tracks the delivery state of an outgoing message.
///
/// Follows an optimistic UI pattern: the message appears instantly with `Sending`,
/// then transitions to `Sent` or `Failed` after the background publish completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryStatus {
    /// Background publish in progress (message visible in chat immediately)
    Sending,
    /// Published successfully to N relays
    Sent(usize),
    /// All publish attempts exhausted — reason string for debugging (not shown to user)
    Failed(String),
    /// The user retried this message — a new message was created with the same content.
    /// Messages in this state are excluded from UI snapshots so they don't resurface
    /// after app restart.
    Retried,
}

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

    /// Delivery status for outgoing messages.
    /// `None` for incoming messages, `Some(status)` for messages sent by the current user.
    pub delivery_status: Option<DeliveryStatus>,
}

/// A search result wrapping a matched message with token highlight spans.
///
/// Each entry in `highlight_spans` is a `[start, end]` pair of **char indices**
/// (not byte offsets) into `message.content`. The spans appear in the order the
/// query tokens were matched, left-to-right through the content.
///
/// Example: searching `"big plans"` in `"We have big plans"` yields
/// `highlight_spans = [[8, 11], [12, 17]]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchResult {
    /// The matched message, identical to what regular list queries return.
    pub message: ChatMessage,

    /// Char-index spans `[start, end]` (half-open) for each matched query token,
    /// in the order they appear in `message.content`.
    pub highlight_spans: Vec<[usize; 2]>,

    /// 0-based position of the message within the group (0 = newest),
    /// matching the `created_at DESC, message_id DESC` ordering used by pagination.
    /// The frontend can compute `page = position / page_size` to jump to this message.
    pub position: u64,
}

/// Lightweight message summary for previews (chat list).
///
/// This is a subset of `ChatMessage` optimized for display contexts where full
/// message data isn't needed. Uses `DateTime<Utc>` for database consistency.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessageSummary {
    /// Event ID of the message
    pub message_id: EventId,

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
    pub by_emoji: IndexMap<String, EmojiReaction>,

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

    pub reaction_id: EventId,
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

#[cfg(test)]
mod tests {
    use super::*;

    mod processing_error_tests {
        use super::*;

        #[test]
        fn test_error_display_messages() {
            assert_eq!(
                ProcessingError::InvalidReaction.to_string(),
                "Invalid reaction content"
            );
            assert_eq!(
                ProcessingError::MissingETag.to_string(),
                "Missing required e-tag in message"
            );
            assert_eq!(
                ProcessingError::InvalidTag.to_string(),
                "Invalid tag format"
            );
            assert_eq!(
                ProcessingError::InvalidTimestamp.to_string(),
                "Invalid timestamp"
            );
            assert_eq!(
                ProcessingError::FetchFailed("connection timeout".to_string()).to_string(),
                "Failed to fetch messages from mdk: connection timeout"
            );
            assert_eq!(
                ProcessingError::Internal("unexpected state".to_string()).to_string(),
                "Internal processing error: unexpected state"
            );
        }
    }
}
