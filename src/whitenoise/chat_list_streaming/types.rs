use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::whitenoise::chat_list::ChatListItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatListUpdateTrigger {
    NewGroup,
    NewLastMessage,
    LastMessageDeleted,
    /// The chat's archive status changed (archived or unarchived). The item's
    /// `archived_at` field indicates direction: `Some` = archived, `None` = unarchived.
    /// Emitted to both active and archived channels so each can add/remove accordingly.
    ChatArchiveChanged,
    /// The account was involuntarily removed from this group by an admin.
    /// The group stays visible (read-only) until the user explicitly archives or
    /// deletes it. The item's `removed_at` field is set. Routed to the active
    /// channel when not archived, or the archived channel if already archived.
    RemovedFromGroup,
    /// The chat's mute status changed (muted or unmuted). The item's
    /// `muted_until` field indicates the new state.
    ChatMuteChanged,
    /// The account voluntarily left this group via SelfRemove.
    /// The group stays visible (read-only) until the user explicitly archives
    /// or deletes it. The item's `removed_at` and `self_removed` fields are set.
    /// Routing: same as RemovedFromGroup — active or archived based on archive status.
    LeftGroup,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatListUpdate {
    pub trigger: ChatListUpdateTrigger,
    pub item: ChatListItem,
}

pub struct ChatListSubscription {
    pub initial_items: Vec<ChatListItem>,
    pub updates: broadcast::Receiver<ChatListUpdate>,
}
