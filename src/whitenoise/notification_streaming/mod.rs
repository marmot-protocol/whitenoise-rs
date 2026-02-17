mod manager;
mod types;

pub(crate) use manager::NotificationStreamManager;
pub use types::{
    NotificationSubscription, NotificationTrigger, NotificationUpdate, NotificationUser,
};

use chrono::Utc;
use mdk_core::prelude::GroupId;
use nostr_sdk::PublicKey;

use crate::whitenoise::{
    Whitenoise,
    account_settings::AccountSettings,
    accounts::Account,
    group_information::{GroupInformation, GroupType},
    message_aggregator::ChatMessage,
    users::User,
    utils::timestamp_to_datetime,
};

impl Whitenoise {
    /// Emit a notification for a new message.
    /// Filters out messages from any of the user's own accounts.
    pub(crate) async fn emit_new_message_notification(
        &self,
        account: &Account,
        group_id: &GroupId,
        message: &ChatMessage,
        group_name: Option<String>,
    ) {
        if !self.notification_stream_manager.has_subscribers() {
            return;
        }

        if self.is_own_account(&message.author).await {
            return;
        }

        let is_dm = GroupInformation::find_by_mls_group_id(group_id, &self.database)
            .await
            .ok()
            .map(|gi| gi.group_type == GroupType::DirectMessage)
            .unwrap_or(false);

        let receiver = self.build_notification_user(&account.pubkey).await;
        let sender = self.build_notification_user(&message.author).await;
        let timestamp = timestamp_to_datetime(message.created_at).unwrap_or_else(|_| Utc::now());

        let update = NotificationUpdate {
            trigger: NotificationTrigger::NewMessage,
            mls_group_id: group_id.clone(),
            group_name,
            is_dm,
            receiver,
            sender,
            content: message.content.clone(),
            timestamp,
        };

        self.notification_stream_manager.emit(update);
    }

    pub(crate) async fn emit_group_invite_notification(
        &self,
        account: &Account,
        group_id: &GroupId,
        group_name: &str,
        welcomer_pubkey: PublicKey,
    ) {
        if !self.notification_stream_manager.has_subscribers() {
            return;
        }

        let is_dm = GroupInformation::find_by_mls_group_id(group_id, &self.database)
            .await
            .ok()
            .map(|group_info| group_info.group_type == GroupType::DirectMessage)
            .unwrap_or(false);

        let receiver = self.build_notification_user(&account.pubkey).await;
        let sender = self.build_notification_user(&welcomer_pubkey).await;

        let update = NotificationUpdate {
            trigger: NotificationTrigger::GroupInvite,
            mls_group_id: group_id.clone(),
            group_name: Some(group_name.to_string()),
            is_dm,
            receiver,
            sender,
            content: String::new(), // No content for invites
            timestamp: Utc::now(),
        };

        self.notification_stream_manager.emit(update);
    }

    /// Emits a new-message notification only if notifications are enabled for the account.
    ///
    /// Fail-open: if the settings lookup fails, defaults to enabled and logs a warning.
    pub(crate) async fn emit_new_message_notification_if_enabled(
        &self,
        account: &Account,
        group_id: &GroupId,
        message: &ChatMessage,
        group_name: Option<String>,
    ) {
        if !self.are_notifications_enabled(account).await {
            return;
        }
        self.emit_new_message_notification(account, group_id, message, group_name)
            .await;
    }

    /// Emits a group-invite notification only if notifications are enabled for the account.
    ///
    /// Fail-open: if the settings lookup fails, defaults to enabled and logs a warning.
    pub(crate) async fn emit_group_invite_notification_if_enabled(
        &self,
        account: &Account,
        group_id: &GroupId,
        group_name: &str,
        welcomer_pubkey: PublicKey,
    ) {
        if !self.are_notifications_enabled(account).await {
            return;
        }
        self.emit_group_invite_notification(account, group_id, group_name, welcomer_pubkey)
            .await;
    }

