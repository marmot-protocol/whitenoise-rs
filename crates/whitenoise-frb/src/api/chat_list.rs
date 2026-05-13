use crate::api::chat_summary::ChatSummary;
use crate::api::error::ApiError;
use crate::api::{wn, wn_session};
use crate::frb_generated::StreamSink;
use chrono::{DateTime, Utc};
use flutter_rust_bridge::frb;
use nostr_sdk::PublicKey;
use whitenoise::mdk::GroupId;
use whitenoise::{
    ChatListUpdate as WhitenoiseChatListUpdate,
    ChatListUpdateTrigger as WhitenoiseChatListUpdateTrigger, MuteDuration,
};

/// How long to mute a chat.
///
/// Use a preset variant for common durations or [`ChatMuteDuration::Custom`]
/// for an arbitrary future timestamp.
#[frb]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatMuteDuration {
    /// Mute for 1 hour
    OneHour,
    /// Mute for 8 hours
    EightHours,
    /// Mute for 1 day
    OneDay,
    /// Mute for 1 week
    OneWeek,
    /// Mute until manually unmuted
    Forever,
    /// Mute until a specific timestamp (must be in the future)
    Custom { until: DateTime<Utc> },
}

impl From<ChatMuteDuration> for MuteDuration {
    fn from(d: ChatMuteDuration) -> Self {
        match d {
            ChatMuteDuration::OneHour => Self::OneHour,
            ChatMuteDuration::EightHours => Self::EightHours,
            ChatMuteDuration::OneDay => Self::OneDay,
            ChatMuteDuration::OneWeek => Self::OneWeek,
            ChatMuteDuration::Forever => Self::Forever,
            ChatMuteDuration::Custom { until } => Self::Custom(until),
        }
    }
}

/// What triggered a chat list update in the stream.
#[frb]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatListUpdateTrigger {
    /// A new group was created or joined
    NewGroup,
    /// A new message updated the chat's last message preview
    NewLastMessage,
    /// The last message in a chat was deleted
    LastMessageDeleted,
    /// A chat's archive status changed
    ChatArchiveChanged,
    /// This account was removed from the group by an admin.
    RemovedFromGroup,
    /// The chat's mute status changed.
    ChatMuteChanged,
    /// This account left the group.
    LeftGroup,
    /// All messages in the chat were cleared.
    ChatCleared,
    /// The chat was deleted.
    ChatDeleted,
    /// A user was blocked or unblocked.
    UserBlockChanged,
}

impl From<WhitenoiseChatListUpdateTrigger> for ChatListUpdateTrigger {
    fn from(trigger: WhitenoiseChatListUpdateTrigger) -> Self {
        match trigger {
            WhitenoiseChatListUpdateTrigger::NewGroup => Self::NewGroup,
            WhitenoiseChatListUpdateTrigger::NewLastMessage => Self::NewLastMessage,
            WhitenoiseChatListUpdateTrigger::LastMessageDeleted => Self::LastMessageDeleted,
            WhitenoiseChatListUpdateTrigger::ChatArchiveChanged => Self::ChatArchiveChanged,
            WhitenoiseChatListUpdateTrigger::RemovedFromGroup => Self::RemovedFromGroup,
            WhitenoiseChatListUpdateTrigger::ChatMuteChanged => Self::ChatMuteChanged,
            WhitenoiseChatListUpdateTrigger::LeftGroup => Self::LeftGroup,
            WhitenoiseChatListUpdateTrigger::ChatCleared => Self::ChatCleared,
            WhitenoiseChatListUpdateTrigger::ChatDeleted => Self::ChatDeleted,
            WhitenoiseChatListUpdateTrigger::UserBlockChanged => Self::UserBlockChanged,
        }
    }
}

/// A real-time update for the chat list.
///
/// Contains the trigger indicating what changed and the complete,
/// current state of the affected chat item.
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct ChatListUpdate {
    pub trigger: ChatListUpdateTrigger,
    pub item: ChatSummary,
}

impl From<WhitenoiseChatListUpdate> for ChatListUpdate {
    fn from(update: WhitenoiseChatListUpdate) -> Self {
        Self {
            trigger: update.trigger.into(),
            item: update.item.into(),
        }
    }
}

