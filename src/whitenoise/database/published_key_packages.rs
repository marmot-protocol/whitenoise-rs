use nostr_sdk::{Kind, PublicKey};

use super::{Database, DatabaseError};
use crate::perf_instrument;

/// Represents a published key package tracked for lifecycle management.
///
/// Tracks the full lifecycle of every key package from creation through cleanup:
/// 1. Created at publish time with hash_ref and event_id
/// 2. Marked as consumed when a Welcome referencing this KP is received
/// 3. Key material deleted by the maintenance task after a quiet period
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PublishedKeyPackage {
    pub id: i64,
    pub account_pubkey: String,
    pub key_package_hash_ref: Vec<u8>,
    pub event_id: String,
    pub kind: i64,
    pub d_tag: Option<String>,
    pub consumed_at: Option<i64>,
    pub key_material_deleted: bool,
    pub created_at: i64,
}

impl<'r, R> sqlx::FromRow<'r, R> for PublishedKeyPackage
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Vec<u8>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    bool: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<String>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let account_pubkey: String = row.try_get("account_pubkey")?;
        let key_package_hash_ref: Vec<u8> = row.try_get("key_package_hash_ref")?;
        let event_id: String = row.try_get("event_id")?;
        let kind: i64 = row.try_get("kind")?;
        let d_tag: Option<String> = row.try_get("d_tag")?;
        let consumed_at: Option<i64> = row.try_get("consumed_at")?;
        let key_material_deleted: bool = row.try_get("key_material_deleted")?;
        let created_at: i64 = row.try_get("created_at")?;

        Ok(Self {
            id,
            account_pubkey,
            key_package_hash_ref,
            event_id,
            kind,
            d_tag,
            consumed_at,
            key_material_deleted,
            created_at,
        })
    }
}

impl PublishedKeyPackage {
    /// Returns the event kind as a typed [`Kind`].
    ///
    /// Published key package rows store custom Nostr kinds (`30443` and legacy
    /// `443`). Standard NIP kinds would need different handling.
    pub fn kind(&self) -> Kind {
        Kind::Custom(u16::try_from(self.kind).unwrap_or_default())
    }

