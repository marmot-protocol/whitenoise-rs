//! Database operations for message drafts.
//!
//! Lives in the per-account SQLite file. The owning account is implicit —
//! it's whichever account owns the file.

use chrono::Utc;
use mdk_core::prelude::GroupId;
use nostr_sdk::EventId;

use super::DatabaseError;
use super::account_db::AccountDatabase;
use super::utils::parse_timestamp;
use crate::perf_span;
use crate::whitenoise::drafts::Draft;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::media_files::MediaFile;

/// Local row shape — no `account_pubkey` column (file is the scope).
struct LocalRow {
    id: i64,
    mls_group_id: GroupId,
    content: String,
    reply_to_id: Option<EventId>,
    media_attachments: Vec<MediaFile>,
    created_at: chrono::DateTime<Utc>,
    updated_at: chrono::DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for LocalRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Vec<u8>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let mls_group_id_bytes: Vec<u8> = row.try_get("mls_group_id")?;
        let mls_group_id = GroupId::from_slice(&mls_group_id_bytes);
        let content: String = row.try_get("content")?;
        let reply_to_id = match row.try_get::<Option<String>, _>("reply_to_id")? {
            Some(hex) => Some(
                EventId::from_hex(&hex).map_err(|e| sqlx::Error::ColumnDecode {
                    index: "reply_to_id".to_string(),
                    source: Box::new(e),
                })?,
            ),
            None => None,
        };
        let media_attachments_str: String = row.try_get("media_attachments")?;
        let media_attachments = serde_json::from_str(&media_attachments_str).map_err(|e| {
            sqlx::Error::ColumnDecode {
                index: "media_attachments".to_string(),
                source: Box::new(e),
            }
        })?;
        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(Self {
            id,
            mls_group_id,
            content,
            reply_to_id,
            media_attachments,
            created_at,
            updated_at,
        })
    }
}

impl Draft {
    /// Upsert the draft for `mls_group_id` in the account that owns `db`.
    ///
    /// On conflict the existing row's `content`, `reply_to_id`,
    /// `media_attachments`, and `updated_at` are replaced; `created_at` is
    /// preserved.
    pub(crate) async fn save(
        db: &AccountDatabase,
        mls_group_id: &GroupId,
        content: &str,
        reply_to_id: Option<&EventId>,
        media_attachments: &[MediaFile],
    ) -> Result<Self, WhitenoiseError> {
        let _span = perf_span!("db::draft_save");
        let now = Utc::now().timestamp_millis();

        let media_json =
            serde_json::to_string(media_attachments).map_err(|e| sqlx::Error::ColumnDecode {
                index: "media_attachments".to_string(),
                source: Box::new(e),
            })?;

        let row = sqlx::query_as::<_, LocalRow>(
            "INSERT INTO drafts
                 (mls_group_id, content, reply_to_id, media_attachments, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?)
               ON CONFLICT(mls_group_id) DO UPDATE SET
                   content = excluded.content,
                   reply_to_id = excluded.reply_to_id,
                   media_attachments = excluded.media_attachments,
                   updated_at = excluded.updated_at
               RETURNING *",
        )
        .bind(mls_group_id.as_slice())
        .bind(content)
        .bind(reply_to_id.map(|id| id.to_hex()))
        .bind(&media_json)
        .bind(now)
        .bind(now)
        .fetch_one(&db.inner.pool)
        .await
        .map_err(DatabaseError::from)?;

        Ok(Self::from_local_row(db, row))
    }

    /// Fetch the draft for `mls_group_id`, returning `None` if absent.
    pub(crate) async fn find(
        db: &AccountDatabase,
        mls_group_id: &GroupId,
    ) -> Result<Option<Self>, WhitenoiseError> {
        let _span = perf_span!("db::draft_find");
        let row = sqlx::query_as::<_, LocalRow>("SELECT * FROM drafts WHERE mls_group_id = ?")
            .bind(mls_group_id.as_slice())
            .fetch_optional(&db.inner.pool)
            .await
            .map_err(DatabaseError::from)?;

        Ok(row.map(|r| Self::from_local_row(db, r)))
    }

