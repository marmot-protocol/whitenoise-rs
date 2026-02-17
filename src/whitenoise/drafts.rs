//! Unsent message drafts scoped to (account, group).

use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::{EventId, PublicKey};
use serde::{Deserialize, Serialize};

use crate::whitenoise::{
    Whitenoise, accounts::Account, error::WhitenoiseError, media_files::MediaFile,
};

/// A saved message draft for a specific account and group.
///
/// Drafts are uniquely identified by `(account_pubkey, mls_group_id)`.
/// The `id` field follows the project convention (`AccountSettings`, `MediaFile`,
/// `Account`): `None` when constructing for save, `Some(...)` when returned from
/// the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub id: Option<i64>,
    pub account_pubkey: PublicKey,
    pub mls_group_id: GroupId,
    pub content: String,
    pub reply_to_id: Option<EventId>,
    pub media_attachments: Vec<MediaFile>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Whitenoise {
    /// Saves a draft for (account, group).
    ///
    /// Uses upsert semantics: creates a new draft or updates an existing one.
    /// Returns the persisted draft.
    pub async fn save_draft(
        &self,
        account: &Account,
        group_id: &GroupId,
        content: &str,
        reply_to_id: Option<&EventId>,
        media_attachments: &[MediaFile],
    ) -> Result<Draft, WhitenoiseError> {
        Draft::save(
            &account.pubkey,
            group_id,
            content,
            reply_to_id,
            media_attachments,
            &self.database,
        )
        .await
    }

    /// Loads the draft for (account, group), if any.
    pub async fn load_draft(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Option<Draft>, WhitenoiseError> {
        Draft::find(&account.pubkey, group_id, &self.database).await
    }

    /// Deletes the draft for (account, group).
    pub async fn delete_draft(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<(), WhitenoiseError> {
        Draft::delete(&account.pubkey, group_id, &self.database).await
    }
}

#[cfg(test)]
mod tests {
    use mdk_core::prelude::GroupId;

    use crate::whitenoise::group_information::{GroupInformation, GroupType};
    use crate::whitenoise::test_utils::*;

    #[tokio::test]
    async fn test_save_draft_creates_new() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(b"test-group-id-00");
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let draft = whitenoise
            .save_draft(&account, &group_id, "hello", None, &[])
            .await
            .unwrap();

        assert!(draft.id.is_some());
        assert_eq!(draft.account_pubkey, account.pubkey);
        assert_eq!(draft.mls_group_id, group_id);
        assert_eq!(draft.content, "hello");
        assert!(draft.reply_to_id.is_none());
        assert!(draft.media_attachments.is_empty());
    }

    #[tokio::test]
    async fn test_save_draft_updates_existing() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(b"test-group-id-00");
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let first = whitenoise
            .save_draft(&account, &group_id, "first", None, &[])
            .await
            .unwrap();
        let second = whitenoise
            .save_draft(&account, &group_id, "second", None, &[])
            .await
            .unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(second.content, "second");
    }

    #[tokio::test]
    async fn test_load_draft_returns_draft() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(b"test-group-id-00");
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();

        whitenoise
            .save_draft(&account, &group_id, "persisted", None, &[])
            .await
            .unwrap();

        let loaded = whitenoise
            .load_draft(&account, &group_id)
            .await
            .unwrap()
            .expect("draft should exist");
        assert_eq!(loaded.content, "persisted");
    }

    #[tokio::test]
    async fn test_load_draft_returns_none() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(b"test-group-id-00");

        let loaded = whitenoise.load_draft(&account, &group_id).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_delete_draft_removes() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(b"test-group-id-00");
        GroupInformation::find_or_create_by_mls_group_id(
            &group_id,
            Some(GroupType::Group),
            &whitenoise.database,
        )
        .await
        .unwrap();

        whitenoise
            .save_draft(&account, &group_id, "to delete", None, &[])
            .await
            .unwrap();
        whitenoise.delete_draft(&account, &group_id).await.unwrap();

        let loaded = whitenoise.load_draft(&account, &group_id).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_delete_draft_nonexistent_succeeds() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = GroupId::from_slice(b"test-group-id-00");

        let result = whitenoise.delete_draft(&account, &group_id).await;
        assert!(result.is_ok());
    }
}
