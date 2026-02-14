//! Database operations for cached graph users.

use chrono::{DateTime, Duration, Utc};
use nostr_sdk::{Metadata, PublicKey};

use super::{Database, DatabaseError, utils::parse_timestamp};
use crate::{
    WhitenoiseError,
    whitenoise::cached_graph_user::{CachedGraphUser, DEFAULT_CACHE_TTL_HOURS},
};

/// Internal row type for database mapping.
struct CachedGraphUserRow {
    id: i64,
    pubkey: PublicKey,
    metadata: Metadata,
    follows: Vec<PublicKey>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

/// Custom Debug impl to prevent sensitive data from leaking into logs.
impl std::fmt::Debug for CachedGraphUserRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedGraphUserRow")
            .field("pubkey", &self.pubkey)
            .finish()
    }
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
        let metadata_json: String = row.try_get("metadata")?;
        let follows_json: String = row.try_get("follows")?;

        // Parse pubkey from hex string
        let pubkey = PublicKey::parse(&pubkey_str).map_err(|e| sqlx::Error::ColumnDecode {
            index: "pubkey".to_string(),
            source: Box::new(e),
        })?;

        // Parse metadata from JSON
        let metadata: Metadata =
            serde_json::from_str(&metadata_json).map_err(|e| sqlx::Error::ColumnDecode {
                index: "metadata".to_string(),
                source: Box::new(e),
            })?;

        // Parse follows from JSON array of hex strings
        let follows_hex: Vec<String> =
            serde_json::from_str(&follows_json).map_err(|e| sqlx::Error::ColumnDecode {
                index: "follows".to_string(),
                source: Box::new(e),
            })?;

        let follows: Vec<PublicKey> = follows_hex
            .into_iter()
            .filter_map(|hex| PublicKey::parse(&hex).ok())
            .collect();

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
    /// Find by pubkey, returns `None` if not found or stale (uses default 24h TTL).
    pub(crate) async fn find_fresh(
        pubkey: &PublicKey,
        database: &Database,
    ) -> Result<Option<Self>, WhitenoiseError> {
        Self::find_fresh_with_ttl(pubkey, DEFAULT_CACHE_TTL_HOURS, database).await
    }

    /// Find by pubkey, returns `None` if not found or stale (custom TTL).
    pub(crate) async fn find_fresh_with_ttl(
        pubkey: &PublicKey,
        max_age_hours: i64,
        database: &Database,
    ) -> Result<Option<Self>, WhitenoiseError> {
        let cutoff = (Utc::now() - Duration::hours(max_age_hours)).timestamp_millis();

        let row = sqlx::query_as::<_, CachedGraphUserRow>(
            "SELECT * FROM cached_graph_users WHERE pubkey = ? AND updated_at > ?",
        )
        .bind(pubkey.to_hex())
        .bind(cutoff)
        .fetch_optional(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(row.map(Self::from))
    }

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

    /// Upsert (insert or update) a cached graph user.
    pub(crate) async fn upsert(&self, database: &Database) -> Result<Self, WhitenoiseError> {
        let pubkey_hex = self.pubkey.to_hex();
        let metadata_json =
            serde_json::to_string(&self.metadata).map_err(DatabaseError::Serialization)?;
        let follows_hex: Vec<String> = self.follows.iter().map(|pk| pk.to_hex()).collect();
        let follows_json =
            serde_json::to_string(&follows_hex).map_err(DatabaseError::Serialization)?;
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
    async fn find_fresh_returns_none_for_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let unknown_pubkey = Keys::generate().public_key();

        let result = CachedGraphUser::find_fresh(&unknown_pubkey, &whitenoise.database)
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn find_fresh_returns_none_for_stale() {
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

        let result = CachedGraphUser::find_fresh(&keys.public_key(), &whitenoise.database)
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn find_fresh_returns_some_for_fresh() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        let user = CachedGraphUser::new(
            keys.public_key(),
            Metadata::new().name("Fresh User"),
            vec![],
        );
        user.upsert(&whitenoise.database).await.unwrap();

        let result = CachedGraphUser::find_fresh(&keys.public_key(), &whitenoise.database)
            .await
            .unwrap();

        assert!(result.is_some());
        let found = result.unwrap();
        assert_eq!(found.pubkey, keys.public_key());
        assert_eq!(found.metadata.name, Some("Fresh User".to_string()));
    }

    #[tokio::test]
    async fn upsert_creates_new_entry() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let follow1 = Keys::generate().public_key();
        let follow2 = Keys::generate().public_key();

        let user = CachedGraphUser::new(
            keys.public_key(),
            Metadata::new().name("New User").about("Test about"),
            vec![follow1, follow2],
        );

        let saved = user.upsert(&whitenoise.database).await.unwrap();

        assert!(saved.id.is_some());
        assert_eq!(saved.pubkey, keys.public_key());
        assert_eq!(saved.metadata.name, Some("New User".to_string()));
        assert_eq!(saved.metadata.about, Some("Test about".to_string()));
        assert_eq!(saved.follows.len(), 2);
        assert!(saved.follows.contains(&follow1));
        assert!(saved.follows.contains(&follow2));
    }

    #[tokio::test]
    async fn upsert_updates_existing_entry() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        // Create initial entry
        let user1 = CachedGraphUser::new(
            keys.public_key(),
            Metadata::new().name("Original Name"),
            vec![],
        );
        let saved1 = user1.upsert(&whitenoise.database).await.unwrap();

        // Update with new data
        let user2 = CachedGraphUser::new(
            keys.public_key(),
            Metadata::new().name("Updated Name"),
            vec![Keys::generate().public_key()],
        );
        let saved2 = user2.upsert(&whitenoise.database).await.unwrap();

        // ID should remain the same
        assert_eq!(saved1.id, saved2.id);
        // Data should be updated
        assert_eq!(saved2.metadata.name, Some("Updated Name".to_string()));
        assert_eq!(saved2.follows.len(), 1);
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
        let before =
            CachedGraphUser::find_fresh_with_ttl(&keys.public_key(), 10000, &whitenoise.database)
                .await
                .unwrap();
        assert!(before.is_some());

        // Run cleanup
        let deleted = CachedGraphUser::cleanup_stale(&whitenoise.database)
            .await
            .unwrap();

        assert_eq!(deleted, 1);

        // Verify it's gone
        let after =
            CachedGraphUser::find_fresh_with_ttl(&keys.public_key(), 10000, &whitenoise.database)
                .await
                .unwrap();
        assert!(after.is_none());
    }

    #[tokio::test]
    async fn cleanup_stale_preserves_fresh_entries() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        // Create a fresh entry
        let user = CachedGraphUser::new(keys.public_key(), Metadata::new(), vec![]);
        user.upsert(&whitenoise.database).await.unwrap();

        // Run cleanup
        let deleted = CachedGraphUser::cleanup_stale(&whitenoise.database)
            .await
            .unwrap();

        assert_eq!(deleted, 0);

        // Verify it still exists
        let after = CachedGraphUser::find_fresh(&keys.public_key(), &whitenoise.database)
            .await
            .unwrap();
        assert!(after.is_some());
    }

    #[tokio::test]
    async fn custom_ttl_is_respected_in_find_fresh() {
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
        let result_1h =
            CachedGraphUser::find_fresh_with_ttl(&keys.public_key(), 1, &whitenoise.database)
                .await
                .unwrap();
        assert!(result_1h.is_none());

        // With 3-hour TTL, should be fresh
        let result_3h =
            CachedGraphUser::find_fresh_with_ttl(&keys.public_key(), 3, &whitenoise.database)
                .await
                .unwrap();
        assert!(result_3h.is_some());
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
        let user1 = CachedGraphUser::new(keys1.public_key(), Metadata::new().name("User1"), vec![]);
        user1.upsert(&whitenoise.database).await.unwrap();

        let user2 = CachedGraphUser::new(keys2.public_key(), Metadata::new().name("User2"), vec![]);
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
            .filter_map(|u| u.metadata.name.clone())
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
            Metadata::new().name("Fresh"),
            vec![],
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
            Metadata::new().name("Exists"),
            vec![],
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
}
