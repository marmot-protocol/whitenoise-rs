//! Database operations for cached graph users.

use chrono::{DateTime, Duration, Utc};
use nostr_sdk::{Metadata, PublicKey};

use super::{Database, DatabaseError, utils::parse_timestamp};
use crate::{
    WhitenoiseError,
    whitenoise::cached_graph_user::{CachedGraphUser, DEFAULT_CACHE_TTL_HOURS},
};

/// Internal row type for database mapping.
#[derive(Debug)]
struct CachedGraphUserRow {
    id: i64,
    pubkey: PublicKey,
    metadata: Option<Metadata>,
    follows: Option<Vec<PublicKey>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for CachedGraphUserRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let pubkey_str: String = row.try_get("pubkey")?;
        let metadata_json: Option<String> = row.try_get("metadata")?;
        let follows_json: Option<String> = row.try_get("follows")?;

        // Parse pubkey from hex string
        let pubkey = PublicKey::parse(&pubkey_str).map_err(|e| sqlx::Error::ColumnDecode {
            index: "pubkey".to_string(),
            source: Box::new(e),
        })?;

        // Parse metadata from JSON (None if column is NULL)
        let metadata: Option<Metadata> = match metadata_json {
            Some(json) => Some(serde_json::from_str::<Metadata>(&json).map_err(|e| {
                sqlx::Error::ColumnDecode {
                    index: "metadata".to_string(),
                    source: Box::new(e),
                }
            })?),
            None => None,
        };

        // Parse follows from JSON array of hex strings (None if column is NULL)
        let follows: Option<Vec<PublicKey>> = match follows_json {
            Some(json) => {
                let follows_hex: Vec<String> =
                    serde_json::from_str(&json).map_err(|e| sqlx::Error::ColumnDecode {
                        index: "follows".to_string(),
                        source: Box::new(e),
                    })?;
                Some(
                    follows_hex
                        .into_iter()
                        .filter_map(|hex| PublicKey::parse(&hex).ok())
                        .collect(),
                )
            }
            None => None,
        };

        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(Self {
            id,
            pubkey,
            metadata,
            follows,
            created_at,
            updated_at,
        })
    }
}

