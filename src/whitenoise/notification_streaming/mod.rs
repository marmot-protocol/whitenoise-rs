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
}