/// Stream item emitted by `subscribe_to_chat_list`.
///
/// The first item is always `InitialSnapshot` containing all current chats.
/// Subsequent items are `Update` containing real-time changes.
#[frb]
#[derive(Debug, Clone)]
pub enum ChatListStreamItem {
    /// Initial snapshot of all chats at subscription time
    InitialSnapshot { items: Vec<ChatSummary> },
    /// Real-time update for a single chat
    Update { update: ChatListUpdate },
}

/// Sets the pin order for a chat.
///
/// Pinned chats appear before unpinned chats in the chat list.
/// Lower pin_order values appear first among pinned chats.
///
/// - `pin_order = None` = unpin the chat
/// - `pin_order = Some(n)` = pin the chat with order n
#[frb]
pub async fn set_chat_pin_order(
    account_pubkey: String,
    mls_group_id: String,
    pin_order: Option<i64>,
) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id_bytes = hex::decode(&mls_group_id)?;
    let group_id = GroupId::from_slice(&group_id_bytes);
    let session = wn_session(&pubkey)?;

    session
        .membership()
        .for_group(&group_id)
        .set_pin_order(pin_order)
        .await?;

    Ok(())
}

/// Mutes a chat for the specified duration.
///
/// Notifications for this chat will be suppressed until the duration expires.
/// Use [`ChatMuteDuration::Forever`] to mute indefinitely.
#[frb]
pub async fn mute_chat(
    account_pubkey: String,
    mls_group_id: String,
    duration: ChatMuteDuration,
) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id_bytes = hex::decode(&mls_group_id)?;
    let group_id = GroupId::from_slice(&group_id_bytes);
    let session = wn_session(&pubkey)?;

    session
        .membership()
        .for_group(&group_id)
        .mute(duration.into())
        .await?;

    Ok(())
}

/// Unmutes a previously muted chat.
///
/// Notifications for this chat will resume immediately.
#[frb]
pub async fn unmute_chat(account_pubkey: String, mls_group_id: String) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let group_id_bytes = hex::decode(&mls_group_id)?;
    let group_id = GroupId::from_slice(&group_id_bytes);
    let session = wn_session(&pubkey)?;

    session.membership().for_group(&group_id).unmute().await?;

    Ok(())
}

/// Retrieves the chat list for an account.
///
/// Returns a list of chat summaries sorted by:
/// 1. Pinned chats first (sorted by pin_order, lower values first)
/// 2. Unpinned chats sorted by last activity (most recent first)
/// 3. Groups without messages are sorted by creation date
#[frb]
pub async fn get_chat_list(account_pubkey: String) -> Result<Vec<ChatSummary>, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    let chat_list = session.chat_list().active().await?;
    Ok(chat_list.into_iter().map(|item| item.into()).collect())
}

/// Subscribe to real-time chat list updates for an account.
///
/// The stream first emits an `InitialSnapshot` containing all current chats,
/// then emits `Update` items as chats are created, receive new messages, or have messages deleted.
///
/// The initial snapshot is race-condition free: any updates that arrive between
/// subscribing and fetching are merged into the snapshot.
#[frb]
pub async fn subscribe_to_chat_list(
    account_pubkey: String,
    sink: StreamSink<ChatListStreamItem>,
) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;

    let subscription = whitenoise.subscribe_to_chat_list(&account).await?;

    // Emit initial snapshot first
    let initial_items: Vec<ChatSummary> = subscription
        .initial_items
        .into_iter()
        .map(|item| item.into())
        .collect();

    if sink
        .add(ChatListStreamItem::InitialSnapshot {
            items: initial_items,
        })
        .is_err()
    {
        return Ok(()); // Sink closed, exit gracefully
    }

    // Stream real-time updates
    let mut rx = subscription.updates;
    loop {
        match rx.recv().await {
            Ok(update) => {
                let item = ChatListStreamItem::Update {
                    update: update.into(),
                };
                if sink.add(item).is_err() {
                    break; // Sink closed
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // Slow consumer missed some updates - safe to continue since
                // each update contains the complete chat item state
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break; // Channel closed
            }
        }
    }

    Ok(())
}