    /// Records a published key package for lifecycle tracking.
    ///
    /// Called at publish time with the hash_ref computed atomically during
    /// key package creation. Fire-and-forget: if this fails, the KP is still
    /// functional on relays, we just lose cleanup tracking for this one.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn create(
        account_pubkey: &PublicKey,
        hash_ref: &[u8],
        event_id: &str,
        kind: Kind,
        d_tag: Option<&str>,
        database: &Database,
    ) -> Result<(), DatabaseError> {
        sqlx::query(
            "INSERT OR IGNORE INTO published_key_packages
             (account_pubkey, key_package_hash_ref, event_id, kind, d_tag)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(account_pubkey.to_hex())
        .bind(hash_ref)
        .bind(event_id)
        .bind(i64::from(kind.as_u16()))
        .bind(d_tag)
        .execute(&database.pool)
        .await?;

        tracing::debug!(
            target: "whitenoise::database::published_key_packages",
            "Tracked published key package for account {}",
            account_pubkey.to_hex()
        );

        Ok(())
    }

    /// Looks up a published key package by its event ID.
    ///
    /// Used as a pre-check before processing a Welcome to determine whether
    /// we have this key package and whether its key material is still available.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn find_by_event_id(
        account_pubkey: &PublicKey,
        event_id: &str,
        database: &Database,
    ) -> Result<Option<Self>, DatabaseError> {
        let row = sqlx::query_as::<_, Self>(
            "SELECT id, account_pubkey, key_package_hash_ref, event_id, kind, d_tag,
                    consumed_at, key_material_deleted, created_at
             FROM published_key_packages
             WHERE account_pubkey = ? AND event_id = ?",
        )
        .bind(account_pubkey.to_hex())
        .bind(event_id)
        .fetch_optional(&database.pool)
        .await?;

        Ok(row)
    }

    /// Looks up all published key packages sharing the same hash reference.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn find_by_hash_ref(
        account_pubkey: &PublicKey,
        hash_ref: &[u8],
        database: &Database,
    ) -> Result<Vec<Self>, DatabaseError> {
        let rows = sqlx::query_as::<_, Self>(
            "SELECT id, account_pubkey, key_package_hash_ref, event_id, kind, d_tag,
                    consumed_at, key_material_deleted, created_at
             FROM published_key_packages
             WHERE account_pubkey = ? AND key_package_hash_ref = ?
             ORDER BY created_at DESC, id DESC",
        )
        .bind(account_pubkey.to_hex())
        .bind(hash_ref)
        .fetch_all(&database.pool)
        .await?;

        Ok(rows)
    }

    /// Marks a published key package as consumed (used by a Welcome).
    ///
    /// Updates `consumed_at` for all rows that share the same `key_package_hash_ref`
    /// as the given event, so both the canonical (kind:30443) and legacy (kind:443) twins
    /// are marked together. A KP can be consumed multiple times (last-resort reuse);
    /// each Welcome restarts the quiet period before key material cleanup.
    ///
    /// Returns `false` if no matching row exists or key material is already deleted.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn mark_consumed(
        account_pubkey: &PublicKey,
        event_id: &str,
        database: &Database,
    ) -> Result<bool, DatabaseError> {
        let row = Self::find_by_event_id(account_pubkey, event_id, database).await?;
        let Some(package) = row else {
            return Ok(false);
        };

        if package.key_material_deleted {
            return Ok(false);
        }

        let result = sqlx::query(
            "UPDATE published_key_packages
             SET consumed_at = unixepoch()
             WHERE account_pubkey = ? AND key_package_hash_ref = ? AND key_material_deleted = 0",
        )
        .bind(account_pubkey.to_hex())
        .bind(&package.key_package_hash_ref)
        .execute(&database.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Returns all published key packages eligible for key material cleanup.
    ///
    /// A package is eligible when:
    /// - `consumed_at` is set (it was used by a Welcome)
    /// - `key_material_deleted` is 0 (key material hasn't been cleaned up yet)
    /// - ALL consumed packages for this account have `consumed_at` older than
    ///   `quiet_period_secs` (no recent welcomes — the burst is over)
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn find_eligible_for_cleanup(
        account_pubkey: &PublicKey,
        quiet_period_secs: i64,
        database: &Database,
    ) -> Result<Vec<Self>, DatabaseError> {
        let rows = sqlx::query_as::<_, Self>(
            "SELECT id, account_pubkey, key_package_hash_ref, event_id, kind, d_tag,
                    consumed_at, key_material_deleted, created_at
             FROM published_key_packages
             WHERE account_pubkey = ?
               AND consumed_at IS NOT NULL
               AND key_material_deleted = 0
               AND NOT EXISTS (
                   SELECT 1 FROM published_key_packages
                   WHERE account_pubkey = ?
                     AND consumed_at IS NOT NULL
                     AND key_material_deleted = 0
                     AND consumed_at > unixepoch() - ?
               )",
        )
        .bind(account_pubkey.to_hex())
        .bind(account_pubkey.to_hex())
        .bind(quiet_period_secs)
        .fetch_all(&database.pool)
        .await?;

        Ok(rows)
    }

    /// Marks all rows sharing a key package hash as deleted.
    ///
    /// Dual-published kind:30443/kind:443 events point at the same local MLS
    /// key material, so cleanup must update the whole hash group together.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn mark_key_material_deleted_by_hash_ref(
        account_pubkey: &PublicKey,
        hash_ref: &[u8],
        database: &Database,
    ) -> Result<u64, DatabaseError> {
        let result = sqlx::query(
            "UPDATE published_key_packages
             SET key_material_deleted = 1
             WHERE account_pubkey = ? AND key_package_hash_ref = ?",
        )
        .bind(account_pubkey.to_hex())
        .bind(hash_ref)
        .execute(&database.pool)
        .await?;

        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::whitenoise::key_packages::{MLS_KEY_PACKAGE_KIND, MLS_KEY_PACKAGE_KIND_LEGACY};

    async fn setup_test_db() -> Database {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE published_key_packages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_pubkey TEXT NOT NULL,
                key_package_hash_ref BLOB NOT NULL,
                event_id TEXT NOT NULL,
                kind INTEGER NOT NULL DEFAULT 443,
                d_tag TEXT NULL,
                consumed_at INTEGER,
                key_material_deleted INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                UNIQUE(account_pubkey, event_id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        Database {
            pool,
            path: std::path::PathBuf::from(":memory:"),
            last_connected: std::time::SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn test_create_inserts_row() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let hash_ref = vec![1, 2, 3, 4, 5];
        let event_id = "abc123";

        let result = PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            event_id,
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await;
        assert!(result.is_ok());

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM published_key_packages WHERE account_pubkey = ?")
                .bind(pubkey.to_hex())
                .fetch_one(&db.pool)
                .await
                .unwrap();

        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn test_create_duplicate_event_id_ignored() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let hash_ref = vec![1, 2, 3, 4, 5];
        let event_id = "abc123";

        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            event_id,
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();
        // Second insert with same event_id should be ignored (INSERT OR IGNORE)
        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            event_id,
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM published_key_packages WHERE account_pubkey = ?")
                .bind(pubkey.to_hex())
                .fetch_one(&db.pool)
                .await
                .unwrap();

        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn test_find_by_event_id_returns_none_for_unknown() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();

        let result = PublishedKeyPackage::find_by_event_id(&pubkey, "unknown", &db)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_find_by_event_id_returns_some_for_known() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let hash_ref = vec![1, 2, 3];
        let event_id = "known_event";

        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            event_id,
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();

        let result = PublishedKeyPackage::find_by_event_id(&pubkey, event_id, &db)
            .await
            .unwrap();
        assert!(result.is_some());

        let pkg = result.unwrap();
        assert_eq!(pkg.key_package_hash_ref, hash_ref);
        assert_eq!(pkg.event_id, event_id);
        assert_eq!(pkg.kind, 443);
        assert_eq!(pkg.d_tag, None);
        assert!(pkg.consumed_at.is_none());
        assert!(!pkg.key_material_deleted);
    }

    #[tokio::test]
    async fn test_create_stores_kind_and_d_tag() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let hash_ref = vec![1, 2, 3];
        let d_tag = "d-tag-value";

        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            "canonical_event",
            MLS_KEY_PACKAGE_KIND,
            Some(d_tag),
            &db,
        )
        .await
        .unwrap();

        let pkg = PublishedKeyPackage::find_by_event_id(&pubkey, "canonical_event", &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pkg.kind, 30443);
        assert_eq!(pkg.d_tag.as_deref(), Some(d_tag));
    }

    #[tokio::test]
    async fn test_mark_consumed_updates_timestamp() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let event_id = "consume_test";

        PublishedKeyPackage::create(
            &pubkey,
            &[1, 2, 3],
            event_id,
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();

        let updated = PublishedKeyPackage::mark_consumed(&pubkey, event_id, &db)
            .await
            .unwrap();
        assert!(updated);

        let pkg = PublishedKeyPackage::find_by_event_id(&pubkey, event_id, &db)
            .await
            .unwrap()
            .unwrap();
        assert!(pkg.consumed_at.is_some());
    }

    #[tokio::test]
    async fn test_mark_consumed_marks_key_package_twins() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let hash_ref = vec![1, 2, 3];

        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            "canonical_event",
            MLS_KEY_PACKAGE_KIND,
            Some("d-tag-value"),
            &db,
        )
        .await
        .unwrap();
        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            "legacy_event",
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();

        let updated = PublishedKeyPackage::mark_consumed(&pubkey, "canonical_event", &db)
            .await
            .unwrap();
        assert!(updated);

        let canonical = PublishedKeyPackage::find_by_event_id(&pubkey, "canonical_event", &db)
            .await
            .unwrap()
            .unwrap();
        let legacy = PublishedKeyPackage::find_by_event_id(&pubkey, "legacy_event", &db)
            .await
            .unwrap()
            .unwrap();

        assert!(canonical.consumed_at.is_some());
        assert_eq!(legacy.consumed_at, canonical.consumed_at);
    }

    #[tokio::test]
    async fn test_mark_consumed_marks_canonical_when_legacy_consumed() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let hash_ref = vec![1, 2, 3];

        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            "canonical_event",
            MLS_KEY_PACKAGE_KIND,
            Some("d-tag-value"),
            &db,
        )
        .await
        .unwrap();
        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            "legacy_event",
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();

        let updated = PublishedKeyPackage::mark_consumed(&pubkey, "legacy_event", &db)
            .await
            .unwrap();
        assert!(updated);

        let canonical = PublishedKeyPackage::find_by_event_id(&pubkey, "canonical_event", &db)
            .await
            .unwrap()
            .unwrap();
        let legacy = PublishedKeyPackage::find_by_event_id(&pubkey, "legacy_event", &db)
            .await
            .unwrap()
            .unwrap();

        assert!(legacy.consumed_at.is_some());
        assert_eq!(canonical.consumed_at, legacy.consumed_at);
    }

    #[tokio::test]
    async fn test_mark_consumed_returns_false_for_missing() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();

        let updated = PublishedKeyPackage::mark_consumed(&pubkey, "nonexistent", &db)
            .await
            .unwrap();
        assert!(!updated);
    }

    #[tokio::test]
    async fn test_mark_consumed_on_already_consumed_updates_timestamp_for_twins() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let hash_ref = vec![1, 2, 3];

        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            "canonical_burst",
            MLS_KEY_PACKAGE_KIND,
            Some("d-tag"),
            &db,
        )
        .await
        .unwrap();
        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            "legacy_burst",
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();

        // First consumption via canonical event_id
        PublishedKeyPackage::mark_consumed(&pubkey, "canonical_burst", &db)
            .await
            .unwrap();
        let first_canonical =
            PublishedKeyPackage::find_by_event_id(&pubkey, "canonical_burst", &db)
                .await
                .unwrap()
                .unwrap();
        let first_legacy = PublishedKeyPackage::find_by_event_id(&pubkey, "legacy_burst", &db)
            .await
            .unwrap()
            .unwrap();

        // Both twins should be consumed with the same timestamp
        assert_eq!(first_canonical.consumed_at, first_legacy.consumed_at);

        // Second consumption (burst) via legacy event_id — both twins must update
        let updated = PublishedKeyPackage::mark_consumed(&pubkey, "legacy_burst", &db)
            .await
            .unwrap();
        assert!(updated);

        let second_canonical =
            PublishedKeyPackage::find_by_event_id(&pubkey, "canonical_burst", &db)
                .await
                .unwrap()
                .unwrap();
        let second_legacy = PublishedKeyPackage::find_by_event_id(&pubkey, "legacy_burst", &db)
            .await
            .unwrap()
            .unwrap();

        assert!(second_canonical.consumed_at.unwrap() >= first_canonical.consumed_at.unwrap());
        assert_eq!(second_canonical.consumed_at, second_legacy.consumed_at);
    }

    #[tokio::test]
    async fn test_mark_consumed_on_already_consumed_updates_timestamp() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let event_id = "burst_test";

        PublishedKeyPackage::create(
            &pubkey,
            &[1, 2, 3],
            event_id,
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();

        // First consumption
        PublishedKeyPackage::mark_consumed(&pubkey, event_id, &db)
            .await
            .unwrap();
        let first = PublishedKeyPackage::find_by_event_id(&pubkey, event_id, &db)
            .await
            .unwrap()
            .unwrap();

        // Second consumption (burst scenario) - should update timestamp
        let updated = PublishedKeyPackage::mark_consumed(&pubkey, event_id, &db)
            .await
            .unwrap();
        assert!(updated);

        let second = PublishedKeyPackage::find_by_event_id(&pubkey, event_id, &db)
            .await
            .unwrap()
            .unwrap();
        assert!(second.consumed_at.unwrap() >= first.consumed_at.unwrap());
    }

    #[tokio::test]
    async fn test_mark_consumed_on_deleted_returns_false() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let event_id = "deleted_test";

        PublishedKeyPackage::create(
            &pubkey,
            &[1, 2, 3],
            event_id,
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();
        PublishedKeyPackage::mark_consumed(&pubkey, event_id, &db)
            .await
            .unwrap();

        PublishedKeyPackage::mark_key_material_deleted_by_hash_ref(&pubkey, &[1, 2, 3], &db)
            .await
            .unwrap();

        // mark_consumed should return false for deleted KP
        let updated = PublishedKeyPackage::mark_consumed(&pubkey, event_id, &db)
            .await
            .unwrap();
        assert!(!updated);
    }

    #[tokio::test]
    async fn test_find_eligible_respects_quiet_period() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();

        // Insert a consumed KP with recent timestamp
        PublishedKeyPackage::create(
            &pubkey,
            &[1, 2, 3],
            "recent",
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();
        PublishedKeyPackage::mark_consumed(&pubkey, "recent", &db)
            .await
            .unwrap();

        // Should not be eligible yet (consumed just now)
        let eligible = PublishedKeyPackage::find_eligible_for_cleanup(&pubkey, 30, &db)
            .await
            .unwrap();
        assert!(
            eligible.is_empty(),
            "Recently consumed packages should not be eligible"
        );
    }

    #[tokio::test]
    async fn test_find_eligible_returns_old_consumed_packages() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();

        // Insert a consumed KP with old timestamp
        sqlx::query(
            "INSERT INTO published_key_packages (account_pubkey, key_package_hash_ref, event_id, consumed_at)
             VALUES (?, ?, ?, unixepoch() - 60)",
        )
        .bind(pubkey.to_hex())
        .bind(&[1u8, 2, 3] as &[u8])
        .bind("old_event")
        .execute(&db.pool)
        .await
        .unwrap();

        let eligible = PublishedKeyPackage::find_eligible_for_cleanup(&pubkey, 30, &db)
            .await
            .unwrap();
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].event_id, "old_event");
    }

    #[tokio::test]
    async fn test_find_eligible_blocked_by_recent_consumption() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();

        // One old consumed KP
        sqlx::query(
            "INSERT INTO published_key_packages (account_pubkey, key_package_hash_ref, event_id, consumed_at)
             VALUES (?, ?, ?, unixepoch() - 60)",
        )
        .bind(pubkey.to_hex())
        .bind(&[1u8, 2, 3] as &[u8])
        .bind("old_event")
        .execute(&db.pool)
        .await
        .unwrap();

        // One recently consumed KP (blocks all cleanup for this account)
        PublishedKeyPackage::create(
            &pubkey,
            &[4, 5, 6],
            "recent_event",
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();
        PublishedKeyPackage::mark_consumed(&pubkey, "recent_event", &db)
            .await
            .unwrap();

        let eligible = PublishedKeyPackage::find_eligible_for_cleanup(&pubkey, 30, &db)
            .await
            .unwrap();
        assert!(
            eligible.is_empty(),
            "No packages should be eligible when a recent one exists"
        );
    }

    #[tokio::test]
    async fn test_mark_key_material_deleted_by_hash_ref() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let canonical_event_id = "delete_test_canonical";
        let legacy_event_id = "delete_test_legacy";
        let hash_ref = [1, 2, 3];

        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            canonical_event_id,
            MLS_KEY_PACKAGE_KIND,
            Some("test-d-tag"),
            &db,
        )
        .await
        .unwrap();
        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            legacy_event_id,
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();

        let canonical = PublishedKeyPackage::find_by_event_id(&pubkey, canonical_event_id, &db)
            .await
            .unwrap()
            .unwrap();
        let legacy = PublishedKeyPackage::find_by_event_id(&pubkey, legacy_event_id, &db)
            .await
            .unwrap()
            .unwrap();
        assert!(!canonical.key_material_deleted);
        assert!(!legacy.key_material_deleted);

        let updated =
            PublishedKeyPackage::mark_key_material_deleted_by_hash_ref(&pubkey, &hash_ref, &db)
                .await
                .unwrap();
        assert_eq!(updated, 2);

        let canonical = PublishedKeyPackage::find_by_event_id(&pubkey, canonical_event_id, &db)
            .await
            .unwrap()
            .unwrap();
        let legacy = PublishedKeyPackage::find_by_event_id(&pubkey, legacy_event_id, &db)
            .await
            .unwrap()
            .unwrap();
        assert!(canonical.key_material_deleted);
        assert!(legacy.key_material_deleted);
    }

    #[tokio::test]
    async fn test_mark_key_material_deleted_by_hash_ref_marks_twins() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();
        let hash_ref = vec![1, 2, 3];

        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            "canonical_event",
            MLS_KEY_PACKAGE_KIND,
            Some("d-tag-value"),
            &db,
        )
        .await
        .unwrap();
        PublishedKeyPackage::create(
            &pubkey,
            &hash_ref,
            "legacy_event",
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();

        let affected =
            PublishedKeyPackage::mark_key_material_deleted_by_hash_ref(&pubkey, &hash_ref, &db)
                .await
                .unwrap();
        assert_eq!(affected, 2);

        let canonical = PublishedKeyPackage::find_by_event_id(&pubkey, "canonical_event", &db)
            .await
            .unwrap()
            .unwrap();
        let legacy = PublishedKeyPackage::find_by_event_id(&pubkey, "legacy_event", &db)
            .await
            .unwrap()
            .unwrap();

        assert!(canonical.key_material_deleted);
        assert!(legacy.key_material_deleted);
    }

    #[tokio::test]
    async fn test_account_isolation() {
        let db = setup_test_db().await;
        let pubkey1 = Keys::generate().public_key();
        let pubkey2 = Keys::generate().public_key();

        // Insert old consumed KP for account 1
        sqlx::query(
            "INSERT INTO published_key_packages (account_pubkey, key_package_hash_ref, event_id, consumed_at)
             VALUES (?, ?, ?, unixepoch() - 60)",
        )
        .bind(pubkey1.to_hex())
        .bind(&[1u8, 2, 3] as &[u8])
        .bind("event_a1")
        .execute(&db.pool)
        .await
        .unwrap();

        // Insert recently consumed KP for account 2
        PublishedKeyPackage::create(
            &pubkey2,
            &[4, 5, 6],
            "event_b1",
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();
        PublishedKeyPackage::mark_consumed(&pubkey2, "event_b1", &db)
            .await
            .unwrap();

        // Account 1 should have eligible packages
        let eligible1 = PublishedKeyPackage::find_eligible_for_cleanup(&pubkey1, 30, &db)
            .await
            .unwrap();
        assert_eq!(eligible1.len(), 1);

        // Account 2 should not (recently consumed)
        let eligible2 = PublishedKeyPackage::find_eligible_for_cleanup(&pubkey2, 30, &db)
            .await
            .unwrap();
        assert!(eligible2.is_empty());

        // find_by_event_id should be account-scoped
        let result = PublishedKeyPackage::find_by_event_id(&pubkey1, "event_b1", &db)
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "Should not find account 2's event under account 1"
        );
    }

    #[tokio::test]
    async fn test_unconsumed_packages_not_eligible_for_cleanup() {
        let db = setup_test_db().await;
        let pubkey = Keys::generate().public_key();

        // Insert a published but unconsumed KP
        PublishedKeyPackage::create(
            &pubkey,
            &[1, 2, 3],
            "unconsumed",
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &db,
        )
        .await
        .unwrap();

        let eligible = PublishedKeyPackage::find_eligible_for_cleanup(&pubkey, 30, &db)
            .await
            .unwrap();
        assert!(
            eligible.is_empty(),
            "Unconsumed packages should never be eligible for cleanup"
        );
    }
}
