use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::whitenoise::users::User;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UserUpdateTrigger {
    UserCreated,
    MetadataChanged,
    LocalMetadataChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserUpdate {
    pub trigger: UserUpdateTrigger,
    pub user: User,
}

pub struct UserSubscription {
    pub initial_user: User,
    pub updates: broadcast::Receiver<UserUpdate>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_trigger_derives_copy_and_eq() {
        let trigger = UserUpdateTrigger::UserCreated;
        let copied = trigger;
        assert_eq!(trigger, copied);

        let trigger2 = UserUpdateTrigger::MetadataChanged;
        assert_ne!(trigger, trigger2);
    }

    #[test]
    fn update_trigger_serialization_roundtrip() {
        let triggers = [
            UserUpdateTrigger::UserCreated,
            UserUpdateTrigger::MetadataChanged,
            UserUpdateTrigger::LocalMetadataChanged,
        ];

        for trigger in triggers {
            let serialized = serde_json::to_string(&trigger).expect("serialize");
            let deserialized: UserUpdateTrigger =
                serde_json::from_str(&serialized).expect("deserialize");
            assert_eq!(trigger, deserialized);
        }
    }

    #[test]
    fn update_trigger_debug_output() {
        let debug_str = format!("{:?}", UserUpdateTrigger::UserCreated);
        assert!(debug_str.contains("UserCreated"));

        let debug_str = format!("{:?}", UserUpdateTrigger::MetadataChanged);
        assert!(debug_str.contains("MetadataChanged"));

        let debug_str = format!("{:?}", UserUpdateTrigger::LocalMetadataChanged);
        assert!(debug_str.contains("LocalMetadataChanged"));
    }
}
