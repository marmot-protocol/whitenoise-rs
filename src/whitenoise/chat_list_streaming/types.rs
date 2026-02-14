use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::whitenoise::chat_list::ChatListItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatListUpdateTrigger {
    NewGroup,
    NewLastMessage,
    LastMessageDeleted,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ChatListUpdate {
    pub trigger: ChatListUpdateTrigger,
    pub item: ChatListItem,
}

/// Custom Debug impl to prevent sensitive data from leaking into logs.
impl std::fmt::Debug for ChatListUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatListUpdate")
            .field("trigger", &self.trigger)
            .finish()
    }
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
    }
}
