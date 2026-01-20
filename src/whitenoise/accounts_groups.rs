use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::whitenoise::{
    Whitenoise, accounts::Account, aggregated_message::AggregatedMessage, error::WhitenoiseError,
};

/// Represents the relationship between an account and an MLS group.
///
/// This struct tracks whether a user has accepted or declined a group invite.
/// When a welcome message is received, an AccountGroup is created with
/// `user_confirmation = None` (pending). The user can then accept or decline.
///
/// Confirmation states:
/// - `None` = pending (auto-joined but awaiting user decision)
/// - `Some(true)` = accepted
/// - `Some(false)` = declined (hidden from UI)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountGroup {
    pub id: Option<i64>,
    pub account_pubkey: PublicKey,
    pub mls_group_id: GroupId,
    pub user_confirmation: Option<bool>,
    pub welcomer_pubkey: Option<PublicKey>,
    pub last_read_message_id: Option<EventId>,
    /// Pin order for chat list sorting.
    /// - `None` = not pinned (appears after pinned chats)
    /// - `Some(n)` = pinned, lower values appear first
    pub pin_order: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl AccountGroup {
    /// Returns true if this group should be visible to the user.
    /// Visible means: pending (NULL) or accepted (true).
    /// Declined groups (false) are hidden.
    pub fn is_visible(&self) -> bool {
        self.user_confirmation != Some(false)
    }

    /// Returns true if this group is pending user confirmation.
    pub fn is_pending(&self) -> bool {
        self.user_confirmation.is_none()
    }

    /// Returns true if the user has accepted this group.
    pub fn is_accepted(&self) -> bool {
        self.user_confirmation == Some(true)
    }

    /// Returns true if the user has declined this group.
    pub fn is_declined(&self) -> bool {
        self.user_confirmation == Some(false)
    }

    /// Creates or retrieves an AccountGroup for the given account and group.
    /// New records are created with user_confirmation = None (pending).
    pub async fn get_or_create(
        whitenoise: &Whitenoise,
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
    ) -> Result<(Self, bool), WhitenoiseError> {
        let (account_group, was_created) =
            Self::find_or_create(account_pubkey, mls_group_id, &whitenoise.database).await?;
        Ok((account_group, was_created))
    }

    /// Gets an AccountGroup for the given account and group, if it exists.
    pub async fn get(
        whitenoise: &Whitenoise,
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
    ) -> Result<Option<Self>, WhitenoiseError> {
        let account_group =
            Self::find_by_account_and_group(account_pubkey, mls_group_id, &whitenoise.database)
                .await?;
        Ok(account_group)
    }

    /// Gets all visible AccountGroups for the given account.
    /// Visible means: pending or accepted (not declined).
    pub async fn visible_for_account(
        whitenoise: &Whitenoise,
        account_pubkey: &PublicKey,
    ) -> Result<Vec<Self>, WhitenoiseError> {
        let groups = Self::find_visible_for_account(account_pubkey, &whitenoise.database).await?;
        Ok(groups)
    }

    /// Gets all pending AccountGroups for the given account.
    pub async fn pending_for_account(
        whitenoise: &Whitenoise,
        account_pubkey: &PublicKey,
    ) -> Result<Vec<Self>, WhitenoiseError> {
        let groups = Self::find_pending_for_account(account_pubkey, &whitenoise.database).await?;
        Ok(groups)
    }

    /// Accepts this group invite by setting user_confirmation to true.
    pub async fn accept(&self, whitenoise: &Whitenoise) -> Result<Self, WhitenoiseError> {
        let updated = self
            .update_user_confirmation(true, &whitenoise.database)
            .await?;
        Ok(updated)
    }

    /// Declines this group invite by setting user_confirmation to false.
    /// The group will be hidden from the UI but remains in MLS.
    pub async fn decline(&self, whitenoise: &Whitenoise) -> Result<Self, WhitenoiseError> {
        let updated = self
            .update_user_confirmation(false, &whitenoise.database)
            .await?;
        Ok(updated)
    }
}

impl Whitenoise {
    /// Gets or creates an AccountGroup for the given account and MLS group.
    pub async fn get_or_create_account_group(
        &self,
        account: &Account,
        mls_group_id: &GroupId,
    ) -> Result<(AccountGroup, bool), WhitenoiseError> {
        AccountGroup::get_or_create(self, &account.pubkey, mls_group_id).await
    }

