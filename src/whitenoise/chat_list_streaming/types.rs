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
    /// All messages were cleared from this chat.
    /// The group remains in the chat list with no messages.
    /// Consumers should reset their message list to empty.
    ChatCleared,
    /// The chat was permanently deleted by this account.
    /// Consumers must remove the item from their list.
    /// Emitted to both active and archived channels since the group
    /// could be in either state at the time of deletion.
    ChatDeleted,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_trigger_derives_copy_and_eq() {
        let trigger = ChatListUpdateTrigger::NewGroup;
        let copied = trigger; // Copy trait allows implicit copy
        assert_eq!(trigger, copied);

        let trigger2 = ChatListUpdateTrigger::NewLastMessage;
        assert_ne!(trigger, trigger2);
    }

    #[test]
    fn update_trigger_serialization_roundtrip() {
        let triggers = [
            ChatListUpdateTrigger::NewGroup,
            ChatListUpdateTrigger::NewLastMessage,
            ChatListUpdateTrigger::LastMessageDeleted,
            ChatListUpdateTrigger::ChatArchiveChanged,
            ChatListUpdateTrigger::RemovedFromGroup,
            ChatListUpdateTrigger::ChatMuteChanged,
            ChatListUpdateTrigger::LeftGroup,
            ChatListUpdateTrigger::ChatCleared,
            ChatListUpdateTrigger::ChatDeleted,
        ];

        for trigger in triggers {
            let serialized = serde_json::to_string(&trigger).expect("serialize");
            let deserialized: ChatListUpdateTrigger =
                serde_json::from_str(&serialized).expect("deserialize");
            assert_eq!(trigger, deserialized);
        }
    }

    #[test]
    fn update_trigger_debug_output() {
        let debug_str = format!("{:?}", ChatListUpdateTrigger::NewGroup);
        assert!(debug_str.contains("NewGroup"));

        let debug_str = format!("{:?}", ChatListUpdateTrigger::NewLastMessage);
        assert!(debug_str.contains("NewLastMessage"));

        let debug_str = format!("{:?}", ChatListUpdateTrigger::LastMessageDeleted);
        assert!(debug_str.contains("LastMessageDeleted"));

        let debug_str = format!("{:?}", ChatListUpdateTrigger::ChatArchiveChanged);
        assert!(debug_str.contains("ChatArchiveChanged"));

        let debug_str = format!("{:?}", ChatListUpdateTrigger::RemovedFromGroup);
        assert!(debug_str.contains("RemovedFromGroup"));

        let debug_str = format!("{:?}", ChatListUpdateTrigger::ChatMuteChanged);
        assert!(debug_str.contains("ChatMuteChanged"));

        let debug_str = format!("{:?}", ChatListUpdateTrigger::LeftGroup);
        assert!(debug_str.contains("LeftGroup"));

        let debug_str = format!("{:?}", ChatListUpdateTrigger::ChatCleared);
        assert!(debug_str.contains("ChatCleared"));

        let debug_str = format!("{:?}", ChatListUpdateTrigger::ChatDeleted);
        assert!(debug_str.contains("ChatDeleted"));
    }
}
