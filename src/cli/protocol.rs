use std::fmt;

use serde::{Deserialize, Serialize};

pub use crate::whitenoise::accounts_groups::MuteDuration;

/// A request from the CLI client to the daemon.
///
/// Each variant maps to one daemon method. The JSON wire format uses
/// `{"method": "variant_name", "params": {...}}` via serde's tagged enum.
#[derive(Deserialize, Serialize)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    // Daemon management
    #[serde(rename = "ping")]
    Ping,

    // Identity & auth
    #[serde(rename = "create_identity")]
    CreateIdentity,
    #[serde(rename = "login_start")]
    LoginStart { nsec: String },
    #[serde(rename = "login_publish_default_relays")]
    LoginPublishDefaultRelays { pubkey: String },
    #[serde(rename = "login_with_custom_relay")]
    LoginWithCustomRelay { pubkey: String, relay_url: String },
    #[serde(rename = "login_cancel")]
    LoginCancel { pubkey: String },
    #[serde(rename = "logout")]
    Logout { pubkey: String },

    // Accounts
    #[serde(rename = "all_accounts")]
    AllAccounts,
    #[serde(rename = "export_nsec")]
    ExportNsec { pubkey: String },

    // Groups
    #[serde(rename = "visible_groups")]
    VisibleGroups { account: String },
    #[serde(rename = "create_group")]
    CreateGroup {
        account: String,
        name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        members: Vec<String>,
    },
    #[serde(rename = "add_members")]
    AddMembers {
        account: String,
        group_id: String,
        members: Vec<String>,
    },
    #[serde(rename = "get_group")]
    GetGroup { account: String, group_id: String },

    #[serde(rename = "group_members")]
    GroupMembers { account: String, group_id: String },
    #[serde(rename = "group_admins")]
    GroupAdmins { account: String, group_id: String },
    #[serde(rename = "group_relays")]
    GroupRelays { account: String, group_id: String },
    #[serde(rename = "remove_members")]
    RemoveMembers {
        account: String,
        group_id: String,
        members: Vec<String>,
    },
    #[serde(rename = "leave_group")]
    LeaveGroup { account: String, group_id: String },
    #[serde(rename = "self_demote")]
    SelfDemote { account: String, group_id: String },
    #[serde(rename = "rename_group")]
    RenameGroup {
        account: String,
        group_id: String,
        name: String,
    },
    #[serde(rename = "group_invites")]
    GroupInvites { account: String },
    #[serde(rename = "accept_invite")]
    AcceptInvite { account: String, group_id: String },
    #[serde(rename = "decline_invite")]
    DeclineInvite { account: String, group_id: String },

    // Follows
    #[serde(rename = "follows_list")]
    FollowsList { account: String },
    #[serde(rename = "follows_add")]
    FollowsAdd { account: String, pubkey: String },
    #[serde(rename = "follows_remove")]
    FollowsRemove { account: String, pubkey: String },
    #[serde(rename = "follows_check")]
    FollowsCheck { account: String, pubkey: String },

    // Profile
    #[serde(rename = "profile_show")]
    ProfileShow { account: String },
    #[serde(rename = "profile_update")]
    ProfileUpdate {
        account: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        display_name: Option<String>,
        #[serde(default)]
        about: Option<String>,
        #[serde(default)]
        picture: Option<String>,
        #[serde(default)]
        nip05: Option<String>,
        #[serde(default)]
        lud16: Option<String>,
    },

    // Chat list
    #[serde(rename = "chats_list")]
    ChatsList { account: String },
    #[serde(rename = "archive_chat")]
    ArchiveChat { account: String, group_id: String },
    #[serde(rename = "unarchive_chat")]
    UnarchiveChat { account: String, group_id: String },
    #[serde(rename = "archived_chats_list")]
    ArchivedChatsList { account: String },
    #[serde(rename = "mute_chat")]
    MuteChat {
        account: String,
        group_id: String,
        duration: MuteDuration,
    },
    #[serde(rename = "unmute_chat")]
    UnmuteChat { account: String, group_id: String },

    // Settings
    #[serde(rename = "settings_show")]
    SettingsShow,
    #[serde(rename = "settings_theme")]
    SettingsTheme { theme: String },
    #[serde(rename = "settings_language")]
    SettingsLanguage { language: String },

    // Users
    #[serde(rename = "users_show")]
    UsersShow { pubkey: String },
    #[serde(rename = "users_search")]
    UsersSearch {
        account: String,
        query: String,
        #[serde(default = "default_radius_start")]
        radius_start: u8,
        #[serde(default = "default_radius_end")]
        radius_end: u8,
    },

    // Block / Unblock
    #[serde(rename = "block_user")]
    BlockUser { account: String, pubkey: String },
    #[serde(rename = "unblock_user")]
    UnblockUser { account: String, pubkey: String },
    #[serde(rename = "blocked_users")]
    BlockedUsers { account: String },

    // Relays
    #[serde(rename = "relays_list")]
    RelaysList {
        account: String,
        #[serde(default)]
        relay_type: Option<String>,
    },
    #[serde(rename = "relays_add")]
    RelaysAdd {
        account: String,
        url: String,
        relay_type: String,
    },
    #[serde(rename = "relays_remove")]
    RelaysRemove {
        account: String,
        url: String,
        relay_type: String,
    },

    // Messages
    #[serde(rename = "list_messages")]
    ListMessages {
        account: String,
        group_id: String,
        /// Cursor timestamp: fetch messages created before this Unix timestamp (seconds).
        /// Omit (or pass null) for the most-recent page.
        #[serde(default)]
        before: Option<u64>,
        /// Companion cursor ID: the `id` of the oldest message in the current page.
        /// Pair with `before` so that ties at the same second are resolved deterministically.
        #[serde(default)]
        before_message_id: Option<String>,
        /// Cursor timestamp: fetch messages created after this Unix timestamp (seconds).
        /// Omit (or pass null) for the most-recent page.
        #[serde(default)]
        after: Option<u64>,
        /// Companion cursor ID: the `id` of the newest message in the current page.
        /// Pair with `after` so that ties at the same second are resolved deterministically.
        #[serde(default)]
        after_message_id: Option<String>,
        /// Maximum number of messages to return. Defaults to 50 when absent, capped at 200.
        #[serde(default)]
        limit: Option<u32>,
    },
    #[serde(rename = "send_message")]
    SendMessage {
        account: String,
        group_id: String,
        message: String,
        #[serde(default)]
        reply_to: Option<String>,
    },
    #[serde(rename = "delete_message")]
    DeleteMessage {
        account: String,
        group_id: String,
        message_id: String,
    },
    #[serde(rename = "retry_message")]
    RetryMessage {
        account: String,
        group_id: String,
        event_id: String,
    },
    #[serde(rename = "react_to_message")]
    ReactToMessage {
        account: String,
        group_id: String,
        message_id: String,
        #[serde(default = "default_reaction_emoji")]
        emoji: String,
    },
    #[serde(rename = "unreact_to_message")]
    UnreactToMessage {
        account: String,
        group_id: String,
        message_id: String,
    },

    #[serde(rename = "search_messages")]
    SearchMessages {
        account: String,
        group_id: String,
        query: String,
        #[serde(default)]
        limit: Option<u32>,
    },

    #[serde(rename = "search_all_messages")]
    SearchAllMessages {
        account: String,
        query: String,
        #[serde(default)]
        limit: Option<u32>,
    },

    // Media
    #[serde(rename = "upload_media")]
    UploadMedia {
        account: String,
        group_id: String,
        file_path: String,
        /// When true, send a kind-9 message with the imeta tag after upload.
        #[serde(default)]
        send: bool,
        /// Optional caption text for the message (only used when `send` is true).
        #[serde(default)]
        message: Option<String>,
    },
    #[serde(rename = "download_media")]
    DownloadMedia {
        account: String,
        group_id: String,
        file_hash: String,
    },
    #[serde(rename = "list_media")]
    ListMedia { group_id: String },

    // Streaming
    #[serde(rename = "messages_subscribe")]
    MessagesSubscribe {
        account: String,
        group_id: String,
        /// Maximum number of messages to include in the initial snapshot.
        /// Defaults to 50 when absent, capped at 200.
        #[serde(default)]
        limit: Option<u32>,
    },
    #[serde(rename = "chats_subscribe")]
    ChatsSubscribe { account: String },
    #[serde(rename = "archived_chats_subscribe")]
    ArchivedChatsSubscribe { account: String },
    #[serde(rename = "notifications_subscribe")]
    NotificationsSubscribe,

    // Debug
    #[serde(rename = "debug_relay_control_state")]
    DebugRelayControlState,
    #[serde(rename = "debug_health")]
    DebugHealth { account: String },
    #[serde(rename = "debug_ratchet_tree")]
    DebugRatchetTree { account: String, group_id: String },

    // Reset
    #[serde(rename = "delete_all_data")]
    DeleteAllData,

    // Groups — admin management
    #[serde(rename = "group_promote")]
    GroupPromote {
        account: String,
        group_id: String,
        pubkey: String,
    },
    #[serde(rename = "group_demote")]
    GroupDemote {
        account: String,
        group_id: String,
        pubkey: String,
    },

    // Key packages
    #[serde(rename = "keys_list")]
    KeysList { account: String },
    #[serde(rename = "keys_publish")]
    KeysPublish { account: String },
    #[serde(rename = "keys_delete")]
    KeysDelete { account: String, event_id: String },
    #[serde(rename = "keys_delete_all")]
    KeysDeleteAll { account: String },
    #[serde(rename = "keys_check")]
    KeysCheck { pubkey: String },
}

