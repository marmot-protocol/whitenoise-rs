use crate::api::error::ApiError;
use crate::api::groups::GroupType;
use crate::api::messages::ChatMessageSummary;
use crate::api::utils::group_id_to_string;
use crate::api::wn_session;
use chrono::{DateTime, Utc};
use flutter_rust_bridge::frb;
use nostr_sdk::PublicKey;
use whitenoise::whitenoise::chat_list::ChatListItem as WhitenoiseChatListItem;

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct ChatSummary {
    /// MLS group identifier (hex string)
    pub mls_group_id: String,
    /// Display name for this chat:
    /// - Groups: The group name (may be empty)
    /// - DMs: The other user's display name (None if no metadata)
    pub name: Option<String>,
    /// Type of chat: Group or DirectMessage
    pub group_type: GroupType,
    /// When this group was created
    pub created_at: DateTime<Utc>,
    /// Path to cached decrypted group image (Groups only)
    pub group_image_path: Option<String>,
    /// Profile picture URL of the other user (DMs only)
    pub group_image_url: Option<String>,
    /// Preview of the last message (None if no messages)
    pub last_message: Option<ChatMessageSummary>,
    /// Whether the group is pending user confirmation
    pub pending_confirmation: bool,
    /// Public key (hex) of the user who invited this account to the group.
    /// `Some` when invited by another user, `None` when the user created the group.
    pub welcomer_pubkey: Option<String>,
    /// When this chat was archived, if at all.
    pub archived_at: Option<DateTime<Utc>>,
    /// When this account was removed from the group by an admin, if at all.
    /// `Some` means the group is read-only; the user must archive/delete to hide it.
    pub removed_at: Option<DateTime<Utc>>,
    /// Whether the user voluntarily left the group. Only meaningful when `removed_at` is `Some`.
    pub self_removed: bool,
    /// Number of unread messages in this chat
    pub unread_count: u64,
    /// Pin order for chat list sorting.
    /// - `None` = not pinned (appears after pinned chats)
    /// - `Some(n)` = pinned, lower values appear first
    pub pin_order: Option<i64>,
    /// For DMs: the public key (hex) of the other participant.
    /// `None` for Group chats.
    pub dm_peer_pubkey: Option<String>,
    /// When this chat is muted until, if at all.
    /// `None` = not muted.
    /// `Some(far-future)` = muted forever.
    pub muted_until: Option<DateTime<Utc>>,
}

impl From<WhitenoiseChatListItem> for ChatSummary {
    fn from(item: WhitenoiseChatListItem) -> Self {
        Self {
            mls_group_id: group_id_to_string(&item.mls_group_id),
            name: item.name,
            group_type: item.group_type.into(),
            created_at: item.created_at,
            group_image_path: item
                .group_image_path
                .map(|p| p.to_string_lossy().to_string()),
            group_image_url: item.group_image_url,
            last_message: item.last_message.map(|m| m.into()),
            pending_confirmation: item.pending_confirmation,
            welcomer_pubkey: item.welcomer_pubkey.map(|pk| pk.to_hex()),
            archived_at: item.archived_at,
            removed_at: item.removed_at,
            self_removed: item.self_removed,
            unread_count: item.unread_count as u64,
            pin_order: item.pin_order,
            dm_peer_pubkey: item.dm_peer_pubkey.map(|pk| pk.to_hex()),
            muted_until: item.muted_until,
        }
    }
}

#[frb]
pub async fn get_chat_summary(
    account_pubkey: String,
    mls_group_id: String,
) -> Result<ChatSummary, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    let active = session.chat_list().active().await?;
    let archived = session.chat_list().archived().await?;
    let item = active
        .into_iter()
        .chain(archived)
        .find(|item| group_id_to_string(&item.mls_group_id) == mls_group_id)
        .ok_or_else(|| ApiError::Other {
            message: format!("Chat not found: {}", mls_group_id),
        })?;
    Ok(item.into())
}