    /// Delete the draft for `mls_group_id`. A no-op if absent.
    pub(crate) async fn delete(
        db: &AccountDatabase,
        mls_group_id: &GroupId,
    ) -> Result<(), WhitenoiseError> {
        let _span = perf_span!("db::draft_delete");
        sqlx::query("DELETE FROM drafts WHERE mls_group_id = ?")
            .bind(mls_group_id.as_slice())
            .execute(&db.inner.pool)
            .await
            .map_err(DatabaseError::from)?;

        Ok(())
    }

    fn from_local_row(db: &AccountDatabase, row: LocalRow) -> Self {
        Self {
            id: Some(row.id),
            account_pubkey: *db.account_pubkey(),
            mls_group_id: row.mls_group_id,
            content: row.content,
            reply_to_id: row.reply_to_id,
            media_attachments: row.media_attachments,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::Keys;
    use tempfile::TempDir;

    /// Build a per-account DB with the post-move local schema applied
    /// directly. Bypasses the migration framework — that's covered in the
    /// migration's own tests.
    async fn setup() -> (AccountDatabase, TempDir) {
        let dir = TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let path = dir.path().join("acct.db");
        let db = AccountDatabase::new(pubkey, path).await.unwrap();

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

        (db, dir)
    }

    #[tokio::test]
    async fn test_save_creates_new_draft() {
        let (db, _dir) = setup().await;
        let group_id = GroupId::from_slice(b"test-group-id-00");

        let draft = Draft::save(&db, &group_id, "hello", None, &[])
            .await
            .unwrap();

        assert!(draft.id.is_some());
        assert_eq!(draft.account_pubkey, *db.account_pubkey());
        assert_eq!(draft.mls_group_id, group_id);
        assert_eq!(draft.content, "hello");
        assert!(draft.reply_to_id.is_none());
        assert!(draft.media_attachments.is_empty());
    }

    #[tokio::test]
    async fn test_save_updates_existing_draft() {
        let (db, _dir) = setup().await;
        let group_id = GroupId::from_slice(b"test-group-id-00");

        let first = Draft::save(&db, &group_id, "v1", None, &[]).await.unwrap();
        let second = Draft::save(&db, &group_id, "v2", None, &[]).await.unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(second.content, "v2");
    }

    #[tokio::test]
    async fn test_save_preserves_created_at_on_update() {
        let (db, _dir) = setup().await;
        let group_id = GroupId::from_slice(b"test-group-id-00");

        let first = Draft::save(&db, &group_id, "v1", None, &[]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let second = Draft::save(&db, &group_id, "v2", None, &[]).await.unwrap();

        assert_eq!(first.created_at, second.created_at);
        assert!(second.updated_at >= first.updated_at);
    }

    #[tokio::test]
    async fn test_find_returns_draft() {
        let (db, _dir) = setup().await;
        let group_id = GroupId::from_slice(b"test-group-id-00");

        Draft::save(&db, &group_id, "persisted", None, &[])
            .await
            .unwrap();

        let found = Draft::find(&db, &group_id)
            .await
            .unwrap()
            .expect("draft should exist");
        assert_eq!(found.content, "persisted");
    }

    #[tokio::test]
    async fn test_find_returns_none_for_nonexistent() {
        let (db, _dir) = setup().await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        assert!(Draft::find(&db, &group_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_delete_removes_draft() {
        let (db, _dir) = setup().await;
        let group_id = GroupId::from_slice(b"test-group-id-00");

        Draft::save(&db, &group_id, "to delete", None, &[])
            .await
            .unwrap();
        Draft::delete(&db, &group_id).await.unwrap();

        assert!(Draft::find(&db, &group_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_delete_nonexistent_is_noop() {
        let (db, _dir) = setup().await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        assert!(Draft::delete(&db, &group_id).await.is_ok());
    }
}
