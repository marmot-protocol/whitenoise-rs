use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::PublicKey;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// What triggered the notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationTrigger {
    /// A new message was received in a group
    NewMessage,
    /// User was invited to a new group (welcome received)
    GroupInvite,
}

/// User information for notification display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationUser {
    pub pubkey: PublicKey,
    /// Display name from metadata (display_name or name)
    pub display_name: Option<String>,
    /// Profile picture URL from metadata
    pub picture_url: Option<String>,
}

/// Represents a notification update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationUpdate {
    /// What triggered this notification
    pub trigger: NotificationTrigger,
    /// The group where the event occurred
    pub mls_group_id: GroupId,
    /// Group name (for groups). For DMs, Flutter can use sender's display_name.
    pub group_name: Option<String>,
    /// Whether this is a direct message or group chat
    pub is_dm: bool,
    /// The account that received this notification (with full metadata)
    pub receiver: NotificationUser,
    /// Who triggered the notification - message author or group inviter (with full metadata)
    pub sender: NotificationUser,
    /// Full message content (for NewMessage) or empty string (for GroupInvite)
    pub content: String,
    /// When the event occurred
    pub timestamp: DateTime<Utc>,
}

/// Subscription handle for notification updates.
///
/// Unlike ChatListSubscription and GroupMessageSubscription, this does NOT
/// include initial items - notifications are real-time only.
pub struct NotificationSubscription {
    pub updates: broadcast::Receiver<NotificationUpdate>,
}
