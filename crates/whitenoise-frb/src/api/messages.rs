use crate::api::wn;
use crate::api::{
    error::ApiError,
    media_files::MediaFile,
    utils::{group_id_from_string, group_id_to_string},
};
use crate::frb_generated::StreamSink;
use chrono::{DateTime, TimeZone, Utc};
use flutter_rust_bridge::frb;
use nostr_sdk::prelude::*;
use tracing::{info, warn};
use whitenoise::whitenoise::message_aggregator::ChatMessageSummary as WhitenoiseChatMessageSummary;
use whitenoise::whitenoise::message_aggregator::SearchResult as WhitenoiseSearchResult;
use whitenoise::whitenoise::messages::PaginationOptions;
pub use whitenoise::{
    ChatMessage as WhitenoiseChatMessage, DeliveryStatus as WhitenoiseDeliveryStatus,
    EmojiReaction as WhitenoiseEmojiReaction, MediaFile as WhitenoiseMediaFile,
    MessageUpdate as WhitenoiseMessageUpdate, MessageWithTokens as WhitenoiseMessageWithTokens,
    ReactionSummary as WhitenoiseReactionSummary, SerializableToken as WhitenoiseSerializableToken,
    UpdateTrigger as WhitenoiseUpdateTrigger, UserReaction as WhitenoiseUserReaction, Whitenoise,
};

/// Flutter-compatible message with tokens
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct MessageWithTokens {
    pub id: String,
    pub pubkey: String,
    pub kind: u16,
    pub created_at: DateTime<Utc>,
    pub content: Option<String>,
    pub tokens: Vec<SerializableToken>,
}

/// Flutter-compatible chat message
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub id: String,
    pub pubkey: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub tags: Vec<Vec<String>>,
    pub is_reply: bool,
    pub reply_to_id: Option<String>,
    pub is_deleted: bool,
    pub content_tokens: Vec<SerializableToken>,
    pub reactions: ReactionSummary,
    pub media_attachments: Vec<MediaFile>,
    pub kind: u16,
    /// Delivery status for outgoing messages. `None` for incoming messages.
    pub delivery_status: Option<DeliveryStatus>,
}

/// Flutter-compatible reaction summary
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct ReactionSummary {
    pub by_emoji: Vec<EmojiReaction>,
    pub user_reactions: Vec<UserReaction>,
}

/// Flutter-compatible emoji reaction details
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct EmojiReaction {
    pub emoji: String,
    pub count: u64,         // Using u64 for Flutter compatibility
    pub users: Vec<String>, // PublicKey converted to hex strings
}

/// Flutter-compatible user reaction
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct UserReaction {
    pub reaction_id: String,
    pub user: String,
    pub emoji: String,
    pub created_at: DateTime<Utc>,
}

/// Flutter-compatible serializable token
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct SerializableToken {
    pub token_type: String, // "Nostr", "Url", "Hashtag", "Text", "LineBreak", "Whitespace"
    pub content: Option<String>, // None for LineBreak and Whitespace
}

/// Tracks the delivery state of an outgoing message.
///
/// Follows an optimistic UI pattern: the message appears instantly with `Sending`,
/// then transitions to `Sent` or `Failed` after the background publish completes.
/// `None` delivery_status on ChatMessage means the message is incoming (from others).
#[frb]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryStatus {
    /// Background publish in progress
    Sending,
    /// Published successfully to N relays
    Sent { relay_count: u64 },
    /// All publish attempts exhausted
    Failed { reason: String },
    /// The user retried this message — excluded from UI snapshots
    Retried,
}

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct ChatMessageSummary {
    pub mls_group_id: String,
    pub author: String,
    pub author_display_name: Option<String>,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub media_attachment_count: u64,
}

/// What triggered a message update in the stream.
#[frb]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateTrigger {
    /// A new message was added to the group
    NewMessage,
    /// A reaction was added to this message
    ReactionAdded,
    /// A reaction was removed from this message
    ReactionRemoved,
    /// The message itself was marked as deleted
    MessageDeleted,
    /// The delivery status of an outgoing message changed (Sending -> Sent or Failed)
    DeliveryStatusChanged,
}

