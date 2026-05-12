//! Database operations for the NIP-51 mute list.
//!
//! Lives in the per-account SQLite file. The owning account's pubkey is
//! implicit — it's whichever account owns the file — so methods take an
//! `&AccountDatabase` and never need an `account_pubkey` parameter.

use chrono::{DateTime, Utc};
use nostr_sdk::PublicKey;

use super::DatabaseError;
use super::account_db::AccountDatabase;
use super::utils::parse_timestamp;
use crate::perf_instrument;

/// Local row shape (no `account_pubkey` column — implied by the file).
struct LocalRow {
    muted_pubkey: PublicKey,
    is_private: bool,
    created_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for LocalRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let muted_pubkey_str: String = row.try_get("muted_pubkey")?;
        let muted_pubkey =
            PublicKey::parse(&muted_pubkey_str).map_err(|e| sqlx::Error::ColumnDecode {
                index: "muted_pubkey".to_string(),
                source: Box::new(e),
            })?;

        let is_private: i64 = row.try_get("is_private")?;
        let created_at = parse_timestamp(row, "created_at")?;

        Ok(Self {
            muted_pubkey,
            is_private: is_private != 0,
            created_at,
        })
    }
}

/// Public mute list entry exposed to consumers.
///
/// `account_pubkey` is implicit — the owning account is whichever
/// per-account database the entry came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MuteListEntry {
    pub muted_pubkey: PublicKey,
    pub is_private: bool,
    pub created_at: DateTime<Utc>,
}

impl From<LocalRow> for MuteListEntry {
    fn from(row: LocalRow) -> Self {
        Self {
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
        muted_pubkey: &PublicKey,
        is_private: bool,
        db: &AccountDatabase,
    ) -> std::result::Result<(), DatabaseError> {
        sqlx::query(
            "INSERT OR IGNORE INTO mute_list (muted_pubkey, is_private, created_at)
             VALUES (?, ?, ?)",
        )
        .bind(muted_pubkey.to_hex())
        .bind(i64::from(is_private))
        .bind(Utc::now().timestamp_millis())
        .execute(&db.inner.pool)
        .await?;
        Ok(())
    }

    /// Removes a mute list entry by muted pubkey.
    #[perf_instrument("mute_list")]
    pub async fn delete(
        muted_pubkey: &PublicKey,
        db: &AccountDatabase,
    ) -> std::result::Result<(), DatabaseError> {
        sqlx::query("DELETE FROM mute_list WHERE muted_pubkey = ?")
            .bind(muted_pubkey.to_hex())
            .execute(&db.inner.pool)
            .await?;
        Ok(())
    }

    /// Returns all mute list entries for the owning account.
    #[perf_instrument("mute_list")]
    pub async fn find_all(db: &AccountDatabase) -> std::result::Result<Vec<Self>, DatabaseError> {
        let rows: Vec<LocalRow> = sqlx::query_as(
            "SELECT muted_pubkey, is_private, created_at
             FROM mute_list
             ORDER BY created_at DESC",
        )
        .fetch_all(&db.inner.pool)
        .await?;

        Ok(rows.into_iter().map(Self::from).collect())
    }

    /// Returns the mute list entry for a specific muted pubkey, if any.
    #[perf_instrument("mute_list")]
    pub async fn find_by_muted_pubkey(
        muted_pubkey: &PublicKey,
        db: &AccountDatabase,
    ) -> std::result::Result<Option<Self>, DatabaseError> {
        let row: Option<LocalRow> = sqlx::query_as(
            "SELECT muted_pubkey, is_private, created_at
             FROM mute_list
             WHERE muted_pubkey = ?",
        )
        .bind(muted_pubkey.to_hex())
        .fetch_optional(&db.inner.pool)
        .await?;

        Ok(row.map(Self::from))
    }

    /// Returns `true` if the given pubkey is on the account's mute list.
    #[perf_instrument("mute_list")]
    pub async fn exists(
        muted_pubkey: &PublicKey,
        db: &AccountDatabase,
    ) -> std::result::Result<bool, DatabaseError> {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mute_list WHERE muted_pubkey = ?)")
                .bind(muted_pubkey.to_hex())
                .fetch_one(&db.inner.pool)
                .await?;

        Ok(exists)
    }

    /// Replaces all mute list entries with the provided list. Used when
    /// syncing from a kind 10000 event received from relays.
    #[perf_instrument("mute_list")]
    pub async fn sync_from_event(
        entries: &[(PublicKey, bool)],
        db: &AccountDatabase,
    ) -> std::result::Result<(), DatabaseError> {
        let mut txn = db.inner.pool.begin().await?;

        sqlx::query("DELETE FROM mute_list")
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
                "INSERT OR IGNORE INTO mute_list (muted_pubkey, is_private, created_at)
                 VALUES (?, ?, ?)",
            )
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
    use tempfile::TempDir;

