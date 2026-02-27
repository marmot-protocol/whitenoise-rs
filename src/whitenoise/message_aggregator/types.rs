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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DeliveryStatus {
    /// Background publish in progress (message visible in chat immediately)
    Sending,
    /// Published successfully to N relays
    Sent(usize),
    /// All publish attempts exhausted — reason string for debugging (not shown to user)
    Failed(String),
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

    mod delivery_status_tests {
        use super::*;

        #[test]
        fn test_serialization_roundtrip() {
            let statuses = [
                DeliveryStatus::Sending,
                DeliveryStatus::Sent(3),
                DeliveryStatus::Failed("No relay accepted".to_string()),
            ];

            for status in &statuses {
                let json = serde_json::to_string(status).expect("serialize");
                let deserialized: DeliveryStatus =
                    serde_json::from_str(&json).expect("deserialize");
                assert_eq!(status, &deserialized);
            }
        }

        #[test]
        fn test_option_none_serialization() {
            let status: Option<DeliveryStatus> = None;
            let json = serde_json::to_string(&status).expect("serialize");
            assert_eq!(json, "null");
            let deserialized: Option<DeliveryStatus> =
                serde_json::from_str(&json).expect("deserialize");
            assert_eq!(deserialized, None);
        }

        #[test]
        fn test_option_some_serialization() {
            let status = Some(DeliveryStatus::Sent(5));
            let json = serde_json::to_string(&status).expect("serialize");
            let deserialized: Option<DeliveryStatus> =
                serde_json::from_str(&json).expect("deserialize");
            assert_eq!(deserialized, status);
        }

        #[test]
        fn test_clone_and_eq() {
            let status = DeliveryStatus::Failed("timeout".to_string());
            let cloned = status.clone();
            assert_eq!(status, cloned);
        }
    }

    mod aggregator_config_tests {
        use super::*;

        #[test]
        fn test_default_config() {
            let config = AggregatorConfig::default();
            assert!(config.normalize_emoji);
            assert!(!config.enable_debug_logging);
        }

        #[test]
        fn test_config_equality() {
            let config1 = AggregatorConfig::default();
            let config2 = AggregatorConfig {
                normalize_emoji: true,
                enable_debug_logging: false,
            };
            assert_eq!(config1, config2);
        }

        #[test]
        fn test_config_inequality() {
            let config1 = AggregatorConfig::default();
            let config2 = AggregatorConfig {
                normalize_emoji: false,
                enable_debug_logging: true,
            };
            assert_ne!(config1, config2);
        }
    }

    mod reaction_summary_tests {
        use super::*;

        #[test]
        fn test_default_reaction_summary() {
            let summary = ReactionSummary::default();
            assert!(summary.by_emoji.is_empty());
            assert!(summary.user_reactions.is_empty());
        }

        #[test]
        fn test_reaction_summary_with_data() {
            let pubkey = PublicKey::from_hex(
                "0000000000000000000000000000000000000000000000000000000000000001",
            )
            .unwrap();

            let emoji_reaction = EmojiReaction {
                emoji: "👍".to_string(),
                count: 1,
                users: vec![pubkey],
            };

            let user_reaction = UserReaction {
                user: pubkey,
                emoji: "👍".to_string(),
                created_at: Timestamp::now(),
                reaction_id: EventId::all_zeros(),
            };

            let mut by_emoji = IndexMap::new();
            by_emoji.insert("👍".to_string(), emoji_reaction);

            let summary = ReactionSummary {
                by_emoji,
                user_reactions: vec![user_reaction],
            };

            assert_eq!(summary.by_emoji.len(), 1);
            assert_eq!(summary.user_reactions.len(), 1);
            assert_eq!(summary.by_emoji.get("👍").unwrap().count, 1);
        }
    }

    mod emoji_reaction_tests {
        use super::*;

        #[test]
        fn test_emoji_reaction_creation() {
            let pubkey = PublicKey::from_hex(
                "0000000000000000000000000000000000000000000000000000000000000001",
            )
            .unwrap();

            let reaction = EmojiReaction {
                emoji: "❤️".to_string(),
                count: 3,
                users: vec![pubkey],
            };

            assert_eq!(reaction.emoji, "❤️");
            assert_eq!(reaction.count, 3);
            assert_eq!(reaction.users.len(), 1);
        }

        #[test]
        fn test_emoji_reaction_equality() {
            let pubkey = PublicKey::from_hex(
                "0000000000000000000000000000000000000000000000000000000000000001",
            )
            .unwrap();

            let reaction1 = EmojiReaction {
                emoji: "👍".to_string(),
                count: 1,
                users: vec![pubkey],
            };

            let reaction2 = EmojiReaction {
                emoji: "👍".to_string(),
                count: 1,
                users: vec![pubkey],
            };

            assert_eq!(reaction1, reaction2);
        }
    }

    mod user_reaction_tests {
        use super::*;

        #[test]
        fn test_user_reaction_creation() {
            let pubkey = PublicKey::from_hex(
                "0000000000000000000000000000000000000000000000000000000000000001",
            )
            .unwrap();
            let timestamp = Timestamp::now();
            let reaction_id = EventId::all_zeros();

            let reaction = UserReaction {
                user: pubkey,
                emoji: "🎉".to_string(),
                created_at: timestamp,
                reaction_id,
            };

            assert_eq!(reaction.user, pubkey);
            assert_eq!(reaction.emoji, "🎉");
            assert_eq!(reaction.created_at, timestamp);
            assert_eq!(reaction.reaction_id, reaction_id);
        }
    }

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

    mod group_statistics_tests {
        use super::*;

        #[test]
        fn test_group_statistics_creation() {
            let stats = GroupStatistics {
                message_count: 100,
                reaction_count: 50,
                deleted_message_count: 5,
                memory_usage_bytes: 1024,
                last_processed_at: Some(Timestamp::now()),
            };

            assert_eq!(stats.message_count, 100);
            assert_eq!(stats.reaction_count, 50);
            assert_eq!(stats.deleted_message_count, 5);
            assert_eq!(stats.memory_usage_bytes, 1024);
            assert!(stats.last_processed_at.is_some());
        }

        #[test]
        fn test_group_statistics_no_last_processed() {
            let stats = GroupStatistics {
                message_count: 0,
                reaction_count: 0,
                deleted_message_count: 0,
                memory_usage_bytes: 0,
                last_processed_at: None,
            };

            assert!(stats.last_processed_at.is_none());
        }
    }
}