    /// Gets all visible AccountGroups for the given account.
    pub async fn get_visible_account_groups(
        &self,
        account: &Account,
    ) -> Result<Vec<AccountGroup>, WhitenoiseError> {
        AccountGroup::visible_for_account(self, &account.pubkey).await
    }

    /// Gets all pending AccountGroups for the given account.
    pub async fn get_pending_account_groups(
        &self,
        account: &Account,
    ) -> Result<Vec<AccountGroup>, WhitenoiseError> {
        AccountGroup::pending_for_account(self, &account.pubkey).await
    }

    /// Accepts a group invite for the given account and MLS group.
    pub async fn accept_account_group(
        &self,
        account: &Account,
        mls_group_id: &GroupId,
    ) -> Result<AccountGroup, WhitenoiseError> {
        let account_group = AccountGroup::get(self, &account.pubkey, mls_group_id)
            .await?
            .ok_or(WhitenoiseError::GroupNotFound)?;
        account_group.accept(self).await
    }

    /// Declines a group invite for the given account and MLS group.
    pub async fn decline_account_group(
        &self,
        account: &Account,
        mls_group_id: &GroupId,
    ) -> Result<AccountGroup, WhitenoiseError> {
        let account_group = AccountGroup::get(self, &account.pubkey, mls_group_id)
            .await?
            .ok_or(WhitenoiseError::GroupNotFound)?;
        account_group.decline(self).await
    }

    /// Marks a message as read for the given account.
    ///
    /// Looks up the message to find its group, then atomically updates the
    /// last_read_message_id only if the new message is newer than the current
    /// read marker. This prevents regression when messages arrive out of order
    /// or when the UI marks an older message as read after a newer one.
    pub async fn mark_message_read(
        &self,
        account: &Account,
        message_id: &EventId,
    ) -> Result<AccountGroup, WhitenoiseError> {
        let message = AggregatedMessage::find_by_message_id(message_id, &self.database)
            .await?
            .ok_or(WhitenoiseError::MessageNotFound)?;

        let account_group = AccountGroup::get(self, &account.pubkey, &message.mls_group_id)
            .await?
            .ok_or(WhitenoiseError::GroupNotFound)?;

        // Atomic compare-and-update: only advances if newer
        let message_created_at_ms = message.created_at.timestamp_millis();
        if let Some(updated) = account_group
            .update_last_read_if_newer(message_id, message_created_at_ms, &self.database)
            .await?
        {
            return Ok(updated);
        }

        // Update was skipped (message not newer), return current state
        Ok(account_group)
    }

    /// Gets the last read message ID for an account in a group.
    pub async fn get_last_read_message_id(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Option<EventId>, WhitenoiseError> {
        let account_group = AccountGroup::get(self, &account.pubkey, group_id).await?;
        Ok(account_group.and_then(|ag| ag.last_read_message_id))
    }

    /// Sets the pin order for a chat.
    ///
    /// - `None` = unpin the chat
    /// - `Some(n)` = pin the chat with order n (lower values appear first)
    pub async fn set_chat_pin_order(
        &self,
        account: &Account,
        mls_group_id: &GroupId,
        pin_order: Option<i64>,
    ) -> Result<AccountGroup, WhitenoiseError> {
        let account_group = AccountGroup::get(self, &account.pubkey, mls_group_id)
            .await?
            .ok_or(WhitenoiseError::GroupNotFound)?;

        let updated = account_group
            .update_pin_order(pin_order, &self.database)
            .await?;

        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    #[tokio::test]
    async fn test_account_group_visibility_methods() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[1; 32]);

        // Create a pending group
        let (pending_group, _) = whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();

        assert!(pending_group.is_visible());
        assert!(pending_group.is_pending());
        assert!(!pending_group.is_accepted());
        assert!(!pending_group.is_declined());

        // Accept it
        let accepted_group = pending_group.accept(&whitenoise).await.unwrap();

        assert!(accepted_group.is_visible());
        assert!(!accepted_group.is_pending());
        assert!(accepted_group.is_accepted());
        assert!(!accepted_group.is_declined());
    }