/// A real-time update for a group message.
///
/// Contains the trigger indicating what changed and the complete,
/// current state of the affected message.
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct MessageUpdate {
    pub trigger: UpdateTrigger,
    pub message: ChatMessage,
}

/// Stream item emitted by `subscribe_to_group_messages`.
///
/// The first item is always `InitialSnapshot` containing all current messages.
/// Subsequent items are `Update` containing real-time changes.
#[frb]
#[derive(Debug, Clone)]
pub enum MessageStreamItem {
    /// Initial snapshot of all messages in the group at subscription time
    InitialSnapshot { messages: Vec<ChatMessage> },
    /// Real-time update for a single message
    Update { update: MessageUpdate },
}

/// Char-index span marking where a query token matched in message content.
///
/// Indices are char-based (not byte-based), half-open: `content[start..end]`.
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct HighlightSpan {
    pub start: i32,
    pub end: i32,
}

/// A search result wrapping a matched message with highlight spans.
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub message: ChatMessage,
    /// The MLS group this message belongs to (hex-encoded).
    pub mls_group_id: String,
    /// One span per matched query token, in the order they appear in the content.
    pub highlight_spans: Vec<HighlightSpan>,
    /// 0-based position of the message within its group (0 = newest),
    /// matching the `created_at DESC, message_id DESC` ordering used by pagination.
    pub position: u64,
}

impl From<WhitenoiseSearchResult> for SearchResult {
    fn from(result: WhitenoiseSearchResult) -> Self {
        Self {
            message: (&result.message).into(),
            mls_group_id: group_id_to_string(&result.mls_group_id),
            highlight_spans: result
                .highlight_spans
                .into_iter()
                .map(|[s, e]| HighlightSpan {
                    start: s as i32,
                    end: e as i32,
                })
                .collect(),
            position: result.position,
        }
    }
}

// From implementations to convert from Whitenoise types to Flutter-compatible types

impl From<&WhitenoiseMessageWithTokens> for MessageWithTokens {
    fn from(message_with_tokens: &WhitenoiseMessageWithTokens) -> Self {
        // Convert tokens to Flutter-compatible representation
        let tokens = message_with_tokens
            .tokens
            .iter()
            .map(|token| token.into())
            .collect();

        Self {
            id: message_with_tokens.message.id.to_hex(),
            pubkey: message_with_tokens.message.pubkey.to_hex(),
            kind: message_with_tokens.message.kind.as_u16(),
            created_at: {
                let ts =
                    i64::try_from(message_with_tokens.message.created_at.as_secs()).unwrap_or(0);
                Utc.timestamp_opt(ts, 0)
                    .single()
                    .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap())
            },
            content: Some(message_with_tokens.message.content.clone()),
            tokens,
        }
    }
}

impl From<WhitenoiseMessageWithTokens> for MessageWithTokens {
    fn from(message_with_tokens: WhitenoiseMessageWithTokens) -> Self {
        (&message_with_tokens).into()
    }
}

impl From<&WhitenoiseSerializableToken> for SerializableToken {
    fn from(token: &WhitenoiseSerializableToken) -> Self {
        match token {
            WhitenoiseSerializableToken::Nostr(s) => Self {
                token_type: "Nostr".to_string(),
                content: Some(s.clone()),
            },
            WhitenoiseSerializableToken::Url(s) => Self {
                token_type: "Url".to_string(),
                content: Some(s.clone()),
            },
            WhitenoiseSerializableToken::Hashtag(s) => Self {
                token_type: "Hashtag".to_string(),
                content: Some(s.clone()),
            },
            WhitenoiseSerializableToken::Text(s) => Self {
                token_type: "Text".to_string(),
                content: Some(s.clone()),
            },
            WhitenoiseSerializableToken::LineBreak => Self {
                token_type: "LineBreak".to_string(),
                content: None,
            },
            WhitenoiseSerializableToken::Whitespace => Self {
                token_type: "Whitespace".to_string(),
                content: None,
            },
        }
    }
}

