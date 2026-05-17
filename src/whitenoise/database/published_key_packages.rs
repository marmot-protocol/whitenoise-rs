use nostr_sdk::Kind;

use super::DatabaseError;
use super::account_db::AccountDatabase;
use crate::perf_instrument;

/// Represents a published key package tracked for lifecycle management.
///
/// Tracks the full lifecycle of every key package from creation through cleanup:
/// 1. Created at publish time with `hash_ref` and `event_id`
/// 2. Marked as consumed when a Welcome referencing this KP is received
/// 3. Key material deleted by the maintenance task after a quiet period
///
/// Lives in the per-account SQLite file. The owning account is implicit —
/// it's whichever account owns the file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PublishedKeyPackage {
    pub id: i64,
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
        Ok(Self {
            id: row.try_get("id")?,
            key_package_hash_ref: row.try_get("key_package_hash_ref")?,
            event_id: row.try_get("event_id")?,
            kind: row.try_get("kind")?,
            d_tag: row.try_get("d_tag")?,
            consumed_at: row.try_get("consumed_at")?,
            key_material_deleted: row.try_get("key_material_deleted")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

impl PublishedKeyPackage {
    /// Returns the event kind as a typed [`Kind`].
    ///
    /// Published key package rows store custom Nostr kinds (`30443` and legacy
    /// `443`).
    pub fn kind(&self) -> Kind {
        Kind::Custom(u16::try_from(self.kind).unwrap_or_default())
    }

    /// Records a published key package for lifecycle tracking.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn create(
        db: &AccountDatabase,
        hash_ref: &[u8],
        event_id: &str,
        kind: Kind,
        d_tag: Option<&str>,
    ) -> Result<(), DatabaseError> {
        sqlx::query(
            "INSERT OR IGNORE INTO published_key_packages
             (key_package_hash_ref, event_id, kind, d_tag)
             VALUES (?, ?, ?, ?)",
        )
        .bind(hash_ref)
        .bind(event_id)
        .bind(i64::from(kind.as_u16()))
        .bind(d_tag)
        .execute(&db.inner.pool)
        .await?;

        tracing::debug!(
            target: "whitenoise::database::published_key_packages",
            "Tracked published key package for account {}",
            db.account_pubkey().to_hex()
        );

        Ok(())
    }

    /// Looks up a published key package by its event ID.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn find_by_event_id(
        db: &AccountDatabase,
        event_id: &str,
    ) -> Result<Option<Self>, DatabaseError> {
        let row = sqlx::query_as::<_, Self>(
            "SELECT id, key_package_hash_ref, event_id, kind, d_tag,
                    consumed_at, key_material_deleted, created_at
             FROM published_key_packages
             WHERE event_id = ?",
        )
        .bind(event_id)
        .fetch_optional(&db.inner.pool)
        .await?;

        Ok(row)
    }

    /// Returns the most recently inserted row for a given event kind, if any.
    ///
    /// Single query that covers both NIP-33 canonical-slot needs:
    /// - `d_tag` — reuse the previously-published addressable-slot
    ///   identifier so the next publish replaces the prior canonical event
    ///   on relays instead of landing in a fresh slot. MDK generates a
    ///   fresh random `d` tag every call to `create_key_package_for_event`
    ///   and the MDK docs say "Callers SHOULD store and reuse this value
    ///   when rotating the KeyPackage."
    /// - `created_at` (the row's SQLite `unixepoch()` insert timestamp,
    ///   not the Nostr event's `created_at`) — use as a lower bound when
    ///   computing a strictly-monotonic event `created_at` for the next
    ///   publish. NIP-01 says relays keep the lowest-id event on a tie,
    ///   so reusing the d-tag alone isn't enough under same-second
    ///   publishes. The row insert timestamp is always ≥ the event's
    ///   `created_at` (INSERT runs after the publish round-trip), which
    ///   makes it a sound upper bound for the prior event time.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn find_latest_by_kind(
        db: &AccountDatabase,
        kind: Kind,
    ) -> Result<Option<Self>, DatabaseError> {
        let row = sqlx::query_as::<_, Self>(
            "SELECT id, key_package_hash_ref, event_id, kind, d_tag,
                    consumed_at, key_material_deleted, created_at
             FROM published_key_packages
             WHERE kind = ?
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )
        .bind(i64::from(kind.as_u16()))
        .fetch_optional(&db.inner.pool)
        .await?;

        Ok(row)
    }

    /// Looks up all published key packages sharing the same hash reference.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn find_by_hash_ref(
        db: &AccountDatabase,
        hash_ref: &[u8],
    ) -> Result<Vec<Self>, DatabaseError> {
        let rows = sqlx::query_as::<_, Self>(
            "SELECT id, key_package_hash_ref, event_id, kind, d_tag,
                    consumed_at, key_material_deleted, created_at
             FROM published_key_packages
             WHERE key_package_hash_ref = ?
             ORDER BY created_at DESC, id DESC",
        )
        .bind(hash_ref)
        .fetch_all(&db.inner.pool)
        .await?;

        Ok(rows)
    }

    /// Marks a published key package as consumed (used by a Welcome).
    ///
    /// Updates `consumed_at` for all rows that share the same `key_package_hash_ref`
    /// as the given event, so canonical (kind:30443) and legacy (kind:443) twins
    /// are marked together. Returns `false` if no matching row exists or key
    /// material is already deleted.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn mark_consumed(
        db: &AccountDatabase,
        event_id: &str,
    ) -> Result<bool, DatabaseError> {
        let Some(package) = Self::find_by_event_id(db, event_id).await? else {
            return Ok(false);
        };

        if package.key_material_deleted {
            return Ok(false);
        }

        let result = sqlx::query(
            "UPDATE published_key_packages
             SET consumed_at = unixepoch()
             WHERE key_package_hash_ref = ? AND key_material_deleted = 0",
        )
        .bind(&package.key_package_hash_ref)
        .execute(&db.inner.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Returns all published key packages eligible for key material cleanup.
    ///
    /// A package is eligible when:
    /// - `consumed_at` is set (it was used by a Welcome)
    /// - `key_material_deleted` is 0 (key material hasn't been cleaned up yet)
    /// - ALL consumed packages have `consumed_at` older than `quiet_period_secs`
    ///   (no recent welcomes — the burst is over)
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn find_eligible_for_cleanup(
        db: &AccountDatabase,
        quiet_period_secs: i64,
    ) -> Result<Vec<Self>, DatabaseError> {
        let rows = sqlx::query_as::<_, Self>(
            "SELECT id, key_package_hash_ref, event_id, kind, d_tag,
                    consumed_at, key_material_deleted, created_at
             FROM published_key_packages
             WHERE consumed_at IS NOT NULL
               AND key_material_deleted = 0
               AND NOT EXISTS (
                   SELECT 1 FROM published_key_packages
                   WHERE consumed_at IS NOT NULL
                     AND key_material_deleted = 0
                     AND consumed_at > unixepoch() - ?
               )",
        )
        .bind(quiet_period_secs)
        .fetch_all(&db.inner.pool)
        .await?;

        Ok(rows)
    }

    /// Marks all rows sharing a key package hash as deleted.
    ///
    /// Dual-published kind:30443/kind:443 events point at the same local MLS
    /// key material, so cleanup must update the whole hash group together.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn mark_key_material_deleted_by_hash_ref(
        db: &AccountDatabase,
        hash_ref: &[u8],
    ) -> Result<u64, DatabaseError> {
        let result = sqlx::query(
            "UPDATE published_key_packages
             SET key_material_deleted = 1
             WHERE key_package_hash_ref = ?",
        )
        .bind(hash_ref)
        .execute(&db.inner.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// Marks a single key package row's key material as deleted by row id.
    ///
    /// Called after the maintenance task successfully deletes the local MLS
    /// key material. Rows are never deleted — the table serves as an audit trail.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn mark_key_material_deleted(
        db: &AccountDatabase,
        id: i64,
    ) -> Result<(), DatabaseError> {
        sqlx::query("UPDATE published_key_packages SET key_material_deleted = 1 WHERE id = ?")
            .bind(id)
            .execute(&db.inner.pool)
            .await?;
        Ok(())
    }

    /// Backdates the `consumed_at` timestamp for a published key package group.
    ///
    /// Test-only helper that shifts `consumed_at` into the past so the
    /// maintenance task considers it eligible for cleanup without waiting the
    /// full quiet period.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn backdate_consumed_at(
        db: &AccountDatabase,
        event_id: &str,
        age_secs: i64,
    ) -> Result<(), DatabaseError> {
        let Some(package) = Self::find_by_event_id(db, event_id).await? else {
            return Err(DatabaseError::NotFound(format!(
                "Published key package not found: {event_id}"
            )));
        };

        sqlx::query(
            "UPDATE published_key_packages SET consumed_at = unixepoch() - ?
             WHERE key_package_hash_ref = ?",
        )
        .bind(age_secs)
        .bind(&package.key_package_hash_ref)
        .execute(&db.inner.pool)
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;
    use tempfile::TempDir;

    use super::*;
    use crate::whitenoise::key_packages::{MLS_KEY_PACKAGE_KIND, MLS_KEY_PACKAGE_KIND_LEGACY};

    async fn setup() -> (AccountDatabase, TempDir) {
        let dir = TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let path = dir.path().join("acct.db");
        let db = AccountDatabase::new(pubkey, path).await.unwrap();

        sqlx::query("DROP TABLE IF EXISTS published_key_packages")
            .execute(&db.inner.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE published_key_packages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                key_package_hash_ref BLOB NOT NULL,
                event_id TEXT NOT NULL UNIQUE,
                kind INTEGER NOT NULL DEFAULT 443,
                d_tag TEXT NULL,
                consumed_at INTEGER,
                key_material_deleted INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch())
            )",
        )
        .execute(&db.inner.pool)
        .await
        .unwrap();

        (db, dir)
    }

    #[tokio::test]
    async fn test_create_and_find_by_event_id() {
        let (db, _dir) = setup().await;

        PublishedKeyPackage::create(&db, &[1, 2, 3], "evt", MLS_KEY_PACKAGE_KIND_LEGACY, None)
            .await
            .unwrap();

        let pkg = PublishedKeyPackage::find_by_event_id(&db, "evt")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pkg.event_id, "evt");
        assert_eq!(pkg.kind, 443);
        assert!(pkg.d_tag.is_none());
        assert!(pkg.consumed_at.is_none());
        assert!(!pkg.key_material_deleted);
    }

    #[tokio::test]
    async fn test_create_duplicate_event_id_ignored() {
        let (db, _dir) = setup().await;

        PublishedKeyPackage::create(&db, &[1, 2, 3], "evt", MLS_KEY_PACKAGE_KIND_LEGACY, None)
            .await
            .unwrap();
        PublishedKeyPackage::create(&db, &[1, 2, 3], "evt", MLS_KEY_PACKAGE_KIND_LEGACY, None)
            .await
            .unwrap();

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM published_key_packages")
            .fetch_one(&db.inner.pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn test_find_by_event_id_returns_none_for_unknown() {
        let (db, _dir) = setup().await;
        assert!(
            PublishedKeyPackage::find_by_event_id(&db, "missing")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_create_stores_kind_and_d_tag() {
        let (db, _dir) = setup().await;

        PublishedKeyPackage::create(
            &db,
            &[1, 2, 3],
            "canonical",
            MLS_KEY_PACKAGE_KIND,
            Some("d-tag"),
        )
        .await
        .unwrap();

        let pkg = PublishedKeyPackage::find_by_event_id(&db, "canonical")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pkg.kind, 30443);
        assert_eq!(pkg.d_tag.as_deref(), Some("d-tag"));
    }

    #[tokio::test]
    async fn test_mark_consumed_updates_timestamp_for_twins() {
        let (db, _dir) = setup().await;
        let hash = vec![1, 2, 3];

        PublishedKeyPackage::create(&db, &hash, "canonical", MLS_KEY_PACKAGE_KIND, Some("d-tag"))
            .await
            .unwrap();
        PublishedKeyPackage::create(&db, &hash, "legacy", MLS_KEY_PACKAGE_KIND_LEGACY, None)
            .await
            .unwrap();

        assert!(
            PublishedKeyPackage::mark_consumed(&db, "canonical")
                .await
                .unwrap()
        );

        let canonical = PublishedKeyPackage::find_by_event_id(&db, "canonical")
            .await
            .unwrap()
            .unwrap();
        let legacy = PublishedKeyPackage::find_by_event_id(&db, "legacy")
            .await
            .unwrap()
            .unwrap();
        assert!(canonical.consumed_at.is_some());
        assert_eq!(canonical.consumed_at, legacy.consumed_at);
    }

    #[tokio::test]
    async fn test_mark_consumed_returns_false_for_missing() {
        let (db, _dir) = setup().await;
        assert!(
            !PublishedKeyPackage::mark_consumed(&db, "nope")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_mark_consumed_returns_false_when_already_deleted() {
        let (db, _dir) = setup().await;

        PublishedKeyPackage::create(&db, &[1, 2, 3], "evt", MLS_KEY_PACKAGE_KIND_LEGACY, None)
            .await
            .unwrap();
        PublishedKeyPackage::mark_consumed(&db, "evt")
            .await
            .unwrap();
        PublishedKeyPackage::mark_key_material_deleted_by_hash_ref(&db, &[1, 2, 3])
            .await
            .unwrap();

        assert!(
            !PublishedKeyPackage::mark_consumed(&db, "evt")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_find_eligible_respects_quiet_period() {
        let (db, _dir) = setup().await;

        PublishedKeyPackage::create(&db, &[1, 2, 3], "recent", MLS_KEY_PACKAGE_KIND_LEGACY, None)
            .await
            .unwrap();
        PublishedKeyPackage::mark_consumed(&db, "recent")
            .await
            .unwrap();

        let eligible = PublishedKeyPackage::find_eligible_for_cleanup(&db, 30)
            .await
            .unwrap();
        assert!(eligible.is_empty(), "Recent → not eligible");
    }

    #[tokio::test]
    async fn test_find_eligible_returns_old_consumed_packages() {
        let (db, _dir) = setup().await;

        sqlx::query(
            "INSERT INTO published_key_packages \
                (key_package_hash_ref, event_id, consumed_at) \
             VALUES (?, ?, unixepoch() - 60)",
        )
        .bind(&[1u8, 2, 3] as &[u8])
        .bind("old")
        .execute(&db.inner.pool)
        .await
        .unwrap();

        let eligible = PublishedKeyPackage::find_eligible_for_cleanup(&db, 30)
            .await
            .unwrap();
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].event_id, "old");
    }

    #[tokio::test]
    async fn test_find_latest_by_kind_returns_none_when_empty() {
        let (db, _dir) = setup().await;

        let result = PublishedKeyPackage::find_latest_by_kind(&db, MLS_KEY_PACKAGE_KIND)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_find_latest_by_kind_returns_most_recent_row_per_kind() {
        let (db, _dir) = setup().await;

        PublishedKeyPackage::create(
            &db,
            &[1, 1, 1],
            "older",
            MLS_KEY_PACKAGE_KIND,
            Some("d-old"),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE published_key_packages SET created_at = 100 WHERE event_id = ?")
            .bind("older")
            .execute(&db.inner.pool)
            .await
            .unwrap();

        PublishedKeyPackage::create(
            &db,
            &[2, 2, 2],
            "newer",
            MLS_KEY_PACKAGE_KIND,
            Some("d-new"),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE published_key_packages SET created_at = 200 WHERE event_id = ?")
            .bind("newer")
            .execute(&db.inner.pool)
            .await
            .unwrap();

        // A legacy row inserted later must not bleed into the canonical lookup.
        PublishedKeyPackage::create(&db, &[3, 3, 3], "legacy", MLS_KEY_PACKAGE_KIND_LEGACY, None)
            .await
            .unwrap();
        sqlx::query("UPDATE published_key_packages SET created_at = 999 WHERE event_id = ?")
            .bind("legacy")
            .execute(&db.inner.pool)
            .await
            .unwrap();

        let canonical = PublishedKeyPackage::find_latest_by_kind(&db, MLS_KEY_PACKAGE_KIND)
            .await
            .unwrap()
            .expect("canonical row must exist");
        assert_eq!(canonical.event_id, "newer");
        assert_eq!(canonical.d_tag.as_deref(), Some("d-new"));
        assert_eq!(canonical.created_at, 200);

        let legacy = PublishedKeyPackage::find_latest_by_kind(&db, MLS_KEY_PACKAGE_KIND_LEGACY)
            .await
            .unwrap()
            .expect("legacy row must exist");
        assert_eq!(legacy.event_id, "legacy");
        assert_eq!(legacy.created_at, 999);
        assert!(legacy.d_tag.is_none());
    }

    #[tokio::test]
    async fn test_mark_key_material_deleted_by_hash_ref_marks_twins() {
        let (db, _dir) = setup().await;
        let hash = vec![1, 2, 3];

        PublishedKeyPackage::create(&db, &hash, "canonical", MLS_KEY_PACKAGE_KIND, Some("d-tag"))
            .await
            .unwrap();
        PublishedKeyPackage::create(&db, &hash, "legacy", MLS_KEY_PACKAGE_KIND_LEGACY, None)
            .await
            .unwrap();

        let affected = PublishedKeyPackage::mark_key_material_deleted_by_hash_ref(&db, &hash)
            .await
            .unwrap();
        assert_eq!(affected, 2);

        for evt in &["canonical", "legacy"] {
            let pkg = PublishedKeyPackage::find_by_event_id(&db, evt)
                .await
                .unwrap()
                .unwrap();
            assert!(pkg.key_material_deleted);
        }
    }
}
