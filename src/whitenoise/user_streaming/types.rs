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