impl From<WhitenoiseSerializableToken> for SerializableToken {
    fn from(token: WhitenoiseSerializableToken) -> Self {
        (&token).into()
    }
}

impl From<WhitenoiseChatMessageSummary> for ChatMessageSummary {
    fn from(summary: WhitenoiseChatMessageSummary) -> Self {
        Self {
            mls_group_id: group_id_to_string(&summary.mls_group_id),
            author: summary.author.to_hex(),
            author_display_name: summary.author_display_name,
            content: summary.content,
            created_at: summary.created_at,
            media_attachment_count: summary.media_attachment_count as u64,
        }
    }
}

impl From<&WhitenoiseDeliveryStatus> for DeliveryStatus {
    fn from(status: &WhitenoiseDeliveryStatus) -> Self {
        match status {
            WhitenoiseDeliveryStatus::Sending => Self::Sending,
            WhitenoiseDeliveryStatus::Sent(count) => Self::Sent {
                relay_count: *count as u64,
            },
            WhitenoiseDeliveryStatus::Failed(reason) => Self::Failed {
                reason: reason.clone(),
            },
            WhitenoiseDeliveryStatus::Retried => Self::Retried,
        }
    }
}

impl From<WhitenoiseDeliveryStatus> for DeliveryStatus {
    fn from(status: WhitenoiseDeliveryStatus) -> Self {
        (&status).into()
    }
}

impl From<&WhitenoiseReactionSummary> for ReactionSummary {
    fn from(reactions: &WhitenoiseReactionSummary) -> Self {
        let by_emoji = reactions
            .by_emoji
            .iter()
            .map(|(emoji, reaction)| EmojiReaction {
                emoji: emoji.clone(),
                count: reaction.count as u64,
                users: reaction.users.iter().map(|pk| pk.to_hex()).collect(),
            })
            .collect();

        let user_reactions = reactions
            .user_reactions
            .iter()
            .map(|user_reaction| UserReaction {
                reaction_id: user_reaction.reaction_id.to_hex(),
                user: user_reaction.user.to_hex(),
                emoji: user_reaction.emoji.clone(),
                created_at: {
                    let ts = i64::try_from(user_reaction.created_at.as_secs()).unwrap_or(0);
                    Utc.timestamp_opt(ts, 0)
                        .single()
                        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap())
                },
            })
            .collect();

        Self {
            by_emoji,
            user_reactions,
        }
    }
}

impl From<WhitenoiseReactionSummary> for ReactionSummary {
    fn from(reactions: WhitenoiseReactionSummary) -> Self {
        (&reactions).into()
    }
}

impl From<&WhitenoiseChatMessage> for ChatMessage {
    fn from(chat_message: &WhitenoiseChatMessage) -> Self {
        let tags = chat_message
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect();

        // Convert content tokens to proper Flutter-compatible structs
        let content_tokens = chat_message
            .content_tokens
            .iter()
            .map(|token| token.into())
            .collect();

        // Convert reactions to proper Flutter-compatible struct
        let reactions = (&chat_message.reactions).into();

        Self {
            id: chat_message.id.clone(),
            pubkey: chat_message.author.to_hex(),
            content: chat_message.content.clone(),
            created_at: {
                let ts = i64::try_from(chat_message.created_at.as_secs()).unwrap_or(0);
                Utc.timestamp_opt(ts, 0)
                    .single()
                    .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap())
            },
            tags,
            is_reply: chat_message.is_reply,
            reply_to_id: chat_message.reply_to_id.clone(),
            is_deleted: chat_message.is_deleted,
            content_tokens,
            reactions,
            media_attachments: chat_message
                .media_attachments
                .clone()
                .into_iter()
                .map(|media_file| media_file.into())
                .collect(),
            kind: chat_message.kind,
            delivery_status: chat_message.delivery_status.as_ref().map(|s| s.into()),
        }
    }
}

impl From<WhitenoiseChatMessage> for ChatMessage {
    fn from(chat_message: WhitenoiseChatMessage) -> Self {
        (&chat_message).into()
    }
}