fn default_radius_start() -> u8 {
    0
}

fn default_radius_end() -> u8 {
    2
}

fn default_reaction_emoji() -> String {
    "+".to_string()
}

/// Manual `Debug` impl to prevent accidental nsec exposure in logs.
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoginStart { .. } => f
                .debug_struct("LoginStart")
                .field("nsec", &"[REDACTED]")
                .finish(),
            other => write!(f, "{}", serde_json::to_string(other).unwrap_or_default()),
        }
    }
}

impl Request {
    /// Returns true if this request expects a streaming response (multiple lines).
    pub fn is_streaming(&self) -> bool {
        matches!(
            self,
            Self::MessagesSubscribe { .. }
                | Self::ChatsSubscribe { .. }
                | Self::ArchivedChatsSubscribe { .. }
                | Self::NotificationsSubscribe
                | Self::UsersSearch { .. }
        )
    }
}

/// A response from the daemon to the CLI client.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
    /// Reserved for streaming responses (Phase 3). When true, signals no more
    /// messages will follow for this subscription.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub stream_end: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub message: String,
}

impl Response {
    pub fn ok(result: serde_json::Value) -> Self {
        Self {
            result: Some(result),
            error: None,
            stream_end: false,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            result: None,
            error: Some(ErrorPayload {
                message: message.into(),
            }),
            stream_end: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::*;

    /// Unit variants must serialize without a "params" key.
    #[test]
    fn unit_variant_roundtrip() {
        let req = Request::Ping;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"method":"ping"}"#);

        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::Ping));
    }

    #[test]
    fn relays_list_without_type_field() {
        let wire = r#"{"method":"relays_list","params":{"account":"npub1abc"}}"#;
        let parsed: Request = serde_json::from_str(wire).unwrap();
        assert!(matches!(
            parsed,
            Request::RelaysList { account, relay_type }
            if account == "npub1abc" && relay_type.is_none()
        ));
    }

    #[test]
    fn users_search_is_streaming() {
        let req = Request::UsersSearch {
            account: "npub1abc".to_string(),
            query: "test".to_string(),
            radius_start: 0,
            radius_end: 2,
        };
        assert!(req.is_streaming());
    }

    /// Older clients that do not send `limit` in the JSON payload must still deserialise
    /// successfully, with `limit` defaulting to `None` via `#[serde(default)]`.
    #[test]
    fn messages_subscribe_missing_limit_field_deserialises_as_none() {
        // Wire format without the `limit` key — simulates an old client
        let wire = r#"{"method":"messages_subscribe","params":{"account":"npub1abc","group_id":"abcd1234"}}"#;
        let parsed: Request = serde_json::from_str(wire).unwrap();
        assert!(
            matches!(parsed, Request::MessagesSubscribe { limit: None, .. }),
            "missing 'limit' key must deserialise as None for backward compatibility"
        );
    }

    /// `limit: null` in the JSON payload must also deserialise as `None`.
    #[test]
    fn messages_subscribe_explicit_null_limit_deserialises_as_none() {
        let wire = r#"{"method":"messages_subscribe","params":{"account":"npub1abc","group_id":"abcd1234","limit":null}}"#;
        let parsed: Request = serde_json::from_str(wire).unwrap();
        assert!(matches!(
            parsed,
            Request::MessagesSubscribe { limit: None, .. }
        ));
    }

    #[test]
    fn search_messages_wire_missing_limit_deserialises_as_none() {
        // Incoming JSON without the `limit` key must deserialise as None via #[serde(default)]
        let wire = r#"{"method":"search_messages","params":{"account":"npub1abc","group_id":"group123","query":"marmot"}}"#;
        let parsed_wire: Request = serde_json::from_str(wire).unwrap();
        assert!(matches!(
            parsed_wire,
            Request::SearchMessages { limit, .. }
            if limit.is_none()
        ));
    }

    #[test]
    fn mute_duration_to_expiry_one_hour_is_future() {
        let before = Utc::now();
        let expiry = MuteDuration::OneHour.to_expiry();
        let after = Utc::now() + Duration::hours(1) + Duration::seconds(1);

        assert!(expiry > before);
        assert!(expiry < after);
    }

    #[test]
    fn mute_duration_to_expiry_custom_returns_inner_value() {
        let custom_time = Utc::now() + Duration::days(3);
        let expiry = MuteDuration::Custom(custom_time).to_expiry();
        assert_eq!(expiry, custom_time);
    }

    #[test]
    fn mute_duration_display_custom() {
        let custom_time = Utc::now() + Duration::days(3);
        let display = MuteDuration::Custom(custom_time).to_string();
        assert!(display.starts_with("custom("));
        assert!(display.ends_with(')'));
    }

    #[test]
    fn mute_duration_serde_custom_roundtrip() {
        let custom_time = Utc::now() + Duration::days(3);
        let duration = MuteDuration::Custom(custom_time);
        let serialized = serde_json::to_string(&duration).unwrap();
        let deserialized: MuteDuration = serde_json::from_str(&serialized).unwrap();
        assert_eq!(duration, deserialized);
    }

    #[test]
    fn block_user_roundtrip() {
        let req = Request::BlockUser {
            account: "npub1abc".to_string(),
            pubkey: "npub1xyz".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::BlockUser { account, pubkey }
                if account == "npub1abc" && pubkey == "npub1xyz"));
    }

    #[test]
    fn unblock_user_roundtrip() {
        let req = Request::UnblockUser {
            account: "npub1abc".to_string(),
            pubkey: "npub1xyz".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::UnblockUser { account, pubkey }
                if account == "npub1abc" && pubkey == "npub1xyz"));
    }

    #[test]
    fn blocked_users_roundtrip() {
        let req = Request::BlockedUsers {
            account: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::BlockedUsers { account } if account == "npub1abc"));
    }
}
