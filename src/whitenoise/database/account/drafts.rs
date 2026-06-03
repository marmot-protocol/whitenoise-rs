//! Per-account repository for message drafts.

use std::sync::Arc;

use crate::marmot::GroupId;
use nostr_sdk::EventId;

use crate::whitenoise::database::account_db::AccountDatabase;
use crate::whitenoise::drafts::Draft;
use crate::whitenoise::error::Result;
use crate::whitenoise::media_files::MediaFile;

/// Repository for message drafts scoped to a single account.
#[derive(Clone, Debug)]
pub struct DraftsRepo {
    db: Arc<AccountDatabase>,
}

impl DraftsRepo {
    pub(crate) fn new(db: Arc<AccountDatabase>) -> Self {
        Self { db }
    }

    /// Upsert the draft for `group_id`.
    pub async fn save(
        &self,
        mls_group_id: &GroupId,
        content: &str,
        reply_to_id: Option<&EventId>,
        media_attachments: &[MediaFile],
    ) -> Result<Draft> {
        Draft::save(
            &self.db,
            mls_group_id,
            content,
            reply_to_id,
            media_attachments,
        )
        .await
    }

    /// Return the draft for `group_id`, or `None` if absent.
    pub async fn find(&self, mls_group_id: &GroupId) -> Result<Option<Draft>> {
        Draft::find(&self.db, mls_group_id).await
    }

    /// Delete the draft for `group_id`. No-op if no draft exists.
    pub async fn delete(&self, mls_group_id: &GroupId) -> Result<()> {
        Draft::delete(&self.db, mls_group_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::marmot::GroupId;
    use nostr_sdk::Keys;
    use tempfile::TempDir;

    use super::DraftsRepo;
    use crate::whitenoise::database::account_db::AccountDatabase;

    async fn setup() -> (DraftsRepo, TempDir) {
        let dir = TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let db = Arc::new(
            AccountDatabase::new(pubkey, dir.path().join("acct.db"))
                .await
                .unwrap(),
        );

        sqlx::query("DROP TABLE IF EXISTS drafts")
            .execute(&db.inner.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE drafts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                mls_group_id BLOB NOT NULL UNIQUE,
                content TEXT NOT NULL DEFAULT '',
                reply_to_id TEXT
                    CHECK (reply_to_id IS NULL OR (length(reply_to_id) = 64 AND reply_to_id NOT GLOB '*[^0-9a-fA-F]*')),
                media_attachments JSONB NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )",
        )
        .execute(&db.inner.pool)
        .await
        .unwrap();

        (DraftsRepo::new(db), dir)
    }

    #[tokio::test]
    async fn save_creates_draft() {
        let (repo, _dir) = setup().await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        let draft = repo.save(&group_id, "hello", None, &[]).await.unwrap();
        assert!(draft.id.is_some());
        assert_eq!(draft.content, "hello");
    }

    #[tokio::test]
    async fn find_returns_saved_draft() {
        let (repo, _dir) = setup().await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        repo.save(&group_id, "persisted", None, &[]).await.unwrap();

        let found = repo.find(&group_id).await.unwrap();
        assert_eq!(found.unwrap().content, "persisted");
    }

    #[tokio::test]
    async fn find_returns_none_when_no_draft_exists() {
        let (repo, _dir) = setup().await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        assert!(repo.find(&group_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_removes_draft() {
        let (repo, _dir) = setup().await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        repo.save(&group_id, "to delete", None, &[]).await.unwrap();
        repo.delete(&group_id).await.unwrap();

        assert!(repo.find(&group_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_is_noop() {
        let (repo, _dir) = setup().await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        assert!(repo.delete(&group_id).await.is_ok());
    }
}
