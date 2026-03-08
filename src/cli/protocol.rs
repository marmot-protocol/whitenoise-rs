use std::fmt;

use serde::{Deserialize, Serialize};

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
    #[serde(rename = "remove_members")]
    RemoveMembers {
        account: String,
        group_id: String,
        members: Vec<String>,
    },
    #[serde(rename = "leave_group")]
    LeaveGroup { account: String, group_id: String },
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

    // Relays
    #[serde(rename = "relays_list")]
    RelaysList { account: String },

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
    MessagesSubscribe { account: String, group_id: String },
    #[serde(rename = "chats_subscribe")]
    ChatsSubscribe { account: String },
    #[serde(rename = "archived_chats_subscribe")]
    ArchivedChatsSubscribe { account: String },
    #[serde(rename = "notifications_subscribe")]
    NotificationsSubscribe,

    // Debug
    #[serde(rename = "debug_relay_control_state")]
    DebugRelayControlState,
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
    use super::*;
    use serde_json::json;

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
    fn create_identity_roundtrip() {
        let req = Request::CreateIdentity;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"method":"create_identity"}"#);

        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::CreateIdentity));
    }

    #[test]
    fn login_start_roundtrip() {
        let req = Request::LoginStart {
            nsec: "nsec1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::LoginStart { nsec } if nsec == "nsec1abc"));
    }

    #[test]
    fn login_with_custom_relay_roundtrip() {
        let req = Request::LoginWithCustomRelay {
            pubkey: "npub1xyz".to_string(),
            relay_url: "wss://relay.example.com".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(parsed, Request::LoginWithCustomRelay { pubkey, relay_url }
                if pubkey == "npub1xyz" && relay_url == "wss://relay.example.com")
        );
    }

    #[test]
    fn all_accounts_roundtrip() {
        let req = Request::AllAccounts;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"method":"all_accounts"}"#);

        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::AllAccounts));
    }

    #[test]
    fn logout_roundtrip() {
        let req = Request::Logout {
            pubkey: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::Logout { pubkey } if pubkey == "npub1abc"));
    }

    #[test]
    fn export_nsec_roundtrip() {
        let req = Request::ExportNsec {
            pubkey: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::ExportNsec { pubkey } if pubkey == "npub1abc"));
    }

    #[test]
    fn visible_groups_roundtrip() {
        let req = Request::VisibleGroups {
            account: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::VisibleGroups { account } if account == "npub1abc"));
    }

    #[test]
    fn create_group_roundtrip() {
        let req = Request::CreateGroup {
            account: "npub1abc".to_string(),
            name: "My Group".to_string(),
            description: Some("A test group".to_string()),
            members: vec!["npub1x".to_string(), "npub1y".to_string()],
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::CreateGroup { account, name, description, members }
            if account == "npub1abc"
                && name == "My Group"
                && description.as_deref() == Some("A test group")
                && members.len() == 2
        ));
    }

    #[test]
    fn create_group_minimal_roundtrip() {
        // Only required fields — members defaults to empty, description to None
        let wire = r#"{"method":"create_group","params":{"account":"npub1abc","name":"Chat"}}"#;
        let parsed: Request = serde_json::from_str(wire).unwrap();
        assert!(matches!(
            parsed,
            Request::CreateGroup { account, name, description, members }
            if account == "npub1abc"
                && name == "Chat"
                && description.is_none()
                && members.is_empty()
        ));
    }

    #[test]
    fn add_members_roundtrip() {
        let req = Request::AddMembers {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            members: vec!["npub1x".to_string()],
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::AddMembers { account, group_id, members }
            if account == "npub1abc"
                && group_id == "abcd1234"
                && members.len() == 1
        ));
    }

    #[test]
    fn get_group_roundtrip() {
        let req = Request::GetGroup {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::GetGroup { account, group_id }
            if account == "npub1abc" && group_id == "abcd1234"
        ));
    }

    #[test]
    fn list_messages_roundtrip() {
        let req = Request::ListMessages {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            before: None,
            before_message_id: None,
            limit: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::ListMessages { account, group_id, before: None, before_message_id: None, limit: None }
            if account == "npub1abc" && group_id == "abcd1234"
        ));
    }

    #[test]
    fn list_messages_with_pagination_roundtrip() {
        let req = Request::ListMessages {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            before: Some(1_700_000_000),
            before_message_id: Some("abc123".to_string()),
            limit: Some(20),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::ListMessages { account, group_id, before: Some(1_700_000_000), before_message_id: Some(_), limit: Some(20) }
            if account == "npub1abc" && group_id == "abcd1234"
        ));
    }

    #[test]
    fn send_message_roundtrip() {
        let req = Request::SendMessage {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            message: "Hello world".to_string(),
            reply_to: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::SendMessage { account, group_id, message, reply_to }
            if account == "npub1abc" && group_id == "abcd1234" && message == "Hello world" && reply_to.is_none()
        ));
    }

    #[test]
    fn group_members_roundtrip() {
        let req = Request::GroupMembers {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::GroupMembers { account, group_id }
            if account == "npub1abc" && group_id == "abcd1234"
        ));
    }

    #[test]
    fn group_admins_roundtrip() {
        let req = Request::GroupAdmins {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::GroupAdmins { account, group_id }
            if account == "npub1abc" && group_id == "abcd1234"
        ));
    }

    #[test]
    fn remove_members_roundtrip() {
        let req = Request::RemoveMembers {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            members: vec!["npub1x".to_string()],
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::RemoveMembers { account, group_id, members }
            if account == "npub1abc" && group_id == "abcd1234" && members.len() == 1
        ));
    }

    #[test]
    fn leave_group_roundtrip() {
        let req = Request::LeaveGroup {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::LeaveGroup { account, group_id }
            if account == "npub1abc" && group_id == "abcd1234"
        ));
    }

    #[test]
    fn rename_group_roundtrip() {
        let req = Request::RenameGroup {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            name: "New Name".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::RenameGroup { account, group_id, name }
            if account == "npub1abc" && group_id == "abcd1234" && name == "New Name"
        ));
    }

    #[test]
    fn unknown_method_is_deser_error() {
        let json = r#"{"method":"nonexistent"}"#;
        assert!(serde_json::from_str::<Request>(json).is_err());
    }

    #[test]
    fn response_ok_skips_error_and_stream_end() {
        let resp = Response::ok(json!({"npub": "npub1abc"}));
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("error").is_none());
        assert!(parsed.get("stream_end").is_none());
        assert_eq!(parsed["result"]["npub"], "npub1abc");
    }

    #[test]
    fn response_err_skips_result_and_stream_end() {
        let resp = Response::err("something went wrong");
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("result").is_none());
        assert!(parsed.get("stream_end").is_none());
        assert_eq!(parsed["error"]["message"], "something went wrong");
    }

    #[test]
    fn group_invites_roundtrip() {
        let req = Request::GroupInvites {
            account: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::GroupInvites { account } if account == "npub1abc"));
    }

    #[test]
    fn accept_invite_roundtrip() {
        let req = Request::AcceptInvite {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::AcceptInvite { account, group_id }
            if account == "npub1abc" && group_id == "abcd1234"
        ));
    }

    #[test]
    fn decline_invite_roundtrip() {
        let req = Request::DeclineInvite {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::DeclineInvite { account, group_id }
            if account == "npub1abc" && group_id == "abcd1234"
        ));
    }

    #[test]
    fn profile_show_roundtrip() {
        let req = Request::ProfileShow {
            account: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::ProfileShow { account } if account == "npub1abc"));
    }

    #[test]
    fn profile_update_roundtrip() {
        let req = Request::ProfileUpdate {
            account: "npub1abc".to_string(),
            name: Some("alice".to_string()),
            display_name: Some("Alice".to_string()),
            about: None,
            picture: None,
            nip05: Some("alice@example.com".to_string()),
            lud16: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::ProfileUpdate { account, name, display_name, nip05, about, .. }
            if account == "npub1abc"
                && name.as_deref() == Some("alice")
                && display_name.as_deref() == Some("Alice")
                && nip05.as_deref() == Some("alice@example.com")
                && about.is_none()
        ));
    }

    #[test]
    fn profile_update_minimal_roundtrip() {
        let wire = r#"{"method":"profile_update","params":{"account":"npub1abc","name":"bob"}}"#;
        let parsed: Request = serde_json::from_str(wire).unwrap();
        assert!(matches!(
            parsed,
            Request::ProfileUpdate { account, name, display_name, about, picture, nip05, lud16 }
            if account == "npub1abc"
                && name.as_deref() == Some("bob")
                && display_name.is_none()
                && about.is_none()
                && picture.is_none()
                && nip05.is_none()
                && lud16.is_none()
        ));
    }

    #[test]
    fn follows_list_roundtrip() {
        let req = Request::FollowsList {
            account: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::FollowsList { account } if account == "npub1abc"));
    }

    #[test]
    fn follows_add_roundtrip() {
        let req = Request::FollowsAdd {
            account: "npub1abc".to_string(),
            pubkey: "npub1xyz".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::FollowsAdd { account, pubkey }
            if account == "npub1abc" && pubkey == "npub1xyz"
        ));
    }

    #[test]
    fn follows_remove_roundtrip() {
        let req = Request::FollowsRemove {
            account: "npub1abc".to_string(),
            pubkey: "npub1xyz".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::FollowsRemove { account, pubkey }
            if account == "npub1abc" && pubkey == "npub1xyz"
        ));
    }

    #[test]
    fn follows_check_roundtrip() {
        let req = Request::FollowsCheck {
            account: "npub1abc".to_string(),
            pubkey: "npub1xyz".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::FollowsCheck { account, pubkey }
            if account == "npub1abc" && pubkey == "npub1xyz"
        ));
    }

    #[test]
    fn chats_list_roundtrip() {
        let req = Request::ChatsList {
            account: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::ChatsList { account } if account == "npub1abc"));
    }

    #[test]
    fn settings_show_roundtrip() {
        let req = Request::SettingsShow;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"method":"settings_show"}"#);
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::SettingsShow));
    }

    #[test]
    fn settings_theme_roundtrip() {
        let req = Request::SettingsTheme {
            theme: "dark".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::SettingsTheme { theme } if theme == "dark"));
    }

    #[test]
    fn settings_language_roundtrip() {
        let req = Request::SettingsLanguage {
            language: "en".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::SettingsLanguage { language } if language == "en"));
    }

    #[test]
    fn relays_list_roundtrip() {
        let req = Request::RelaysList {
            account: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::RelaysList { account } if account == "npub1abc"));
    }

    #[test]
    fn users_show_roundtrip() {
        let req = Request::UsersShow {
            pubkey: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::UsersShow { pubkey } if pubkey == "npub1abc"));
    }

    #[test]
    fn users_search_roundtrip() {
        let req = Request::UsersSearch {
            account: "npub1abc".to_string(),
            query: "alice".to_string(),
            radius_start: 0,
            radius_end: 3,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::UsersSearch { account, query, radius_start, radius_end }
            if account == "npub1abc"
                && query == "alice"
                && radius_start == 0
                && radius_end == 3
        ));
    }

    #[test]
    fn users_search_minimal_roundtrip() {
        // Only required fields — radius defaults to 0..2
        let wire = r#"{"method":"users_search","params":{"account":"npub1abc","query":"bob"}}"#;
        let parsed: Request = serde_json::from_str(wire).unwrap();
        assert!(matches!(
            parsed,
            Request::UsersSearch { account, query, radius_start, radius_end }
            if account == "npub1abc"
                && query == "bob"
                && radius_start == 0
                && radius_end == 2
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

    #[test]
    fn chats_subscribe_roundtrip() {
        let req = Request::ChatsSubscribe {
            account: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::ChatsSubscribe { account } if account == "npub1abc"));
    }

    #[test]
    fn archive_chat_roundtrip() {
        let req = Request::ArchiveChat {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::ArchiveChat { account, group_id }
            if account == "npub1abc" && group_id == "abcd1234"
        ));
    }

    #[test]
    fn unarchive_chat_roundtrip() {
        let req = Request::UnarchiveChat {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::UnarchiveChat { account, group_id }
            if account == "npub1abc" && group_id == "abcd1234"
        ));
    }

    #[test]
    fn archived_chats_list_roundtrip() {
        let req = Request::ArchivedChatsList {
            account: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::ArchivedChatsList { account } if account == "npub1abc"));
    }

    #[test]
    fn archived_chats_subscribe_roundtrip() {
        let req = Request::ArchivedChatsSubscribe {
            account: "npub1abc".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(parsed, Request::ArchivedChatsSubscribe { account } if account == "npub1abc")
        );
    }

    #[test]
    fn messages_subscribe_roundtrip() {
        let req = Request::MessagesSubscribe {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::MessagesSubscribe { account, group_id }
            if account == "npub1abc" && group_id == "abcd1234"
        ));
    }

    #[test]
    fn react_to_message_roundtrip() {
        let req = Request::ReactToMessage {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            message_id: "eventid123".to_string(),
            emoji: "👍".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::ReactToMessage { account, group_id, message_id, emoji }
            if account == "npub1abc"
                && group_id == "abcd1234"
                && message_id == "eventid123"
                && emoji == "👍"
        ));
    }

    #[test]
    fn react_to_message_default_emoji_roundtrip() {
        let wire = r#"{"method":"react_to_message","params":{"account":"npub1abc","group_id":"abcd1234","message_id":"eventid123"}}"#;
        let parsed: Request = serde_json::from_str(wire).unwrap();
        assert!(matches!(
            parsed,
            Request::ReactToMessage { account, group_id, message_id, emoji }
            if account == "npub1abc"
                && group_id == "abcd1234"
                && message_id == "eventid123"
                && emoji == "+"
        ));
    }

    #[test]
    fn unreact_to_message_roundtrip() {
        let req = Request::UnreactToMessage {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            message_id: "eventid123".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::UnreactToMessage { account, group_id, message_id }
            if account == "npub1abc"
                && group_id == "abcd1234"
                && message_id == "eventid123"
        ));
    }

    #[test]
    fn delete_message_roundtrip() {
        let req = Request::DeleteMessage {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            message_id: "eventid123".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::DeleteMessage { account, group_id, message_id }
            if account == "npub1abc"
                && group_id == "abcd1234"
                && message_id == "eventid123"
        ));
    }

    #[test]
    fn retry_message_roundtrip() {
        let req = Request::RetryMessage {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            event_id: "eventid123".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::RetryMessage { account, group_id, event_id }
            if account == "npub1abc"
                && group_id == "abcd1234"
                && event_id == "eventid123"
        ));
    }

    #[test]
    fn send_message_with_reply_to_roundtrip() {
        let req = Request::SendMessage {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            message: "Hello".to_string(),
            reply_to: Some("eventid456".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::SendMessage { account, group_id, message, reply_to }
            if account == "npub1abc"
                && group_id == "abcd1234"
                && message == "Hello"
                && reply_to.as_deref() == Some("eventid456")
        ));
    }

    #[test]
    fn send_message_without_reply_to_roundtrip() {
        let wire = r#"{"method":"send_message","params":{"account":"npub1abc","group_id":"abcd1234","message":"Hi"}}"#;
        let parsed: Request = serde_json::from_str(wire).unwrap();
        assert!(matches!(
            parsed,
            Request::SendMessage { account, group_id, message, reply_to }
            if account == "npub1abc"
                && group_id == "abcd1234"
                && message == "Hi"
                && reply_to.is_none()
        ));
    }

    #[test]
    fn upload_media_roundtrip() {
        let req = Request::UploadMedia {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            file_path: "/tmp/image.png".to_string(),
            send: false,
            message: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::UploadMedia { account, group_id, file_path, send, message }
            if account == "npub1abc"
                && group_id == "abcd1234"
                && file_path == "/tmp/image.png"
                && !send
                && message.is_none()
        ));
    }

    #[test]
    fn upload_media_minimal_roundtrip() {
        // Old-style wire format without send/message — defaults apply
        let wire = r#"{"method":"upload_media","params":{"account":"npub1abc","group_id":"abcd1234","file_path":"/tmp/image.png"}}"#;
        let parsed: Request = serde_json::from_str(wire).unwrap();
        assert!(matches!(
            parsed,
            Request::UploadMedia { account, group_id, file_path, send, message }
            if account == "npub1abc"
                && group_id == "abcd1234"
                && file_path == "/tmp/image.png"
                && !send
                && message.is_none()
        ));
    }

    #[test]
    fn upload_media_with_send_roundtrip() {
        let req = Request::UploadMedia {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            file_path: "/tmp/image.png".to_string(),
            send: true,
            message: Some("check this out".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::UploadMedia { account, group_id, file_path, send, message }
            if account == "npub1abc"
                && group_id == "abcd1234"
                && file_path == "/tmp/image.png"
                && send
                && message.as_deref() == Some("check this out")
        ));
    }

    #[test]
    fn download_media_roundtrip() {
        let req = Request::DownloadMedia {
            account: "npub1abc".to_string(),
            group_id: "abcd1234".to_string(),
            file_hash: "abcdef1234567890".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::DownloadMedia { account, group_id, file_hash }
            if account == "npub1abc"
                && group_id == "abcd1234"
                && file_hash == "abcdef1234567890"
        ));
    }

    #[test]
    fn list_media_roundtrip() {
        let req = Request::ListMedia {
            group_id: "abcd1234".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            Request::ListMedia { group_id }
            if group_id == "abcd1234"
        ));
    }

    #[test]
    fn notifications_subscribe_roundtrip() {
        let req = Request::NotificationsSubscribe;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"method":"notifications_subscribe"}"#);
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Request::NotificationsSubscribe));
    }

    /// Verify that JSON sent from the wire (e.g. via socat) deserializes correctly.
    #[test]
    fn wire_format_compat() {
        let wire = r#"{"method":"login_start","params":{"nsec":"nsec1test"}}"#;
        let parsed: Request = serde_json::from_str(wire).unwrap();
        assert!(matches!(parsed, Request::LoginStart { nsec } if nsec == "nsec1test"));
    }
}