impl From<CachedGraphUserRow> for CachedGraphUser {
    fn from(row: CachedGraphUserRow) -> Self {
        Self {
            id: Some(row.id),
            pubkey: row.pubkey,
            metadata: row.metadata,
            follows: row.follows,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

impl CachedGraphUser {
    /// Find multiple fresh cached users by pubkeys in a single query.
    ///
    /// Returns only entries that exist and are fresh (within default TTL).
    /// Missing or stale entries are simply not included in the result.
    pub(crate) async fn find_fresh_batch(
        pubkeys: &[PublicKey],
        database: &Database,
    ) -> Result<Vec<Self>, WhitenoiseError> {
        Self::find_fresh_batch_with_ttl(pubkeys, DEFAULT_CACHE_TTL_HOURS, database).await
    }

    /// Find multiple fresh cached users by pubkeys with custom TTL.
    pub(crate) async fn find_fresh_batch_with_ttl(
        pubkeys: &[PublicKey],
        max_age_hours: i64,
        database: &Database,
    ) -> Result<Vec<Self>, WhitenoiseError> {
        if pubkeys.is_empty() {
            return Ok(Vec::new());
        }

        let cutoff = (Utc::now() - Duration::hours(max_age_hours)).timestamp_millis();
        let pubkey_hexes: Vec<String> = pubkeys.iter().map(|pk| pk.to_hex()).collect();

        let placeholders = "?,".repeat(pubkey_hexes.len());
        let placeholders = placeholders.trim_end_matches(',');

        let query = format!(
            "SELECT * FROM cached_graph_users WHERE pubkey IN ({}) AND updated_at > ?",
            placeholders
        );

        let mut query_builder = sqlx::query_as::<_, CachedGraphUserRow>(&query);
        for hex in &pubkey_hexes {
            query_builder = query_builder.bind(hex);
        }
        query_builder = query_builder.bind(cutoff);

        let rows = query_builder
            .fetch_all(&database.pool)
            .await
            .map_err(DatabaseError::Sqlx)?;

        Ok(rows.into_iter().map(Self::from).collect())
    }

    /// Upsert (insert or update) a cached graph user (sets both metadata and follows).
    #[cfg(test)]
    pub(crate) async fn upsert(&self, database: &Database) -> Result<Self, WhitenoiseError> {
        let pubkey_hex = self.pubkey.to_hex();
        let metadata_json = self
            .metadata
            .as_ref()
            .map(|m| serde_json::to_string(m).map_err(DatabaseError::Serialization))
            .transpose()?;
        let follows_json = self
            .follows
            .as_ref()
            .map(|f| {
                let hex: Vec<String> = f.iter().map(|pk| pk.to_hex()).collect();
                serde_json::to_string(&hex).map_err(DatabaseError::Serialization)
            })
            .transpose()?;
        let now = Utc::now().timestamp_millis();
        let created_at = self.created_at.timestamp_millis();

        let row = sqlx::query_as::<_, CachedGraphUserRow>(
            "INSERT INTO cached_graph_users (pubkey, metadata, follows, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(pubkey) DO UPDATE SET
                metadata = excluded.metadata,
                follows = excluded.follows,
                updated_at = excluded.updated_at
             RETURNING *",
        )
        .bind(&pubkey_hex)
        .bind(&metadata_json)
        .bind(&follows_json)
        .bind(created_at)
        .bind(now)
        .fetch_one(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(row.into())
    }

    /// Upsert only metadata, preserving existing follows.
    ///
    /// On fresh insert: follows = NULL (not fetched).
    /// On conflict: only metadata and updated_at change — follows is untouched.
    pub(crate) async fn upsert_metadata_only(
        pubkey: &PublicKey,
        metadata: &Metadata,
        database: &Database,
    ) -> Result<Self, WhitenoiseError> {
        let pubkey_hex = pubkey.to_hex();
        let metadata_json =
            serde_json::to_string(metadata).map_err(DatabaseError::Serialization)?;
        let now = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, CachedGraphUserRow>(
            "INSERT INTO cached_graph_users (pubkey, metadata, follows, created_at, updated_at)
             VALUES (?, ?, NULL, ?, ?)
             ON CONFLICT(pubkey) DO UPDATE SET
                metadata = excluded.metadata,
                updated_at = excluded.updated_at
             RETURNING *",
        )
        .bind(&pubkey_hex)
        .bind(&metadata_json)
        .bind(now)
        .bind(now)
        .fetch_one(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(row.into())
    }

    /// Upsert only follows, preserving existing metadata.
    ///
    /// On fresh insert: metadata = NULL (not fetched).
    /// On conflict: only follows and updated_at change — metadata is untouched.
    pub(crate) async fn upsert_follows_only(
        pubkey: &PublicKey,
        follows: &[PublicKey],
        database: &Database,
    ) -> Result<Self, WhitenoiseError> {
        let pubkey_hex = pubkey.to_hex();
        let follows_hex: Vec<String> = follows.iter().map(|pk| pk.to_hex()).collect();
        let follows_json =
            serde_json::to_string(&follows_hex).map_err(DatabaseError::Serialization)?;
        let now = Utc::now().timestamp_millis();

        let row = sqlx::query_as::<_, CachedGraphUserRow>(
            "INSERT INTO cached_graph_users (pubkey, metadata, follows, created_at, updated_at)
             VALUES (?, NULL, ?, ?, ?)
             ON CONFLICT(pubkey) DO UPDATE SET
                follows = excluded.follows,
                updated_at = excluded.updated_at
             RETURNING *",
        )
        .bind(&pubkey_hex)
        .bind(&follows_json)
        .bind(now)
        .bind(now)
        .fetch_one(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(row.into())
    }

    /// Remove stale cache entries using default TTL (24 hours).
    ///
    /// Returns the number of deleted rows.
    pub(crate) async fn cleanup_stale(database: &Database) -> Result<u64, WhitenoiseError> {
        Self::cleanup_stale_with_ttl(DEFAULT_CACHE_TTL_HOURS, database).await
    }

    /// Remove stale cache entries using custom TTL.
    ///
    /// Returns the number of deleted rows.
    pub(crate) async fn cleanup_stale_with_ttl(
        max_age_hours: i64,
        database: &Database,
    ) -> Result<u64, WhitenoiseError> {
        let cutoff = (Utc::now() - Duration::hours(max_age_hours)).timestamp_millis();

        let result = sqlx::query("DELETE FROM cached_graph_users WHERE updated_at <= ?")
            .bind(cutoff)
            .execute(&database.pool)
            .await
            .map_err(DatabaseError::Sqlx)?;

        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use nostr_sdk::Keys;

    #[tokio::test]
    async fn upsert_creates_new_entry() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let follow1 = Keys::generate().public_key();
        let follow2 = Keys::generate().public_key();

        let user = CachedGraphUser::new(
            keys.public_key(),
            Some(Metadata::new().name("New User").about("Test about")),
            Some(vec![follow1, follow2]),
        );

        let saved = user.upsert(&whitenoise.database).await.unwrap();

        assert!(saved.id.is_some());
        assert_eq!(saved.pubkey, keys.public_key());
        let m = saved.metadata.unwrap();
        assert_eq!(m.name, Some("New User".to_string()));
        assert_eq!(m.about, Some("Test about".to_string()));
        let follows = saved.follows.unwrap();
        assert_eq!(follows.len(), 2);
        assert!(follows.contains(&follow1));
        assert!(follows.contains(&follow2));
    }

    #[tokio::test]
    async fn upsert_updates_existing_entry() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        // Create initial entry
        let user1 = CachedGraphUser::new(
            keys.public_key(),
            Some(Metadata::new().name("Original Name")),
            Some(vec![]),
        );
        let saved1 = user1.upsert(&whitenoise.database).await.unwrap();

        // Update with new data
        let user2 = CachedGraphUser::new(
            keys.public_key(),
            Some(Metadata::new().name("Updated Name")),
            Some(vec![Keys::generate().public_key()]),
        );
        let saved2 = user2.upsert(&whitenoise.database).await.unwrap();

        // ID should remain the same
        assert_eq!(saved1.id, saved2.id);
        // Data should be updated
        assert_eq!(
            saved2.metadata.unwrap().name,
            Some("Updated Name".to_string())
        );
        assert_eq!(saved2.follows.unwrap().len(), 1);
        // updated_at should be newer
        assert!(saved2.updated_at >= saved1.updated_at);
    }

    #[tokio::test]
    async fn cleanup_stale_removes_old_entries() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        // Insert a stale entry directly
        let old_time = (Utc::now() - Duration::hours(25)).timestamp_millis();
        sqlx::query(
            "INSERT INTO cached_graph_users (pubkey, metadata, follows, created_at, updated_at)
             VALUES (?, '{}', '[]', ?, ?)",
        )
        .bind(keys.public_key().to_hex())
        .bind(old_time)
        .bind(old_time)
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        // Verify it exists (use large TTL to find regardless of staleness)
        let before = CachedGraphUser::find_fresh_batch_with_ttl(
            &[keys.public_key()],
            10000,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(before.len(), 1);

        // Run cleanup
        let deleted = CachedGraphUser::cleanup_stale(&whitenoise.database)
            .await
            .unwrap();

        assert_eq!(deleted, 1);

        // Verify it's gone
        let after = CachedGraphUser::find_fresh_batch_with_ttl(
            &[keys.public_key()],
            10000,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(after.is_empty());
    }

    #[tokio::test]
    async fn cleanup_stale_preserves_fresh_entries() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        // Create a fresh entry
        let user = CachedGraphUser::new(keys.public_key(), Some(Metadata::new()), Some(vec![]));
        user.upsert(&whitenoise.database).await.unwrap();

        // Run cleanup
        let deleted = CachedGraphUser::cleanup_stale(&whitenoise.database)
            .await
            .unwrap();

        assert_eq!(deleted, 0);

        // Verify it still exists
        let after = CachedGraphUser::find_fresh_batch(&[keys.public_key()], &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(after.len(), 1);
    }

    #[tokio::test]
    async fn custom_ttl_is_respected() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        // Insert an entry that's 2 hours old
        let two_hours_ago = (Utc::now() - Duration::hours(2)).timestamp_millis();
        sqlx::query(
            "INSERT INTO cached_graph_users (pubkey, metadata, follows, created_at, updated_at)
             VALUES (?, '{}', '[]', ?, ?)",
        )
        .bind(keys.public_key().to_hex())
        .bind(two_hours_ago)
        .bind(two_hours_ago)
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        // With 1-hour TTL, should be stale
        let result_1h = CachedGraphUser::find_fresh_batch_with_ttl(
            &[keys.public_key()],
            1,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(result_1h.is_empty());

        // With 3-hour TTL, should be fresh
        let result_3h = CachedGraphUser::find_fresh_batch_with_ttl(
            &[keys.public_key()],
            3,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(result_3h.len(), 1);
    }

    #[tokio::test]
    async fn cleanup_with_custom_ttl() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        // Insert an entry that's 2 hours old
        let two_hours_ago = (Utc::now() - Duration::hours(2)).timestamp_millis();
        sqlx::query(
            "INSERT INTO cached_graph_users (pubkey, metadata, follows, created_at, updated_at)
             VALUES (?, '{}', '[]', ?, ?)",
        )
        .bind(keys.public_key().to_hex())
        .bind(two_hours_ago)
        .bind(two_hours_ago)
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        // With 3-hour TTL, should not be deleted
        let deleted_3h = CachedGraphUser::cleanup_stale_with_ttl(3, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(deleted_3h, 0);

        // With 1-hour TTL, should be deleted
        let deleted_1h = CachedGraphUser::cleanup_stale_with_ttl(1, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(deleted_1h, 1);
    }

    #[tokio::test]
    async fn find_fresh_batch_returns_empty_for_empty_input() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let result = CachedGraphUser::find_fresh_batch(&[], &whitenoise.database)
            .await
            .unwrap();

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn find_fresh_batch_returns_multiple_fresh_entries() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys1 = Keys::generate();
        let keys2 = Keys::generate();

        // Create two fresh entries
        let user1 = CachedGraphUser::new(
            keys1.public_key(),
            Some(Metadata::new().name("User1")),
            Some(vec![]),
        );
        user1.upsert(&whitenoise.database).await.unwrap();

        let user2 = CachedGraphUser::new(
            keys2.public_key(),
            Some(Metadata::new().name("User2")),
            Some(vec![]),
        );
        user2.upsert(&whitenoise.database).await.unwrap();

        // Batch fetch both
        let result = CachedGraphUser::find_fresh_batch(
            &[keys1.public_key(), keys2.public_key()],
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert_eq!(result.len(), 2);
        let names: Vec<_> = result
            .iter()
            .filter_map(|u| u.metadata.as_ref().and_then(|m| m.name.clone()))
            .collect();
        assert!(names.contains(&"User1".to_string()));
        assert!(names.contains(&"User2".to_string()));
    }

    #[tokio::test]
    async fn find_fresh_batch_excludes_stale_entries() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let fresh_keys = Keys::generate();
        let stale_keys = Keys::generate();

        // Create fresh entry
        let fresh = CachedGraphUser::new(
            fresh_keys.public_key(),
            Some(Metadata::new().name("Fresh")),
            Some(vec![]),
        );
        fresh.upsert(&whitenoise.database).await.unwrap();

        // Insert stale entry directly
        let old_time = (Utc::now() - Duration::hours(25)).timestamp_millis();
        sqlx::query(
            "INSERT INTO cached_graph_users (pubkey, metadata, follows, created_at, updated_at)
             VALUES (?, '{\"name\":\"Stale\"}', '[]', ?, ?)",
        )
        .bind(stale_keys.public_key().to_hex())
        .bind(old_time)
        .bind(old_time)
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        // Batch fetch both
        let result = CachedGraphUser::find_fresh_batch(
            &[fresh_keys.public_key(), stale_keys.public_key()],
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Only fresh entry should be returned
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].pubkey, fresh_keys.public_key());
    }

    #[tokio::test]
    async fn find_fresh_batch_handles_missing_entries() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let existing_keys = Keys::generate();
        let missing_keys = Keys::generate();

        // Create only one entry
        let user = CachedGraphUser::new(
            existing_keys.public_key(),
            Some(Metadata::new().name("Exists")),
            Some(vec![]),
        );
        user.upsert(&whitenoise.database).await.unwrap();

        // Batch fetch both
        let result = CachedGraphUser::find_fresh_batch(
            &[existing_keys.public_key(), missing_keys.public_key()],
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Only existing entry should be returned
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].pubkey, existing_keys.public_key());
    }

    #[tokio::test]
    async fn upsert_metadata_only_preserves_existing_follows() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let follow1 = Keys::generate().public_key();

        // Insert full entry with both fields
        let user = CachedGraphUser::new(
            keys.public_key(),
            Some(Metadata::new().name("Original")),
            Some(vec![follow1]),
        );
        user.upsert(&whitenoise.database).await.unwrap();

        // Now upsert metadata only
        let updated = CachedGraphUser::upsert_metadata_only(
            &keys.public_key(),
            &Metadata::new().name("Updated"),
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Metadata should be updated
        assert_eq!(updated.metadata.unwrap().name, Some("Updated".to_string()));
        // Follows should be preserved
        let follows = updated.follows.unwrap();
        assert_eq!(follows.len(), 1);
        assert!(follows.contains(&follow1));
    }

    #[tokio::test]
    async fn upsert_follows_only_preserves_existing_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let follow1 = Keys::generate().public_key();
        let follow2 = Keys::generate().public_key();

        // Insert full entry with both fields
        let user = CachedGraphUser::new(
            keys.public_key(),
            Some(Metadata::new().name("Alice")),
            Some(vec![follow1]),
        );
        user.upsert(&whitenoise.database).await.unwrap();

        // Now upsert follows only
        let updated = CachedGraphUser::upsert_follows_only(
            &keys.public_key(),
            &[follow1, follow2],
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Metadata should be preserved
        assert_eq!(updated.metadata.unwrap().name, Some("Alice".to_string()));
        // Follows should be updated
        let follows = updated.follows.unwrap();
        assert_eq!(follows.len(), 2);
        assert!(follows.contains(&follow1));
        assert!(follows.contains(&follow2));
    }

    #[tokio::test]
    async fn upsert_metadata_only_creates_entry_with_null_follows() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        // Insert metadata-only for a new pubkey
        let saved = CachedGraphUser::upsert_metadata_only(
            &keys.public_key(),
            &Metadata::new().name("New"),
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert_eq!(saved.metadata.unwrap().name, Some("New".to_string()));
        assert!(
            saved.follows.is_none(),
            "Follows should be NULL for metadata-only insert"
        );
    }

    #[tokio::test]
    async fn upsert_follows_only_creates_entry_with_null_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let follow1 = Keys::generate().public_key();

        // Insert follows-only for a new pubkey
        let saved = CachedGraphUser::upsert_follows_only(
            &keys.public_key(),
            &[follow1],
            &whitenoise.database,
        )
        .await
        .unwrap();

        assert!(
            saved.metadata.is_none(),
            "Metadata should be NULL for follows-only insert"
        );
        let follows = saved.follows.unwrap();
        assert_eq!(follows.len(), 1);
        assert!(follows.contains(&follow1));
    }
}
