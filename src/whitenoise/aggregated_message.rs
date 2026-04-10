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