impl From<WhitenoiseUpdateTrigger> for UpdateTrigger {
    fn from(trigger: WhitenoiseUpdateTrigger) -> Self {
        match trigger {
            WhitenoiseUpdateTrigger::NewMessage => Self::NewMessage,
            WhitenoiseUpdateTrigger::ReactionAdded => Self::ReactionAdded,
            WhitenoiseUpdateTrigger::ReactionRemoved => Self::ReactionRemoved,
            WhitenoiseUpdateTrigger::MessageDeleted => Self::MessageDeleted,
            WhitenoiseUpdateTrigger::DeliveryStatusChanged => Self::DeliveryStatusChanged,
        }
    }
}

impl From<&WhitenoiseMessageUpdate> for MessageUpdate {
    fn from(update: &WhitenoiseMessageUpdate) -> Self {
        Self {
            trigger: update.trigger.into(),
            message: (&update.message).into(),
        }
    }
}

impl From<WhitenoiseMessageUpdate> for MessageUpdate {
    fn from(update: WhitenoiseMessageUpdate) -> Self {
        (&update).into()
    }
}

#[frb]
pub async fn send_message_to_group(
    pubkey: String,
    group_id: String,
    message: String,
    kind: u16,
    tags: Option<Vec<Tag>>,
) -> Result<MessageWithTokens, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    let group_id = group_id_from_string(&group_id)?;
    let message_with_tokens = whitenoise
        .send_message_to_group(&account, &group_id, message, kind, tags)
        .await?;
    Ok((&message_with_tokens).into())
}

/// Retry publishing a failed message.
///
/// Creates a new message with the same content and marks the original as `Retried`.
#[frb]
pub async fn retry_message_publish(
    pubkey: String,
    group_id: String,
    event_id: String,
) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    let group_id = group_id_from_string(&group_id)?;
    let event_id = EventId::parse(&event_id)?;
    whitenoise
        .retry_message_publish(&account, &group_id, &event_id)
        .await?;
    Ok(())
}

/// Search messages within a group by content.
///
/// Uses forward-order substring matching: tokens from the query must appear
/// in the same order within the message content. Case-insensitive.
/// Supports all Unicode scripts including CJK.
///
/// Returns `SearchResult` items containing both the matched message and
/// `highlight_spans` — char-index `[start, end]` pairs for frontend highlighting.
#[frb]
pub async fn search_messages_in_group(
    pubkey: String,
    group_id: String,
    query: String,
    limit: Option<u32>,
) -> Result<Vec<SearchResult>, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let results = whitenoise
        .search_messages_in_group(&pubkey, &group_id, &query, limit)
        .await?;
    Ok(results.into_iter().map(|r| r.into()).collect())
}

/// Search messages across all groups the account belongs to.
///
/// Like [`search_messages_in_group`] but without a group filter. Each result
/// includes `mls_group_id` so callers can group results by conversation.
/// Position is computed per-group so the frontend can still jump to the
/// correct page within each conversation.
#[frb]
pub async fn search_messages(
    pubkey: String,
    query: String,
    limit: Option<u32>,
) -> Result<Vec<SearchResult>, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let results = whitenoise.search_messages(&pubkey, &query, limit).await?;
    Ok(results.into_iter().map(|r| r.into()).collect())
}

/// Fetch a paginated page of messages for a group.
///
/// Returns messages in oldest-first order. Pass the `created_at` and `id` of the
/// oldest message currently loaded to fetch the preceding page (infinite scroll upward).
/// Omit `before` and `before_message_id` for the initial load.
#[frb]
pub async fn fetch_aggregated_messages_for_group(
    pubkey: String,
    group_id: String,
    before: Option<DateTime<Utc>>,
    before_message_id: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<ChatMessage>, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let before_ts = before
        .map(|dt| {
            let ts = dt.timestamp();
            if ts < 0 {
                return Err(ApiError::Other {
                    message: format!("invalid cursor: before timestamp {ts} is before Unix epoch"),
                });
            }
            Ok(Timestamp::from(ts as u64))
        })
        .transpose()?;
    let options = PaginationOptions {
        before: before_ts,
        before_message_id,
        ..Default::default()
    };
    let messages = whitenoise
        .fetch_aggregated_messages_for_group(&pubkey, &group_id, &options, limit)
        .await?;
    Ok(messages.into_iter().map(|m| m.into()).collect())
}