    #[tokio::test]
    async fn test_account_group_decline() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[2; 32]);

        let (pending_group, _) = whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();

        let declined_group = pending_group.decline(&whitenoise).await.unwrap();

        assert!(!declined_group.is_visible());
        assert!(!declined_group.is_pending());
        assert!(!declined_group.is_accepted());
        assert!(declined_group.is_declined());
    }

    #[tokio::test]
    async fn test_whitenoise_accept_account_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[3; 32]);

        // Create pending group
        whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();

        // Accept via Whitenoise method
        let accepted = whitenoise
            .accept_account_group(&account, &group_id)
            .await
            .unwrap();

        assert!(accepted.is_accepted());
    }

    #[tokio::test]
    async fn test_whitenoise_decline_account_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[4; 32]);

        // Create pending group
        whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();

        // Decline via Whitenoise method
        let declined = whitenoise
            .decline_account_group(&account, &group_id)
            .await
            .unwrap();

        assert!(declined.is_declined());
    }

    #[tokio::test]
    async fn test_get_visible_account_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let group_id1 = GroupId::from_slice(&[5; 32]); // pending
        let group_id2 = GroupId::from_slice(&[6; 32]); // accepted
        let group_id3 = GroupId::from_slice(&[7; 32]); // declined

        let (_, _) = whitenoise
            .get_or_create_account_group(&account, &group_id1)
            .await
            .unwrap();

        let (ag2, _) = whitenoise
            .get_or_create_account_group(&account, &group_id2)
            .await
            .unwrap();
        ag2.accept(&whitenoise).await.unwrap();

        let (ag3, _) = whitenoise
            .get_or_create_account_group(&account, &group_id3)
            .await
            .unwrap();
        ag3.decline(&whitenoise).await.unwrap();

        let visible = whitenoise
            .get_visible_account_groups(&account)
            .await
            .unwrap();

        assert_eq!(visible.len(), 2);
        let group_ids: Vec<_> = visible.iter().map(|ag| ag.mls_group_id.clone()).collect();
        assert!(group_ids.contains(&group_id1)); // pending is visible
        assert!(group_ids.contains(&group_id2)); // accepted is visible
        assert!(!group_ids.contains(&group_id3)); // declined is NOT visible
    }

    #[tokio::test]
    async fn test_get_pending_account_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let group_id1 = GroupId::from_slice(&[8; 32]); // pending
        let group_id2 = GroupId::from_slice(&[9; 32]); // accepted

        let (_, _) = whitenoise
            .get_or_create_account_group(&account, &group_id1)
            .await
            .unwrap();

        let (ag2, _) = whitenoise
            .get_or_create_account_group(&account, &group_id2)
            .await
            .unwrap();
        ag2.accept(&whitenoise).await.unwrap();

        let pending = whitenoise
            .get_pending_account_groups(&account)
            .await
            .unwrap();

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].mls_group_id, group_id1);
    }

    #[tokio::test]
    async fn test_accept_nonexistent_group_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[11; 32]);

        let result = whitenoise.accept_account_group(&account, &group_id).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WhitenoiseError::GroupNotFound
        ));
    }

    #[tokio::test]
    async fn test_decline_nonexistent_group_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[12; 32]);

        let result = whitenoise.decline_account_group(&account, &group_id).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WhitenoiseError::GroupNotFound
        ));
    }

    #[tokio::test]
    async fn test_account_group_get_returns_none_for_nonexistent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[13; 32]);

        let result = AccountGroup::get(&whitenoise, &account.pubkey, &group_id)
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_account_group_get_returns_some_for_existing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[14; 32]);

        // Create an account group first
        whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();

        // Now get should return it
        let result = AccountGroup::get(&whitenoise, &account.pubkey, &group_id)
            .await
            .unwrap();

        assert!(result.is_some());
        let ag = result.unwrap();
        assert_eq!(ag.account_pubkey, account.pubkey);
        assert_eq!(ag.mls_group_id, group_id);
    }

    #[tokio::test]
    async fn test_get_or_create_returns_was_created_false_for_existing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[15; 32]);

        // First call creates
        let (_, was_created1) = whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();
        assert!(was_created1);

        // Second call finds existing
        let (_, was_created2) = whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();
        assert!(!was_created2);
    }

    #[tokio::test]
    async fn test_multiple_accounts_can_have_same_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account1 = whitenoise.create_identity().await.unwrap();
        let account2 = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[16; 32]);

        // Both accounts can have the same group
        let (ag1, _) = whitenoise
            .get_or_create_account_group(&account1, &group_id)
            .await
            .unwrap();
        let (ag2, _) = whitenoise
            .get_or_create_account_group(&account2, &group_id)
            .await
            .unwrap();

        // Different records for different accounts
        assert_ne!(ag1.id, ag2.id);

        // Can have different confirmation states
        let accepted = ag1.accept(&whitenoise).await.unwrap();
        let declined = ag2.decline(&whitenoise).await.unwrap();

        assert!(accepted.is_accepted());
        assert!(declined.is_declined());
    }

    #[tokio::test]
    async fn test_pending_groups_empty_when_all_confirmed() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let group_id1 = GroupId::from_slice(&[17; 32]);
        let group_id2 = GroupId::from_slice(&[18; 32]);

        let (ag1, _) = whitenoise
            .get_or_create_account_group(&account, &group_id1)
            .await
            .unwrap();
        let (ag2, _) = whitenoise
            .get_or_create_account_group(&account, &group_id2)
            .await
            .unwrap();

        // Accept one, decline the other
        ag1.accept(&whitenoise).await.unwrap();
        ag2.decline(&whitenoise).await.unwrap();

        // No pending groups should remain
        let pending = whitenoise
            .get_pending_account_groups(&account)
            .await
            .unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn test_is_visible_true_for_accepted() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[19; 32]);

        let (pending_group, _) = whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();

        // Pending should be visible
        assert!(pending_group.is_visible());

        // Accept and verify still visible
        let accepted_group = pending_group.accept(&whitenoise).await.unwrap();
        assert!(accepted_group.is_visible());
    }

    #[tokio::test]
    async fn test_is_visible_false_for_declined() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[20; 32]);

        let (pending_group, _) = whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();

        let declined_group = pending_group.decline(&whitenoise).await.unwrap();
        assert!(!declined_group.is_visible());
    }

    #[tokio::test]
    async fn test_same_account_multiple_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let group_id1 = GroupId::from_slice(&[21; 32]);
        let group_id2 = GroupId::from_slice(&[22; 32]);
        let group_id3 = GroupId::from_slice(&[23; 32]);

        // Create 3 groups for the same account
        let (ag1, c1) = whitenoise
            .get_or_create_account_group(&account, &group_id1)
            .await
            .unwrap();
        let (ag2, c2) = whitenoise
            .get_or_create_account_group(&account, &group_id2)
            .await
            .unwrap();
        let (ag3, c3) = whitenoise
            .get_or_create_account_group(&account, &group_id3)
            .await
            .unwrap();

        assert!(c1 && c2 && c3);
        assert_ne!(ag1.id, ag2.id);
        assert_ne!(ag2.id, ag3.id);

        // All should be pending initially
        assert!(ag1.is_pending() && ag2.is_pending() && ag3.is_pending());
    }

    #[tokio::test]
    async fn test_mark_message_read_fails_for_nonexistent_message() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let fake_message_id = EventId::all_zeros();

        let result = whitenoise
            .mark_message_read(&account, &fake_message_id)
            .await;

        assert!(matches!(result, Err(WhitenoiseError::MessageNotFound)));
    }

    #[tokio::test]
    async fn test_get_last_read_message_id_returns_none_when_not_set() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[99; 32]);

        // Create account group first
        whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();

        let result = whitenoise
            .get_last_read_message_id(&account, &group_id)
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_get_last_read_message_id_returns_none_for_nonexistent_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[98; 32]);

        // Don't create account group - it shouldn't exist
        let result = whitenoise
            .get_last_read_message_id(&account, &group_id)
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_mark_message_read_advances_forward() {
        use crate::whitenoise::aggregated_message::AggregatedMessage;
        use crate::whitenoise::group_information::{GroupInformation, GroupType};
        use chrono::Utc;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[100; 32]);

        // Setup: create group_information (FK constraint) and account_group
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();
        whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();

        // Create two messages with different timestamps
        let now = Utc::now();
        let older_time = now - chrono::Duration::seconds(10);
        let newer_time = now;

        let older_msg_id = EventId::from_hex(&format!("{:0>64}", "aaa")).unwrap();
        let newer_msg_id = EventId::from_hex(&format!("{:0>64}", "bbb")).unwrap();

        AggregatedMessage::create_for_test(
            older_msg_id,
            group_id.clone(),
            account.pubkey,
            older_time,
            &whitenoise.database,
        )
        .await
        .unwrap();

        AggregatedMessage::create_for_test(
            newer_msg_id,
            group_id.clone(),
            account.pubkey,
            newer_time,
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Mark older message as read first
        let ag = whitenoise
            .mark_message_read(&account, &older_msg_id)
            .await
            .unwrap();
        assert_eq!(ag.last_read_message_id, Some(older_msg_id));

        // Mark newer message as read - should update
        let ag = whitenoise
            .mark_message_read(&account, &newer_msg_id)
            .await
            .unwrap();
        assert_eq!(ag.last_read_message_id, Some(newer_msg_id));
    }

    #[tokio::test]
    async fn test_mark_message_read_does_not_regress() {
        use crate::whitenoise::aggregated_message::AggregatedMessage;
        use crate::whitenoise::group_information::{GroupInformation, GroupType};
        use chrono::Utc;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[101; 32]);

        // Setup
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();
        whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();

        // Create two messages with different timestamps
        let now = Utc::now();
        let older_time = now - chrono::Duration::seconds(10);
        let newer_time = now;

        let older_msg_id = EventId::from_hex(&format!("{:0>64}", "ccc")).unwrap();
        let newer_msg_id = EventId::from_hex(&format!("{:0>64}", "ddd")).unwrap();

        AggregatedMessage::create_for_test(
            older_msg_id,
            group_id.clone(),
            account.pubkey,
            older_time,
            &whitenoise.database,
        )
        .await
        .unwrap();

        AggregatedMessage::create_for_test(
            newer_msg_id,
            group_id.clone(),
            account.pubkey,
            newer_time,
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Mark NEWER message as read first
        let ag = whitenoise
            .mark_message_read(&account, &newer_msg_id)
            .await
            .unwrap();
        assert_eq!(ag.last_read_message_id, Some(newer_msg_id));

        // Attempt to mark OLDER message as read - should be a no-op
        let ag = whitenoise
            .mark_message_read(&account, &older_msg_id)
            .await
            .unwrap();
        // Should still be the newer message
        assert_eq!(ag.last_read_message_id, Some(newer_msg_id));
    }

    #[tokio::test]
    async fn test_mark_message_read_equal_timestamp_is_noop() {
        use crate::whitenoise::aggregated_message::AggregatedMessage;
        use crate::whitenoise::group_information::{GroupInformation, GroupType};
        use chrono::Utc;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[102; 32]);

        // Setup
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();
        whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();

        // Create two messages with the SAME timestamp
        let same_time = Utc::now();

        let first_msg_id = EventId::from_hex(&format!("{:0>64}", "eee")).unwrap();
        let second_msg_id = EventId::from_hex(&format!("{:0>64}", "fff")).unwrap();

        AggregatedMessage::create_for_test(
            first_msg_id,
            group_id.clone(),
            account.pubkey,
            same_time,
            &whitenoise.database,
        )
        .await
        .unwrap();

        AggregatedMessage::create_for_test(
            second_msg_id,
            group_id.clone(),
            account.pubkey,
            same_time,
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Mark first message as read
        let ag = whitenoise
            .mark_message_read(&account, &first_msg_id)
            .await
            .unwrap();
        assert_eq!(ag.last_read_message_id, Some(first_msg_id));

        // Mark second message with same timestamp - should NOT update (equal is not newer)
        let ag = whitenoise
            .mark_message_read(&account, &second_msg_id)
            .await
            .unwrap();
        assert_eq!(ag.last_read_message_id, Some(first_msg_id));
    }

    #[tokio::test]
    async fn test_set_chat_pin_order_pins_chat() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[50; 32]);

        // Create account group first
        whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();

        // Pin the chat
        let pinned = whitenoise
            .set_chat_pin_order(&account, &group_id, Some(100))
            .await
            .unwrap();

        assert_eq!(pinned.pin_order, Some(100));
    }

    #[tokio::test]
    async fn test_set_chat_pin_order_unpins_chat() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[51; 32]);

        // Create and pin account group
        whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();
        whitenoise
            .set_chat_pin_order(&account, &group_id, Some(100))
            .await
            .unwrap();

        // Unpin the chat
        let unpinned = whitenoise
            .set_chat_pin_order(&account, &group_id, None)
            .await
            .unwrap();

        assert!(unpinned.pin_order.is_none());
    }

    #[tokio::test]
    async fn test_set_chat_pin_order_updates_existing_pin() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[52; 32]);

        // Create and pin account group
        whitenoise
            .get_or_create_account_group(&account, &group_id)
            .await
            .unwrap();
        whitenoise
            .set_chat_pin_order(&account, &group_id, Some(100))
            .await
            .unwrap();

        // Update pin order
        let updated = whitenoise
            .set_chat_pin_order(&account, &group_id, Some(50))
            .await
            .unwrap();

        assert_eq!(updated.pin_order, Some(50));
    }

    #[tokio::test]
    async fn test_set_chat_pin_order_nonexistent_group_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(&[53; 32]);

        // Don't create account group - it shouldn't exist
        let result = whitenoise
            .set_chat_pin_order(&account, &group_id, Some(100))
            .await;

        assert!(matches!(result, Err(WhitenoiseError::GroupNotFound)));
    }
}
