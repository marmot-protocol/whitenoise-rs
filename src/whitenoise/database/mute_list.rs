use chrono::{DateTime, Utc};
use nostr_sdk::PublicKey;

use super::{Database, DatabaseError, utils::parse_timestamp};
use crate::perf_instrument;

/// Row-level representation of a mute list entry from the `mute_list` table.
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
struct MuteListRow {
    id: i64,
    account_pubkey: PublicKey,
    muted_pubkey: PublicKey,
    is_private: bool,
    created_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for MuteListRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;

        let account_pubkey_str: String = row.try_get("account_pubkey")?;
        let account_pubkey =
            PublicKey::parse(&account_pubkey_str).map_err(|e| sqlx::Error::ColumnDecode {
                index: "account_pubkey".to_string(),
                source: Box::new(e),
            })?;

        let muted_pubkey_str: String = row.try_get("muted_pubkey")?;
        let muted_pubkey =
            PublicKey::parse(&muted_pubkey_str).map_err(|e| sqlx::Error::ColumnDecode {
                index: "muted_pubkey".to_string(),
                source: Box::new(e),
            })?;

        let is_private: i64 = row.try_get("is_private")?;
        let created_at = parse_timestamp(row, "created_at")?;

        Ok(Self {
            id,
            account_pubkey,
            muted_pubkey,
            is_private: is_private != 0,
            created_at,
        })
    }
}

/// Public mute list entry exposed to consumers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MuteListEntry {
    pub account_pubkey: PublicKey,
    pub muted_pubkey: PublicKey,
    pub is_private: bool,
    pub created_at: DateTime<Utc>,
}

impl From<MuteListRow> for MuteListEntry {
    fn from(row: MuteListRow) -> Self {
        Self {
            account_pubkey: row.account_pubkey,
            muted_pubkey: row.muted_pubkey,
            is_private: row.is_private,
            created_at: row.created_at,
        }
    }
}

impl MuteListEntry {
    /// Inserts a new mute list entry. Ignores duplicates (UNIQUE constraint).
    #[perf_instrument("mute_list")]
    pub async fn insert(
        account_pubkey: &PublicKey,
        muted_pubkey: &PublicKey,
        is_private: bool,
        db: &Database,
    ) -> std::result::Result<(), DatabaseError> {
        sqlx::query(
            "INSERT OR IGNORE INTO mute_list (account_pubkey, muted_pubkey, is_private, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(account_pubkey.to_hex())
        .bind(muted_pubkey.to_hex())
        .bind(i64::from(is_private))
        .bind(Utc::now().timestamp_millis())
        .execute(&db.pool)
        .await?;
        Ok(())
    }

    /// Removes a mute list entry for the given account and muted pubkey.
    #[perf_instrument("mute_list")]
    pub async fn delete(
        account_pubkey: &PublicKey,
        muted_pubkey: &PublicKey,
        db: &Database,
    ) -> std::result::Result<(), DatabaseError> {
        sqlx::query("DELETE FROM mute_list WHERE account_pubkey = ? AND muted_pubkey = ?")
            .bind(account_pubkey.to_hex())
            .bind(muted_pubkey.to_hex())
            .execute(&db.pool)
            .await?;
        Ok(())
    }

    /// Returns all mute list entries for the given account.
    #[perf_instrument("mute_list")]
    pub async fn find_by_account(
        account_pubkey: &PublicKey,
        db: &Database,
    ) -> std::result::Result<Vec<Self>, DatabaseError> {
        let rows: Vec<MuteListRow> = sqlx::query_as(
            "SELECT id, account_pubkey, muted_pubkey, is_private, created_at
             FROM mute_list
             WHERE account_pubkey = ?
             ORDER BY created_at DESC",
        )
        .bind(account_pubkey.to_hex())
        .fetch_all(&db.pool)
        .await?;

        Ok(rows.into_iter().map(Self::from).collect())
    }

    /// Returns `true` if the given pubkey is on the account's mute list.
    #[perf_instrument("mute_list")]
    pub async fn exists(
        account_pubkey: &PublicKey,
        muted_pubkey: &PublicKey,
        db: &Database,
    ) -> std::result::Result<bool, DatabaseError> {
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM mute_list
             WHERE account_pubkey = ? AND muted_pubkey = ?",
        )
        .bind(account_pubkey.to_hex())
        .bind(muted_pubkey.to_hex())
        .fetch_one(&db.pool)
        .await?;

        Ok(count.0 > 0)
    }