/// Fetch a single message by its event ID.
///
/// Returns `None` if the message does not exist in the cache.
#[frb]
pub async fn fetch_message_by_id(
    pubkey: String,
    group_id: String,
    message_id: String,
) -> Result<Option<ChatMessage>, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let message = whitenoise
        .fetch_message_by_id(&pubkey, &group_id, &message_id)
        .await?;
    Ok(message.map(|m| m.into()))
}

/// Fetch the newest messages for a group, ensuring all unread messages are included
/// and at least `minimum` messages are returned.
///
/// The effective fetch size is `max(unread_count, minimum)`, so the caller always
/// receives a full page even when there are no unread messages, and always receives
/// every unread message when there are more unreads than the minimum.
///
/// Messages are returned in oldest-first order.
#[frb]
pub async fn fetch_messages_unread_with_minimum(
    pubkey: String,
    group_id: String,
    minimum: Option<u32>,
) -> Result<Vec<ChatMessage>, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let messages = whitenoise
        .fetch_messages_unread_with_minimum(&pubkey, &group_id, minimum)
        .await?;
    Ok(messages.into_iter().map(|m| m.into()).collect())
}

/// Subscribe to real-time message updates for a group.
///
/// The stream first emits an `InitialSnapshot`, then emits `Update` items as
/// messages are added, reacted to, or deleted.
///
/// **Initial snapshot contents depend on whether `pubkey` is supplied:**
///
/// * **Without `pubkey`:** the snapshot contains all messages currently held by
///   the subscription — a full current-message window.  Callers can treat it as
///   a complete history up to the point of subscription.
///
/// * **With `pubkey`:** the snapshot is produced by
///   `fetch_messages_unread_with_minimum` and is an *unread-focused* window: it
///   always includes every unread message plus at least 50 recent messages.
///   Callers **must not** assume the snapshot covers full history in this case;
///   older messages are available via `fetch_aggregated_messages_for_group`.
///
/// The initial snapshot is race-condition free: any updates that arrive between
/// subscribing and fetching are merged into the snapshot.
#[frb]
pub async fn subscribe_to_group_messages(
    pubkey: Option<String>,
    group_id: String,
    sink: StreamSink<MessageStreamItem>,
) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let group_id_str = group_id.clone();
    let group_id = group_id_from_string(&group_id)?;

    info!(group_id = %group_id_str, "subscribe_to_group_messages: subscribing");

    let parsed_pubkey = pubkey.as_deref().map(PublicKey::parse).transpose()?;
    let subscription_pubkey = parsed_pubkey.as_ref().ok_or_else(|| ApiError::Other {
        message: "pubkey is required to subscribe to group messages".to_string(),
    })?;

    let subscription = whitenoise
        .subscribe_to_group_messages(subscription_pubkey, &group_id, None)
        .await?;

    // When a pubkey is provided, replace the subscription's initial snapshot with an
    // unread-aware fetch so the caller always receives every unread message plus at
    // least 50 recent ones — eliminating backwards pagination just to find the read
    // marker.  The subscription was created first (above) so any concurrent updates
    // that arrived during the fetch will arrive on `subscription.updates`.
    let raw_initial: Vec<WhitenoiseChatMessage> = match parsed_pubkey {
        Some(ref pk) => match whitenoise
            .fetch_messages_unread_with_minimum(pk, &group_id, None)
            .await
        {
            Ok(messages) => messages,
            Err(e) => {
                warn!(
                    group_id = %group_id_str,
                    pubkey = %pk.to_hex(),
                    error = %e,
                    "subscribe_to_group_messages: fetch_messages_unread_with_minimum failed, \
                     falling back to subscription initial_messages"
                );
                subscription.initial_messages
            }
        },
        None => subscription.initial_messages,
    };

    // Emit initial snapshot first
    let initial_messages: Vec<ChatMessage> = raw_initial.into_iter().map(|m| m.into()).collect();

    info!(
        group_id = %group_id_str,
        count = initial_messages.len(),
        "subscribe_to_group_messages: emitting initial snapshot"
    );

    if sink
        .add(MessageStreamItem::InitialSnapshot {
            messages: initial_messages,
        })
        .is_err()
    {
        info!(group_id = %group_id_str, "subscribe_to_group_messages: sink closed after snapshot, exiting");
        return Ok(()); // Sink closed, exit gracefully
    }

    // Stream real-time updates
    let mut rx = subscription.updates;
    let mut lagged_total: u64 = 0;
    loop {
        match rx.recv().await {
            Ok(update) => {
                info!(
                    group_id = %group_id_str,
                    trigger = ?update.trigger,
                    message_id = %update.message.id,
                    is_deleted = update.message.is_deleted,
                    "subscribe_to_group_messages: emitting update"
                );
                let item = MessageStreamItem::Update {
                    update: update.into(),
                };
                if sink.add(item).is_err() {
                    info!(group_id = %group_id_str, "subscribe_to_group_messages: sink closed, exiting");
                    break; // Sink closed
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                lagged_total += n;
                warn!(
                    group_id = %group_id_str,
                    skipped = n,
                    total_lagged = lagged_total,
                    "subscribe_to_group_messages: consumer lagged, skipping updates"
                );
                // Safe to continue since each update contains the complete message state
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                info!(group_id = %group_id_str, "subscribe_to_group_messages: channel closed, exiting");
                break; // Channel closed
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_trigger_conversion_new_message() {
        let trigger: UpdateTrigger = WhitenoiseUpdateTrigger::NewMessage.into();
        assert_eq!(trigger, UpdateTrigger::NewMessage);
    }

    #[test]
    fn test_update_trigger_conversion_reaction_added() {
        let trigger: UpdateTrigger = WhitenoiseUpdateTrigger::ReactionAdded.into();
        assert_eq!(trigger, UpdateTrigger::ReactionAdded);
    }

    #[test]
    fn test_update_trigger_conversion_reaction_removed() {
        let trigger: UpdateTrigger = WhitenoiseUpdateTrigger::ReactionRemoved.into();
        assert_eq!(trigger, UpdateTrigger::ReactionRemoved);
    }

    #[test]
    fn test_update_trigger_conversion_message_deleted() {
        let trigger: UpdateTrigger = WhitenoiseUpdateTrigger::MessageDeleted.into();
        assert_eq!(trigger, UpdateTrigger::MessageDeleted);
    }

    #[test]
    fn test_update_trigger_conversion_delivery_status_changed() {
        let trigger: UpdateTrigger = WhitenoiseUpdateTrigger::DeliveryStatusChanged.into();
        assert_eq!(trigger, UpdateTrigger::DeliveryStatusChanged);
    }

    #[test]
    fn test_delivery_status_conversion_sending() {
        let status: DeliveryStatus = WhitenoiseDeliveryStatus::Sending.into();
        assert_eq!(status, DeliveryStatus::Sending);
    }

    #[test]
    fn test_delivery_status_conversion_sent() {
        let status: DeliveryStatus = WhitenoiseDeliveryStatus::Sent(3).into();
        assert_eq!(status, DeliveryStatus::Sent { relay_count: 3 });
    }

    #[test]
    fn test_delivery_status_conversion_failed() {
        let status: DeliveryStatus = WhitenoiseDeliveryStatus::Failed("timeout".to_string()).into();
        assert_eq!(
            status,
            DeliveryStatus::Failed {
                reason: "timeout".to_string()
            }
        );
    }

    #[test]
    fn test_delivery_status_conversion_retried() {
        let status: DeliveryStatus = WhitenoiseDeliveryStatus::Retried.into();
        assert_eq!(status, DeliveryStatus::Retried);
    }
}
