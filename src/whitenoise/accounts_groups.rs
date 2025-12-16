use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::PublicKey;
use serde::{Deserialize, Serialize};

use crate::whitenoise::{Whitenoise, accounts::Account, error::WhitenoiseError};

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
}