#[frb]
pub async fn subscribe_to_archived_chat_list(
    account_pubkey: String,
    sink: StreamSink<ChatListStreamItem>,
) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;

    let subscription = whitenoise.subscribe_to_archived_chat_list(&account).await?;

    let initial_items: Vec<ChatSummary> = subscription
        .initial_items
        .into_iter()
        .map(|item| item.into())
        .collect();

    if sink
        .add(ChatListStreamItem::InitialSnapshot {
            items: initial_items,
        })
        .is_err()
    {
        return Ok(());
    }

    let mut rx = subscription.updates;
    loop {
        match rx.recv().await {
            Ok(update) => {
                let item = ChatListStreamItem::Update {
                    update: update.into(),
                };
                if sink.add(item).is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_list_update_trigger_conversion_new_group() {
        let trigger: ChatListUpdateTrigger = WhitenoiseChatListUpdateTrigger::NewGroup.into();
        assert_eq!(trigger, ChatListUpdateTrigger::NewGroup);
    }

    #[test]
    fn test_chat_list_update_trigger_conversion_new_last_message() {
        let trigger: ChatListUpdateTrigger = WhitenoiseChatListUpdateTrigger::NewLastMessage.into();
        assert_eq!(trigger, ChatListUpdateTrigger::NewLastMessage);
    }

    #[test]
    fn test_chat_list_update_trigger_conversion_last_message_deleted() {
        let trigger: ChatListUpdateTrigger =
            WhitenoiseChatListUpdateTrigger::LastMessageDeleted.into();
        assert_eq!(trigger, ChatListUpdateTrigger::LastMessageDeleted);
    }

    #[test]
    fn test_chat_list_update_trigger_conversion_chat_archive_changed() {
        let trigger: ChatListUpdateTrigger =
            WhitenoiseChatListUpdateTrigger::ChatArchiveChanged.into();
        assert_eq!(trigger, ChatListUpdateTrigger::ChatArchiveChanged);
    }

    #[test]
    fn test_chat_list_update_trigger_conversion_removed_from_group() {
        let trigger: ChatListUpdateTrigger =
            WhitenoiseChatListUpdateTrigger::RemovedFromGroup.into();
        assert_eq!(trigger, ChatListUpdateTrigger::RemovedFromGroup);
    }

    #[test]
    fn test_chat_list_update_trigger_conversion_chat_mute_changed() {
        let trigger: ChatListUpdateTrigger =
            WhitenoiseChatListUpdateTrigger::ChatMuteChanged.into();
        assert_eq!(trigger, ChatListUpdateTrigger::ChatMuteChanged);
    }

    #[test]
    fn test_chat_mute_duration_to_mute_duration_conversion() {
        assert_eq!(
            MuteDuration::from(ChatMuteDuration::OneHour),
            MuteDuration::OneHour
        );
        assert_eq!(
            MuteDuration::from(ChatMuteDuration::EightHours),
            MuteDuration::EightHours
        );
        assert_eq!(
            MuteDuration::from(ChatMuteDuration::OneDay),
            MuteDuration::OneDay
        );
        assert_eq!(
            MuteDuration::from(ChatMuteDuration::OneWeek),
            MuteDuration::OneWeek
        );
        assert_eq!(
            MuteDuration::from(ChatMuteDuration::Forever),
            MuteDuration::Forever
        );

        let custom_time = Utc::now() + chrono::Duration::hours(3);
        assert_eq!(
            MuteDuration::from(ChatMuteDuration::Custom { until: custom_time }),
            MuteDuration::Custom(custom_time)
        );
    }

    #[test]
    fn test_chat_list_update_trigger_conversion_chat_cleared() {
        let trigger: ChatListUpdateTrigger = WhitenoiseChatListUpdateTrigger::ChatCleared.into();
        assert_eq!(trigger, ChatListUpdateTrigger::ChatCleared);
    }

    #[test]
    fn test_chat_list_update_trigger_conversion_chat_deleted() {
        let trigger: ChatListUpdateTrigger = WhitenoiseChatListUpdateTrigger::ChatDeleted.into();
        assert_eq!(trigger, ChatListUpdateTrigger::ChatDeleted);
    }

    #[test]
    fn test_chat_list_update_trigger_conversion_user_block_changed() {
        let trigger: ChatListUpdateTrigger =
            WhitenoiseChatListUpdateTrigger::UserBlockChanged.into();
        assert_eq!(trigger, ChatListUpdateTrigger::UserBlockChanged);
    }
}