    /// Replaces all mute list entries for the given account with the provided
    /// list. Used when syncing from a kind 10000 event received from relays.
    #[perf_instrument("mute_list")]
    pub async fn sync_from_event(
        account_pubkey: &PublicKey,
        entries: &[(PublicKey, bool)],
        db: &Database,
    ) -> std::result::Result<(), DatabaseError> {
        let mut txn = db.pool.begin().await?;

        sqlx::query("DELETE FROM mute_list WHERE account_pubkey = ?")
            .bind(account_pubkey.to_hex())
            .execute(&mut *txn)
            .await?;

        // INSERT OR IGNORE: if the same pubkey appears more than once in the
        // event (e.g. in both the public tags and the encrypted private section)
        // the first occurrence wins and the duplicate is silently skipped.
        // The `is_private` value of the first entry is therefore what gets
        // stored; callers should be aware that public-tag entries are inserted
        // before private-section entries in `parse_mute_list_entries`.
        for (muted_pubkey, is_private) in entries {
            sqlx::query(
                "INSERT OR IGNORE INTO mute_list (account_pubkey, muted_pubkey, is_private, created_at)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(account_pubkey.to_hex())
            .bind(muted_pubkey.to_hex())
            .bind(i64::from(*is_private))
            .bind(Utc::now().timestamp_millis())
            .execute(&mut *txn)
            .await?;
        }

        txn.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;

    use super::*;
    use crate::whitenoise::database::Database;

    async fn create_test_db() -> (Database, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path)
            .await
            .expect("Failed to create test database");
        (db, temp_dir)
    }

    #[tokio::test]
    async fn insert_and_exists() {
        let (db, _tmp) = create_test_db().await;
        let account = Keys::generate().public_key();
        let target = Keys::generate().public_key();

        assert!(!MuteListEntry::exists(&account, &target, &db).await.unwrap());

        MuteListEntry::insert(&account, &target, true, &db)
            .await
            .unwrap();

        assert!(MuteListEntry::exists(&account, &target, &db).await.unwrap());
    }

    #[tokio::test]
    async fn insert_duplicate_is_ignored() {
        let (db, _tmp) = create_test_db().await;
        let account = Keys::generate().public_key();
        let target = Keys::generate().public_key();

        MuteListEntry::insert(&account, &target, true, &db)
            .await
            .unwrap();
        MuteListEntry::insert(&account, &target, true, &db)
            .await
            .unwrap();

        let entries = MuteListEntry::find_by_account(&account, &db).await.unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let (db, _tmp) = create_test_db().await;
        let account = Keys::generate().public_key();
        let target = Keys::generate().public_key();

        MuteListEntry::insert(&account, &target, true, &db)
            .await
            .unwrap();
        MuteListEntry::delete(&account, &target, &db).await.unwrap();

        assert!(!MuteListEntry::exists(&account, &target, &db).await.unwrap());
    }

    #[tokio::test]
    async fn find_by_account_returns_only_matching() {
        let (db, _tmp) = create_test_db().await;
        let account1 = Keys::generate().public_key();
        let account2 = Keys::generate().public_key();
        let target = Keys::generate().public_key();

        MuteListEntry::insert(&account1, &target, true, &db)
            .await
            .unwrap();
        MuteListEntry::insert(&account2, &target, false, &db)
            .await
            .unwrap();

        let entries = MuteListEntry::find_by_account(&account1, &db)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].account_pubkey, account1);
        assert!(entries[0].is_private);
    }

    #[tokio::test]
    async fn sync_from_event_replaces_all() {
        let (db, _tmp) = create_test_db().await;
        let account = Keys::generate().public_key();
        let old_target = Keys::generate().public_key();
        let new_target1 = Keys::generate().public_key();
        let new_target2 = Keys::generate().public_key();

        MuteListEntry::insert(&account, &old_target, true, &db)
            .await
            .unwrap();

        let new_entries = vec![(new_target1, true), (new_target2, false)];
        MuteListEntry::sync_from_event(&account, &new_entries, &db)
            .await
            .unwrap();

        assert!(
            !MuteListEntry::exists(&account, &old_target, &db)
                .await
                .unwrap()
        );
        assert!(
            MuteListEntry::exists(&account, &new_target1, &db)
                .await
                .unwrap()
        );
        assert!(
            MuteListEntry::exists(&account, &new_target2, &db)
                .await
                .unwrap()
        );

        let entries = MuteListEntry::find_by_account(&account, &db).await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn sync_from_event_with_duplicate_pubkeys_is_safe() {
        let (db, _tmp) = create_test_db().await;
        let account = Keys::generate().public_key();
        let target = Keys::generate().public_key();

        // Same pubkey appears twice (e.g. in both public and private sections of one event)
        let entries = vec![(target, false), (target, true)];
        MuteListEntry::sync_from_event(&account, &entries, &db)
            .await
            .unwrap();

        let result = MuteListEntry::find_by_account(&account, &db).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(MuteListEntry::exists(&account, &target, &db).await.unwrap());
    }

    #[tokio::test]
    async fn sync_from_event_with_empty_list_clears_all() {
        let (db, _tmp) = create_test_db().await;
        let account = Keys::generate().public_key();
        let target = Keys::generate().public_key();

        MuteListEntry::insert(&account, &target, true, &db)
            .await
            .unwrap();

        MuteListEntry::sync_from_event(&account, &[], &db)
            .await
            .unwrap();

        assert!(!MuteListEntry::exists(&account, &target, &db).await.unwrap());
        let entries = MuteListEntry::find_by_account(&account, &db).await.unwrap();
        assert!(entries.is_empty());
    }
}