    use super::*;

    /// Build a per-account DB with the post-move local schema applied
    /// directly. Bypasses the migration framework — that's covered in the
    /// migration's own tests; here we exercise the ops in isolation.
    ///
    /// Schema must stay byte-for-byte aligned with
    /// `m0042_move_mute_list.rs` so test/prod don't drift.
    async fn create_test_db() -> (AccountDatabase, TempDir) {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("account.db");
        let pubkey = Keys::generate().public_key();
        let db = AccountDatabase::new(pubkey, db_path)
            .await
            .expect("Failed to create test account database");

        sqlx::query("DROP TABLE IF EXISTS mute_list")
            .execute(&db.inner.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE mute_list (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                muted_pubkey  TEXT NOT NULL UNIQUE
                    CHECK (length(muted_pubkey) = 64
                           AND muted_pubkey GLOB '[0-9a-fA-F]*'),
                is_private    INTEGER NOT NULL DEFAULT 1
                    CHECK (is_private IN (0, 1)),
                created_at    INTEGER NOT NULL
            )",
        )
        .execute(&db.inner.pool)
        .await
        .expect("Failed to create mute_list table");

        (db, temp_dir)
    }

    #[tokio::test]
    async fn insert_and_exists() {
        let (db, _tmp) = create_test_db().await;
        let target = Keys::generate().public_key();

        assert!(!MuteListEntry::exists(&target, &db).await.unwrap());

        MuteListEntry::insert(&target, true, &db).await.unwrap();

        assert!(MuteListEntry::exists(&target, &db).await.unwrap());
    }

    #[tokio::test]
    async fn insert_duplicate_is_ignored() {
        let (db, _tmp) = create_test_db().await;
        let target = Keys::generate().public_key();

        MuteListEntry::insert(&target, true, &db).await.unwrap();
        MuteListEntry::insert(&target, true, &db).await.unwrap();

        let entries = MuteListEntry::find_all(&db).await.unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let (db, _tmp) = create_test_db().await;
        let target = Keys::generate().public_key();

        MuteListEntry::insert(&target, true, &db).await.unwrap();
        MuteListEntry::delete(&target, &db).await.unwrap();

        assert!(!MuteListEntry::exists(&target, &db).await.unwrap());
    }

    #[tokio::test]
    async fn find_all_returns_only_owning_account_entries() {
        let (db1, _tmp1) = create_test_db().await;
        let (db2, _tmp2) = create_test_db().await;
        let target = Keys::generate().public_key();

        MuteListEntry::insert(&target, true, &db1).await.unwrap();
        MuteListEntry::insert(&target, false, &db2).await.unwrap();

        let entries = MuteListEntry::find_all(&db1).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_private);

        let entries = MuteListEntry::find_all(&db2).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].is_private);
    }

    #[tokio::test]
    async fn sync_from_event_replaces_all() {
        let (db, _tmp) = create_test_db().await;
        let old_target = Keys::generate().public_key();
        let new_target1 = Keys::generate().public_key();
        let new_target2 = Keys::generate().public_key();

        MuteListEntry::insert(&old_target, true, &db).await.unwrap();

        let new_entries = vec![(new_target1, true), (new_target2, false)];
        MuteListEntry::sync_from_event(&new_entries, &db)
            .await
            .unwrap();

        assert!(!MuteListEntry::exists(&old_target, &db).await.unwrap());
        assert!(MuteListEntry::exists(&new_target1, &db).await.unwrap());
        assert!(MuteListEntry::exists(&new_target2, &db).await.unwrap());

        let entries = MuteListEntry::find_all(&db).await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn sync_from_event_with_duplicate_pubkeys_is_safe() {
        let (db, _tmp) = create_test_db().await;
        let target = Keys::generate().public_key();

        // Same pubkey appears twice (e.g. in both public and private sections of one event)
        let entries = vec![(target, false), (target, true)];
        MuteListEntry::sync_from_event(&entries, &db).await.unwrap();

        let result = MuteListEntry::find_all(&db).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(MuteListEntry::exists(&target, &db).await.unwrap());
    }

    #[tokio::test]
    async fn sync_from_event_with_empty_list_clears_all() {
        let (db, _tmp) = create_test_db().await;
        let target = Keys::generate().public_key();

        MuteListEntry::insert(&target, true, &db).await.unwrap();

        MuteListEntry::sync_from_event(&[], &db).await.unwrap();

        assert!(!MuteListEntry::exists(&target, &db).await.unwrap());
        let entries = MuteListEntry::find_all(&db).await.unwrap();
        assert!(entries.is_empty());
    }
}
