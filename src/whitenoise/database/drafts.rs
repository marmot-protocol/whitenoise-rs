//! Database operations for message drafts.

use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::{EventId, PublicKey};

use super::{Database, DatabaseError, utils::parse_timestamp};
use crate::whitenoise::drafts::Draft;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::media_files::MediaFile;

/// Internal database row representation for the drafts table.
#[derive(Debug)]
struct DraftRow {
    id: i64,
    account_pubkey: PublicKey,
    mls_group_id: GroupId,
    content: String,
    reply_to_id: Option<EventId>,
    media_attachments: Vec<MediaFile>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for DraftRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Vec<u8>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;

        let account_pubkey_str: String = row.try_get("account_pubkey")?;
        let account_pubkey =
            PublicKey::parse(&account_pubkey_str).map_err(|e| sqlx::Error::ColumnDecode {
                index: "account_pubkey".to_string(),
                source: Box::new(e),
            })?;

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
            account_pubkey,
            mls_group_id,
            content,
            reply_to_id,
            media_attachments,
            created_at,
            updated_at,
        })
    }
}

impl From<DraftRow> for Draft {
    fn from(row: DraftRow) -> Self {
        Self {
            id: Some(row.id),
            account_pubkey: row.account_pubkey,
            mls_group_id: row.mls_group_id,
            content: row.content,
            reply_to_id: row.reply_to_id,
            media_attachments: row.media_attachments,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

impl Draft {
    /// Upserts a draft for the given (account, group).
    ///
    /// On conflict the existing row's `content`, `reply_to_id`,
    /// `media_attachments`, and `updated_at` are replaced while
    /// `created_at` is preserved.
    pub(crate) async fn save(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        content: &str,
        reply_to_id: Option<&EventId>,
        media_attachments: &[MediaFile],
        database: &Database,
    ) -> Result<Self, WhitenoiseError> {
        let now = Utc::now().timestamp_millis();

        let media_json =
            serde_json::to_string(media_attachments).map_err(|e| sqlx::Error::ColumnDecode {
                index: "media_attachments".to_string(),
                source: Box::new(e),
            })?;

        let row = sqlx::query_as::<_, DraftRow>(
            "INSERT INTO drafts
                 (account_pubkey, mls_group_id, content, reply_to_id,
                  media_attachments, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)
               ON CONFLICT(account_pubkey, mls_group_id) DO UPDATE SET
                   content = excluded.content,
                   reply_to_id = excluded.reply_to_id,
                   media_attachments = excluded.media_attachments,
                   updated_at = excluded.updated_at
               RETURNING *",
        )
        .bind(account_pubkey.to_hex())
        .bind(mls_group_id.as_slice())
        .bind(content)
        .bind(reply_to_id.map(|id| id.to_hex()))
        .bind(&media_json)
        .bind(now)
        .bind(now)
        .fetch_one(&database.pool)
        .await
        .map_err(DatabaseError::from)?;

        Ok(row.into())
    }

    /// Fetches the draft for (account, group), returning `None` if absent.
    pub(crate) async fn find(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        database: &Database,
    ) -> Result<Option<Self>, WhitenoiseError> {
        let row = sqlx::query_as::<_, DraftRow>(
            "SELECT * FROM drafts WHERE account_pubkey = ? AND mls_group_id = ?",
        )
        .bind(account_pubkey.to_hex())
        .bind(mls_group_id.as_slice())
        .fetch_optional(&database.pool)
        .await
        .map_err(DatabaseError::from)?;

        Ok(row.map(Into::into))
    }

    /// Deletes the draft for (account, group). A no-op if no draft exists.
    pub(crate) async fn delete(
        account_pubkey: &PublicKey,
        mls_group_id: &GroupId,
        database: &Database,
    ) -> Result<(), WhitenoiseError> {
        sqlx::query("DELETE FROM drafts WHERE account_pubkey = ? AND mls_group_id = ?")
            .bind(account_pubkey.to_hex())
            .bind(mls_group_id.as_slice())
            .execute(&database.pool)
            .await
            .map_err(DatabaseError::from)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::*;
    use nostr_sdk::Keys;

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
        let now = Utc::now().timestamp_millis();
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
    async fn test_save_creates_new_draft() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&whitenoise.database, &keys.public_key()).await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        insert_test_group(&whitenoise.database, &group_id).await;

        let draft = Draft::save(
            &keys.public_key(),
            &group_id,
            "hello",
            None,
            &[],
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert!(draft.id.is_some());
        assert_eq!(draft.account_pubkey, keys.public_key());
        assert_eq!(draft.mls_group_id, group_id);
        assert_eq!(draft.content, "hello");
        assert!(draft.reply_to_id.is_none());
        assert!(draft.media_attachments.is_empty());
    }

    #[tokio::test]
    async fn test_save_updates_existing_draft() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&whitenoise.database, &keys.public_key()).await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        insert_test_group(&whitenoise.database, &group_id).await;

        let first = Draft::save(
            &keys.public_key(),
            &group_id,
            "v1",
            None,
            &[],
            &whitenoise.database,
        )
        .await
        .unwrap();
        let second = Draft::save(
            &keys.public_key(),
            &group_id,
            "v2",
            None,
            &[],
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(second.content, "v2");
    }

    #[tokio::test]
    async fn test_save_preserves_created_at_on_update() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&whitenoise.database, &keys.public_key()).await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        insert_test_group(&whitenoise.database, &group_id).await;

        let first = Draft::save(
            &keys.public_key(),
            &group_id,
            "v1",
            None,
            &[],
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Small delay so updated_at differs
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let second = Draft::save(
            &keys.public_key(),
            &group_id,
            "v2",
            None,
            &[],
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert_eq!(first.created_at, second.created_at);
        assert!(second.updated_at >= first.updated_at);
    }

    #[tokio::test]
    async fn test_find_returns_draft() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&whitenoise.database, &keys.public_key()).await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        insert_test_group(&whitenoise.database, &group_id).await;

        Draft::save(
            &keys.public_key(),
            &group_id,
            "persisted",
            None,
            &[],
            &whitenoise.database,
        )
        .await
        .unwrap();

        let found = Draft::find(&keys.public_key(), &group_id, &whitenoise.database)
            .await
            .unwrap()
            .expect("draft should exist");
        assert_eq!(found.content, "persisted");
    }

    #[tokio::test]
    async fn test_find_returns_none_for_nonexistent() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let group_id = GroupId::from_slice(b"test-group-id-00");

        let found = Draft::find(&keys.public_key(), &group_id, &whitenoise.database)
            .await
            .unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn test_delete_removes_draft() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&whitenoise.database, &keys.public_key()).await;
        let group_id = GroupId::from_slice(b"test-group-id-00");
        insert_test_group(&whitenoise.database, &group_id).await;

        Draft::save(
            &keys.public_key(),
            &group_id,
            "to delete",
            None,
            &[],
            &whitenoise.database,
        )
        .await
        .unwrap();

        Draft::delete(&keys.public_key(), &group_id, &whitenoise.database)
            .await
            .unwrap();

        let found = Draft::find(&keys.public_key(), &group_id, &whitenoise.database)
            .await
            .unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn test_delete_nonexistent_is_noop() {
        let (whitenoise, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let group_id = GroupId::from_slice(b"test-group-id-00");

        let result = Draft::delete(&keys.public_key(), &group_id, &whitenoise.database).await;
        assert!(result.is_ok());
    }
}