    /// Spawns a background task that emits a new-message notification if enabled.
    ///
    /// The caller is not blocked; settings are checked inside the spawned task.
    pub(crate) fn spawn_new_message_notification_if_enabled(
        account: &Account,
        group_id: &GroupId,
        message: &ChatMessage,
        group_name: Option<String>,
    ) {
        let account = account.clone();
        let group_id = group_id.clone();
        let message = message.clone();
        tokio::spawn(async move {
            let whitenoise = match Self::get_instance() {
                Ok(wn) => wn,
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::notification_streaming",
                        "Failed to get Whitenoise instance for notification: {}",
                        e
                    );
                    return;
                }
            };
            whitenoise
                .emit_new_message_notification_if_enabled(&account, &group_id, &message, group_name)
                .await;
        });
    }

    /// Returns whether notifications are enabled for `account`. Fail-open on error.
    async fn are_notifications_enabled(&self, account: &Account) -> bool {
        AccountSettings::notifications_enabled_for_pubkey(&account.pubkey, &self.database)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    target: "whitenoise::notification_streaming",
                    "Failed to check notification settings for {}, defaulting to enabled: {}",
                    account.pubkey.to_hex(),
                    e
                );
                true
            })
    }

    async fn build_notification_user(&self, pubkey: &PublicKey) -> NotificationUser {
        let user = User::find_by_pubkey(pubkey, &self.database).await.ok();

        let (display_name, picture_url) = user
            .map(|u| {
                let name = u
                    .metadata
                    .display_name
                    .filter(|s| !s.is_empty())
                    .or(u.metadata.name.filter(|s| !s.is_empty()));
                let picture = u.metadata.picture.map(|url| url.to_string());
                (name, picture)
            })
            .unwrap_or((None, None));

        NotificationUser {
            pubkey: *pubkey,
            display_name,
            picture_url,
        }
    }

    async fn is_own_account(&self, pubkey: &PublicKey) -> bool {
        Account::find_by_pubkey(pubkey, &self.database)
            .await
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::message_aggregator::ReactionSummary;
    use crate::whitenoise::test_utils::*;
    use nostr_sdk::{Keys, Tags, Timestamp};

    fn create_test_message(author: PublicKey, content: &str) -> ChatMessage {
        ChatMessage {
            id: format!("test-msg-{}", rand::random::<u32>()),
            author,
            content: content.to_string(),
            created_at: Timestamp::now(),
            tags: Tags::new(),
            is_reply: false,
            reply_to_id: None,
            is_deleted: false,
            content_tokens: vec![],
            reactions: ReactionSummary::default(),
            kind: 9, // MLS message kind
            media_attachments: vec![],
        }
    }

    #[tokio::test]
    async fn test_emit_new_message_notification_filters_own_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Subscribe to get notifications
        let mut rx = whitenoise.notification_stream_manager.subscribe();

        // Create a message from the account itself (should be filtered)
        let message = create_test_message(account.pubkey, "Hello");

        let group_id = GroupId::from_slice(&[1u8; 32]);
        whitenoise
            .emit_new_message_notification(&account, &group_id, &message, Some("Test".to_string()))
            .await;

        // Should NOT receive notification (filtered as own account)
        let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            result.is_err(),
            "Should not receive notification for own message"
        );
    }

    #[tokio::test]
    async fn test_emit_new_message_notification_emits_for_external_sender() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Subscribe to get notifications
        let mut rx = whitenoise.notification_stream_manager.subscribe();

        // Create a message from an external user (not in database as account)
        let external_sender = Keys::generate().public_key();
        let message = create_test_message(external_sender, "Hello from external");

        let group_id = GroupId::from_slice(&[2u8; 32]);
        whitenoise
            .emit_new_message_notification(
                &account,
                &group_id,
                &message,
                Some("Test Group".to_string()),
            )
            .await;

        // Should receive notification
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        assert!(
            result.is_ok(),
            "Should receive notification for external message"
        );

        let update = result.unwrap().unwrap();
        assert_eq!(update.trigger, NotificationTrigger::NewMessage);
        assert_eq!(update.mls_group_id, group_id);
        assert_eq!(update.group_name, Some("Test Group".to_string()));
        assert_eq!(update.content, "Hello from external");
        assert_eq!(update.receiver.pubkey, account.pubkey);
        assert_eq!(update.sender.pubkey, external_sender);
    }

    #[tokio::test]
    async fn test_emit_new_message_notification_no_emit_without_subscribers() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Do NOT subscribe - no subscribers

        let external_sender = Keys::generate().public_key();
        let message = create_test_message(external_sender, "Hello");

        let group_id = GroupId::from_slice(&[3u8; 32]);

        // This should not panic even without subscribers
        whitenoise
            .emit_new_message_notification(&account, &group_id, &message, None)
            .await;
    }

    #[tokio::test]
    async fn test_emit_group_invite_notification_emits_with_subscriber() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Subscribe to get notifications
        let mut rx = whitenoise.notification_stream_manager.subscribe();

        let welcomer = Keys::generate().public_key();
        let group_id = GroupId::from_slice(&[4u8; 32]);

        whitenoise
            .emit_group_invite_notification(&account, &group_id, "New Group", welcomer)
            .await;

        // Should receive notification
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        assert!(result.is_ok(), "Should receive group invite notification");

        let update = result.unwrap().unwrap();
        assert_eq!(update.trigger, NotificationTrigger::GroupInvite);
        assert_eq!(update.mls_group_id, group_id);
        assert_eq!(update.group_name, Some("New Group".to_string()));
        assert!(update.content.is_empty());
        assert_eq!(update.receiver.pubkey, account.pubkey);
        assert_eq!(update.sender.pubkey, welcomer);
    }

    #[tokio::test]
    async fn test_emit_group_invite_notification_no_emit_without_subscribers() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Do NOT subscribe - no subscribers

        let welcomer = Keys::generate().public_key();
        let group_id = GroupId::from_slice(&[5u8; 32]);

        // This should not panic even without subscribers
        whitenoise
            .emit_group_invite_notification(&account, &group_id, "Test Group", welcomer)
            .await;
    }

    #[tokio::test]
    async fn test_build_notification_user_with_unknown_user() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let unknown_pubkey = Keys::generate().public_key();
        let user = whitenoise.build_notification_user(&unknown_pubkey).await;

        assert_eq!(user.pubkey, unknown_pubkey);
        assert!(user.display_name.is_none());
        assert!(user.picture_url.is_none());
    }

    #[tokio::test]
    async fn test_build_notification_user_with_known_user() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an account (which also creates a user entry)
        let account = whitenoise.create_identity().await.unwrap();

        let user = whitenoise.build_notification_user(&account.pubkey).await;

        assert_eq!(user.pubkey, account.pubkey);
        // User exists but may not have metadata set
    }

    #[tokio::test]
    async fn test_is_own_account_returns_true_for_logged_in_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        assert!(whitenoise.is_own_account(&account.pubkey).await);
    }

    #[tokio::test]
    async fn test_is_own_account_returns_false_for_external_pubkey() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let external_pubkey = Keys::generate().public_key();
        assert!(!whitenoise.is_own_account(&external_pubkey).await);
    }

    #[tokio::test]
    async fn test_emit_new_message_notification_if_enabled_emits_when_enabled() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let mut rx = whitenoise.notification_stream_manager.subscribe();

        let external_sender = Keys::generate().public_key();
        let message = create_test_message(external_sender, "Hello enabled");
        let group_id = GroupId::from_slice(&[10u8; 32]);

        // Notifications enabled by default
        whitenoise
            .emit_new_message_notification_if_enabled(
                &account,
                &group_id,
                &message,
                Some("Enabled Group".to_string()),
            )
            .await;

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        assert!(result.is_ok(), "Should emit when notifications enabled");
        let update = result.unwrap().unwrap();
        assert_eq!(update.trigger, NotificationTrigger::NewMessage);
        assert_eq!(update.content, "Hello enabled");
    }

    #[tokio::test]
    async fn test_emit_new_message_notification_if_enabled_suppressed_when_disabled() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let mut rx = whitenoise.notification_stream_manager.subscribe();

        // Disable notifications
        AccountSettings::update_notifications_enabled(&account.pubkey, false, &whitenoise.database)
            .await
            .unwrap();

        let external_sender = Keys::generate().public_key();
        let message = create_test_message(external_sender, "Should not arrive");
        let group_id = GroupId::from_slice(&[11u8; 32]);

        whitenoise
            .emit_new_message_notification_if_enabled(&account, &group_id, &message, None)
            .await;

        let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            result.is_err(),
            "Should NOT emit when notifications disabled"
        );
    }

    #[tokio::test]
    async fn test_emit_group_invite_notification_if_enabled_emits_when_enabled() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let mut rx = whitenoise.notification_stream_manager.subscribe();

        let welcomer = Keys::generate().public_key();
        let group_id = GroupId::from_slice(&[12u8; 32]);

        whitenoise
            .emit_group_invite_notification_if_enabled(
                &account,
                &group_id,
                "Invite Group",
                welcomer,
            )
            .await;

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;
        assert!(
            result.is_ok(),
            "Should emit invite when notifications enabled"
        );
        let update = result.unwrap().unwrap();
        assert_eq!(update.trigger, NotificationTrigger::GroupInvite);
        assert_eq!(update.group_name, Some("Invite Group".to_string()));
    }

    #[tokio::test]
    async fn test_emit_group_invite_notification_if_enabled_suppressed_when_disabled() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let mut rx = whitenoise.notification_stream_manager.subscribe();

        // Disable notifications
        AccountSettings::update_notifications_enabled(&account.pubkey, false, &whitenoise.database)
            .await
            .unwrap();

        let welcomer = Keys::generate().public_key();
        let group_id = GroupId::from_slice(&[13u8; 32]);

        whitenoise
            .emit_group_invite_notification_if_enabled(
                &account,
                &group_id,
                "Should Not Arrive",
                welcomer,
            )
            .await;

        let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            result.is_err(),
            "Should NOT emit invite when notifications disabled"
        );
    }
}
