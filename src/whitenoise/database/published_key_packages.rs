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
    pub key_package_ref: Option<Vec<u8>>,
    pub key_package_content: Option<String>,
    pub event_id: String,
    pub kind: i64,
    pub d_tag: Option<String>,
    pub app_components: Vec<String>,
    pub package_version: i64,
    pub package_role: PublishedKeyPackageRole,
    pub consumed_at: Option<i64>,
    pub key_material_deleted: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PublishedKeyPackageRole {
    Legacy,
    LastResort,
    Rotated,
}

impl PublishedKeyPackageRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::LastResort => "last_resort",
            Self::Rotated => "rotated",
        }
    }
}

impl TryFrom<&str> for PublishedKeyPackageRole {
    type Error = String;

    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        match value {
            "legacy" => Ok(Self::Legacy),
            "last_resort" => Ok(Self::LastResort),
            "rotated" => Ok(Self::Rotated),
            other => Err(format!("unknown published key package role: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedKeyPackageProtocolData {
    pub key_package_ref: Option<Vec<u8>>,
    pub key_package_content: Option<String>,
    pub app_components: Vec<String>,
    pub package_version: i64,
    pub package_role: PublishedKeyPackageRole,
}

impl PublishedKeyPackageProtocolData {
    pub fn legacy() -> Self {
        Self {
            key_package_ref: None,
            key_package_content: None,
            app_components: Vec::new(),
            package_version: 1,
            package_role: PublishedKeyPackageRole::Legacy,
        }
    }

    pub fn darkmatter_v2_last_resort(
        key_package_ref: Vec<u8>,
        key_package_content: String,
        app_components: Vec<String>,
    ) -> Self {
        Self {
            key_package_ref: Some(key_package_ref),
            key_package_content: Some(key_package_content),
            app_components,
            package_version: 2,
            package_role: PublishedKeyPackageRole::LastResort,
        }
    }

    pub fn darkmatter_v2_rotated(
        key_package_ref: Vec<u8>,
        key_package_content: String,
        app_components: Vec<String>,
    ) -> Self {
        Self {
            key_package_ref: Some(key_package_ref),
            key_package_content: Some(key_package_content),
            app_components,
            package_version: 2,
            package_role: PublishedKeyPackageRole::Rotated,
        }
    }
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
    Option<Vec<u8>>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let app_components_json: String = row.try_get("app_components")?;
        let app_components: Vec<String> =
            serde_json::from_str(&app_components_json).map_err(|e| sqlx::Error::ColumnDecode {
                index: "app_components".to_string(),
                source: Box::new(e),
            })?;
        let package_role_raw: String = row.try_get("package_role")?;
        let package_role =
            PublishedKeyPackageRole::try_from(package_role_raw.as_str()).map_err(|e| {
                sqlx::Error::ColumnDecode {
                    index: "package_role".to_string(),
                    source: Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                }
            })?;

        Ok(Self {
            id: row.try_get("id")?,
            key_package_hash_ref: row.try_get("key_package_hash_ref")?,
            key_package_ref: row.try_get("key_package_ref")?,
            key_package_content: row.try_get("key_package_content")?,
            event_id: row.try_get("event_id")?,
            kind: row.try_get("kind")?,
            d_tag: row.try_get("d_tag")?,
            app_components,
            package_version: row.try_get("package_version")?,
            package_role,
            consumed_at: row.try_get("consumed_at")?,
            key_material_deleted: row.try_get("key_material_deleted")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

impl PublishedKeyPackage {
    /// Returns the event kind as a typed [`Kind`].
    ///
    /// Published key package rows store custom Nostr kinds.
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
        Self::create_with_protocol_data(
            db,
            hash_ref,
            event_id,
            kind,
            d_tag,
            PublishedKeyPackageProtocolData::legacy(),
        )
        .await
    }

    /// Records a published key package with protocol-specific metadata.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn create_with_protocol_data(
        db: &AccountDatabase,
        hash_ref: &[u8],
        event_id: &str,
        kind: Kind,
        d_tag: Option<&str>,
        protocol_data: PublishedKeyPackageProtocolData,
    ) -> Result<(), DatabaseError> {
        let app_components = serde_json::to_string(&protocol_data.app_components)?;
        sqlx::query(
            "INSERT OR IGNORE INTO published_key_packages
             (key_package_hash_ref, key_package_ref, key_package_content, event_id, kind, d_tag,
              app_components, package_version, package_role)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(hash_ref)
        .bind(protocol_data.key_package_ref)
        .bind(protocol_data.key_package_content)
        .bind(event_id)
        .bind(i64::from(kind.as_u16()))
        .bind(d_tag)
        .bind(app_components)
        .bind(protocol_data.package_version)
        .bind(protocol_data.package_role.as_str())
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
            "SELECT id, key_package_hash_ref, key_package_ref, key_package_content,
                    event_id, kind, d_tag,
                    app_components, package_version, package_role,
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
    ///   on relays instead of landing in a fresh slot. Without that reuse,
    ///   rotations can leave older key packages discoverable as current
    ///   relay state.
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
            "SELECT id, key_package_hash_ref, key_package_ref, key_package_content,
                    event_id, kind, d_tag,
                    app_components, package_version, package_role,
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
            "SELECT id, key_package_hash_ref, key_package_ref, key_package_content,
                    event_id, kind, d_tag,
                    app_components, package_version, package_role,
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

    #[cfg(feature = "integration-tests")]
    pub(crate) async fn find_consumed(db: &AccountDatabase) -> Result<Vec<Self>, DatabaseError> {
        let rows = sqlx::query_as::<_, Self>(
            "SELECT id, key_package_hash_ref, key_package_ref, key_package_content,
                    event_id, kind, d_tag,
                    app_components, package_version, package_role,
                    consumed_at, key_material_deleted, created_at
             FROM published_key_packages
             WHERE consumed_at IS NOT NULL
             ORDER BY consumed_at DESC, id DESC",
        )
        .fetch_all(&db.inner.pool)
        .await?;

        Ok(rows)
    }

    /// Marks a published key package as consumed (used by a Welcome).
    ///
    /// Updates `consumed_at` for all rows that share the same
    /// `key_package_hash_ref` as the given event. Returns `false` if no
    /// matching row exists or key material is already deleted.
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
            "SELECT id, key_package_hash_ref, key_package_ref, key_package_content,
                    event_id, kind, d_tag,
                    app_components, package_version, package_role,
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

    /// Returns true if any consumed key package still falls within the quiet period.
    #[perf_instrument("db::published_key_packages")]
    pub(crate) async fn has_consumed_since(
        db: &AccountDatabase,
        quiet_period_secs: i64,
    ) -> Result<bool, DatabaseError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM published_key_packages
                WHERE consumed_at IS NOT NULL
                  AND key_material_deleted = 0
                  AND consumed_at > unixepoch() - ?
             )",
        )
        .bind(quiet_period_secs)
        .fetch_one(&db.inner.pool)
        .await?;

        Ok(count != 0)
    }

    /// Marks all rows sharing a key package hash as deleted.
    ///
    /// Multiple published rows can point at the same local MLS key material,
    /// so cleanup must update the whole hash group together.
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
    use nostr_sdk::{Keys, Kind};
    use tempfile::TempDir;

    use super::*;
    use crate::whitenoise::key_packages::MLS_KEY_PACKAGE_KIND;

    const OTHER_KEY_PACKAGE_KIND: Kind = Kind::Custom(30000);

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
                key_package_ref BLOB NULL,
                key_package_content TEXT NULL,
                event_id TEXT NOT NULL UNIQUE,
                kind INTEGER NOT NULL DEFAULT 30443,
                d_tag TEXT NULL,
                app_components TEXT NOT NULL DEFAULT '[]',
                package_version INTEGER NOT NULL DEFAULT 1,
                package_role TEXT NOT NULL DEFAULT 'legacy'
                    CHECK (package_role IN ('legacy', 'last_resort', 'rotated')),
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

        PublishedKeyPackage::create(&db, &[1, 2, 3], "evt", MLS_KEY_PACKAGE_KIND, Some("d-tag"))
            .await
            .unwrap();

        let pkg = PublishedKeyPackage::find_by_event_id(&db, "evt")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pkg.event_id, "evt");
        assert_eq!(pkg.kind, 30443);
        assert_eq!(pkg.d_tag.as_deref(), Some("d-tag"));
        assert!(pkg.consumed_at.is_none());
        assert!(!pkg.key_material_deleted);
    }

    #[tokio::test]
    async fn test_create_duplicate_event_id_ignored() {
        let (db, _dir) = setup().await;

        PublishedKeyPackage::create(&db, &[1, 2, 3], "evt", MLS_KEY_PACKAGE_KIND, Some("d-tag"))
            .await
            .unwrap();
        PublishedKeyPackage::create(&db, &[1, 2, 3], "evt", MLS_KEY_PACKAGE_KIND, Some("d-tag"))
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
    async fn create_stores_darkmatter_v2_metadata_separately_from_slot() {
        let (db, _dir) = setup().await;
        let key_package_ref = vec![0xAB; 32];
        let app_components = vec![
            "0x8001".to_string(),
            "0x8003".to_string(),
            "0x8004".to_string(),
        ];

        PublishedKeyPackage::create_with_protocol_data(
            &db,
            &[0xCD; 32],
            "darkmatter-event",
            MLS_KEY_PACKAGE_KIND,
            Some("stable-slot"),
            PublishedKeyPackageProtocolData::darkmatter_v2_last_resort(
                key_package_ref.clone(),
                "base64-key-package".to_string(),
                app_components.clone(),
            ),
        )
        .await
        .unwrap();

        let pkg = PublishedKeyPackage::find_by_event_id(&db, "darkmatter-event")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(pkg.event_id, "darkmatter-event");
        assert_eq!(pkg.d_tag.as_deref(), Some("stable-slot"));
        assert_eq!(
            pkg.key_package_ref.as_deref(),
            Some(key_package_ref.as_slice())
        );
        assert_eq!(
            pkg.key_package_content.as_deref(),
            Some("base64-key-package")
        );
        assert_eq!(pkg.app_components, app_components);
        assert_eq!(pkg.package_version, 2);
        assert_eq!(pkg.package_role, PublishedKeyPackageRole::LastResort);
    }

    #[tokio::test]
    async fn test_mark_consumed_updates_timestamp_for_hash_ref_siblings() {
        let (db, _dir) = setup().await;
        let hash = vec![1, 2, 3];

        PublishedKeyPackage::create(&db, &hash, "canonical", MLS_KEY_PACKAGE_KIND, Some("d-tag"))
            .await
            .unwrap();
        PublishedKeyPackage::create(&db, &hash, "sibling", MLS_KEY_PACKAGE_KIND, Some("d-tag-2"))
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
        let sibling = PublishedKeyPackage::find_by_event_id(&db, "sibling")
            .await
            .unwrap()
            .unwrap();
        assert!(canonical.consumed_at.is_some());
        assert_eq!(canonical.consumed_at, sibling.consumed_at);
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

        PublishedKeyPackage::create(&db, &[1, 2, 3], "evt", MLS_KEY_PACKAGE_KIND, Some("d-tag"))
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

        PublishedKeyPackage::create(
            &db,
            &[1, 2, 3],
            "recent",
            MLS_KEY_PACKAGE_KIND,
            Some("d-tag"),
        )
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

    /// The outer `WHERE key_material_deleted = 0` filter prevents re-returning
    /// rows the cleanup task has already processed. Without it, the scheduler
    /// would re-attempt key-material deletion every tick for a hash_ref that's
    /// already been cleared.
    #[tokio::test]
    async fn test_find_eligible_excludes_already_deleted_rows() {
        let (db, _dir) = setup().await;

        // Old-enough consumed row that's ALSO already had its key material deleted.
        sqlx::query(
            "INSERT INTO published_key_packages \
                (key_package_hash_ref, event_id, consumed_at, key_material_deleted) \
             VALUES (?, ?, unixepoch() - 60, 1)",
        )
        .bind(&[9u8, 9, 9] as &[u8])
        .bind("already_cleaned")
        .execute(&db.inner.pool)
        .await
        .unwrap();

        let eligible = PublishedKeyPackage::find_eligible_for_cleanup(&db, 30)
            .await
            .unwrap();
        assert!(
            eligible.is_empty(),
            "rows with key_material_deleted = 1 must not be returned"
        );
    }

    #[tokio::test]
    async fn test_has_consumed_since_respects_quiet_period() {
        let (db, _dir) = setup().await;

        PublishedKeyPackage::create(
            &db,
            &[1, 2, 3],
            "recent",
            MLS_KEY_PACKAGE_KIND,
            Some("d-tag"),
        )
        .await
        .unwrap();
        PublishedKeyPackage::mark_consumed(&db, "recent")
            .await
            .unwrap();

        assert!(
            PublishedKeyPackage::has_consumed_since(&db, 30)
                .await
                .unwrap()
        );

        PublishedKeyPackage::backdate_consumed_at(&db, "recent", 60)
            .await
            .unwrap();
        assert!(
            !PublishedKeyPackage::has_consumed_since(&db, 30)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_has_consumed_since_ignores_deleted_key_material() {
        let (db, _dir) = setup().await;

        PublishedKeyPackage::create(
            &db,
            &[1, 2, 3],
            "recent",
            MLS_KEY_PACKAGE_KIND,
            Some("d-tag"),
        )
        .await
        .unwrap();
        PublishedKeyPackage::mark_consumed(&db, "recent")
            .await
            .unwrap();
        PublishedKeyPackage::mark_key_material_deleted_by_hash_ref(&db, &[1, 2, 3])
            .await
            .unwrap();

        assert!(
            !PublishedKeyPackage::has_consumed_since(&db, 30)
                .await
                .unwrap()
        );
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

        // A different kind inserted later must not bleed into the canonical lookup.
        PublishedKeyPackage::create(&db, &[3, 3, 3], "other", OTHER_KEY_PACKAGE_KIND, None)
            .await
            .unwrap();
        sqlx::query("UPDATE published_key_packages SET created_at = 999 WHERE event_id = ?")
            .bind("other")
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

        let other = PublishedKeyPackage::find_latest_by_kind(&db, OTHER_KEY_PACKAGE_KIND)
            .await
            .unwrap()
            .expect("other-kind row must exist");
        assert_eq!(other.event_id, "other");
        assert_eq!(other.created_at, 999);
        assert!(other.d_tag.is_none());
    }

    #[tokio::test]
    async fn test_mark_key_material_deleted_by_hash_ref_marks_siblings() {
        let (db, _dir) = setup().await;
        let hash = vec![1, 2, 3];

        PublishedKeyPackage::create(&db, &hash, "canonical", MLS_KEY_PACKAGE_KIND, Some("d-tag"))
            .await
            .unwrap();
        PublishedKeyPackage::create(&db, &hash, "sibling", MLS_KEY_PACKAGE_KIND, Some("d-tag-2"))
            .await
            .unwrap();

        let affected = PublishedKeyPackage::mark_key_material_deleted_by_hash_ref(&db, &hash)
            .await
            .unwrap();
        assert_eq!(affected, 2);

        for evt in &["canonical", "sibling"] {
            let pkg = PublishedKeyPackage::find_by_event_id(&db, evt)
                .await
                .unwrap()
                .unwrap();
            assert!(pkg.key_material_deleted);
        }
    }
}
