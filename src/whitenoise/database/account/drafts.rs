//! Per-account repository for message drafts.
//!
//! Wraps the existing [`Draft`] DB functions so that callers do not need to
//! thread an `account_pubkey` argument through every call — the pubkey is
//! baked in at construction time.

use std::sync::Arc;

use mdk_core::prelude::GroupId;
use nostr_sdk::{EventId, PublicKey};

use crate::whitenoise::database::Database;
use crate::whitenoise::drafts::Draft;
use crate::whitenoise::error::Result;
use crate::whitenoise::media_files::MediaFile;

/// Repository for message drafts scoped to a single account.
#[derive(Clone, Debug)]
pub struct DraftsRepo {
    account_pubkey: PublicKey,
    db: Arc<Database>,
}

impl DraftsRepo {
    /// Construct a new [`DraftsRepo`] for `account_pubkey`.
    pub(crate) fn new(account_pubkey: PublicKey, db: Arc<Database>) -> Self {
        Self { account_pubkey, db }
    }

    /// Upsert the draft for `group_id`.
    ///
    /// Delegates to `Draft::save` with the baked-in account pubkey.
    pub async fn save(
        &self,
        mls_group_id: &GroupId,
        content: &str,
        reply_to_id: Option<&EventId>,
        media_attachments: &[MediaFile],
    ) -> Result<Draft> {
        Draft::save(
            &self.account_pubkey,
            mls_group_id,
            content,
            reply_to_id,
            media_attachments,
            &self.db,
        )
        .await
    }

    /// Return the draft for `group_id`, or `None` if absent.
    ///
    /// Delegates to `Draft::find` with the baked-in account pubkey.
    pub async fn find(&self, mls_group_id: &GroupId) -> Result<Option<Draft>> {
        Draft::find(&self.account_pubkey, mls_group_id, &self.db).await
    }

    /// Delete the draft for `group_id`. No-op if no draft exists.
    ///
    /// Delegates to `Draft::delete` with the baked-in account pubkey.
    pub async fn delete(&self, mls_group_id: &GroupId) -> Result<()> {
        Draft::delete(&self.account_pubkey, mls_group_id, &self.db).await
    }
}

#[cfg(test)]
mod tests {
    use mdk_core::prelude::GroupId;
    use nostr_sdk::{Keys, PublicKey};

    use super::DraftsRepo;
    use crate::whitenoise::database::Database;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    async fn insert_test_account(database: &Database, pubkey: &PublicKey) {
        let user_pubkey = pubkey.to_hex();
        sqlx::query("INSERT INTO users (pubkey, metadata) VALUES (?, '{}')")
            .bind(&user_pubkey)
            .execute(&database.pool)
            .await
            .expect("insert user");
        let (user_id,): (i64,) = sqlx::query_as("SELECT id FROM users WHERE pubkey = ?")
            .bind(&user_pubkey)
            .fetch_one(&database.pool)
            .await
            .expect("get user id");
        sqlx::query("INSERT INTO accounts (pubkey, user_id, last_synced_at) VALUES (?, ?, NULL)")
            .bind(&user_pubkey)
            .bind(user_id)
            .execute(&database.pool)
            .await
            .expect("insert account");
    }

    async fn insert_test_group(database: &Database, group_id: &GroupId) {
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO group_information (mls_group_id, group_type, created_at, updated_at)
             VALUES (?, 'group', ?, ?)",
        )
        .bind(group_id.as_slice())
        .bind(now)
        .bind(now)
        .execute(&database.pool)
        .await
        .expect("insert group_information");
    }

    #[tokio::test]
    async fn save_creates_draft_scoped_to_account() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        insert_test_group(&wn.database, &group_id).await;

        let repo = DraftsRepo::new(keys.public_key(), wn.database.clone());
        let draft = repo.save(&group_id, "hello", None, &[]).await.unwrap();

        assert!(draft.id.is_some());
        assert_eq!(draft.account_pubkey, keys.public_key());
        assert_eq!(draft.content, "hello");
    }

    #[tokio::test]
    async fn find_returns_saved_draft() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        insert_test_group(&wn.database, &group_id).await;

        let repo = DraftsRepo::new(keys.public_key(), wn.database.clone());
        repo.save(&group_id, "persisted", None, &[]).await.unwrap();

        let found = repo.find(&group_id).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().content, "persisted");
    }

    #[tokio::test]
    async fn find_returns_none_when_no_draft_exists() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let group_id = GroupId::from_slice(b"test-group-id-00");

        let repo = DraftsRepo::new(keys.public_key(), wn.database.clone());
        let found = repo.find(&group_id).await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn delete_removes_draft() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        insert_test_group(&wn.database, &group_id).await;

        let repo = DraftsRepo::new(keys.public_key(), wn.database.clone());
        repo.save(&group_id, "to delete", None, &[]).await.unwrap();
        repo.delete(&group_id).await.unwrap();

        assert!(repo.find(&group_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_is_noop() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let group_id = GroupId::from_slice(b"test-group-id-00");

        let repo = DraftsRepo::new(keys.public_key(), wn.database.clone());
        assert!(repo.delete(&group_id).await.is_ok());
    }

    #[tokio::test]
    async fn repos_for_different_accounts_are_isolated() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys_a = Keys::generate();
        let keys_b = Keys::generate();
        insert_test_account(&wn.database, &keys_a.public_key()).await;
        insert_test_account(&wn.database, &keys_b.public_key()).await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        insert_test_group(&wn.database, &group_id).await;

        let repo_a = DraftsRepo::new(keys_a.public_key(), wn.database.clone());
        let repo_b = DraftsRepo::new(keys_b.public_key(), wn.database.clone());

        repo_a
            .save(&group_id, "account A draft", None, &[])
            .await
            .unwrap();

        assert!(repo_b.find(&group_id).await.unwrap().is_none());
    }
}
