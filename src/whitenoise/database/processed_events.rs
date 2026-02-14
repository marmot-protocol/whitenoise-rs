use chrono::{DateTime, Utc};
use nostr_sdk::{EventId, Kind, PublicKey};

use super::{Database, DatabaseError, utils::parse_timestamp};
use crate::whitenoise::{error::WhitenoiseError, relays::RelayType};

/// Row structure for processed_events table
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ProcessedEvent {
    pub id: i64,
    pub event_id: EventId,
    pub account_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub event_created_at: Option<DateTime<Utc>>,
    pub event_kind: Option<Kind>,
    pub author: Option<PublicKey>,
}

impl<'r, R> sqlx::FromRow<'r, R> for ProcessedEvent
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    i64: sqlx::Decode<'r, <R as sqlx::Row>::Database> + sqlx::Type<<R as sqlx::Row>::Database>,
    String: sqlx::Decode<'r, <R as sqlx::Row>::Database> + sqlx::Type<<R as sqlx::Row>::Database>,
{
    fn from_row(row: &'r R) -> Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let event_id_hex: String = row.try_get("event_id")?;
        let account_id: Option<i64> = row.try_get("account_id")?;

        let event_id =
            EventId::from_hex(&event_id_hex).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;

        let created_at = parse_timestamp(row, "created_at")?;

        // Handle nullable event_created_at
        let event_created_at = match row.try_get::<Option<i64>, &str>("event_created_at")? {
            Some(timestamp_ms) => Some(DateTime::from_timestamp_millis(timestamp_ms).ok_or_else(
                || {
                    sqlx::Error::Decode(
                        format!("Invalid event_created_at timestamp value: {}", timestamp_ms)
                            .into(),
                    )
                },
            )?),
            None => None,
        };

        // Handle nullable event_kind
        let event_kind = row
            .try_get::<Option<i64>, &str>("event_kind")?
            .map(|kind| Kind::from(kind as u16));

        // Handle nullable author
        let author = match row.try_get::<Option<String>, &str>("author")? {
            Some(author_hex) => Some(PublicKey::from_hex(&author_hex).map_err(|e| {
                sqlx::Error::Decode(format!("Invalid author public key: {}", e).into())
            })?),
            None => None,
        };

        Ok(ProcessedEvent {
            id,
            event_id,
            account_id,
            created_at,
            event_created_at,
            event_kind,
            author,
        })
    }
}

impl ProcessedEvent {
    /// Records that we processed a specific event to ensure idempotency
    pub(crate) async fn create(
        event_id: &EventId,
        account_id: Option<i64>,
        event_created_at: Option<DateTime<Utc>>,
        event_kind: Option<Kind>,
        author: Option<&PublicKey>,
        database: &Database,
    ) -> Result<(), DatabaseError> {
        // Convert event_created_at to milliseconds if present
        let event_timestamp_ms = event_created_at.map(|dt| dt.timestamp_millis());
        let event_kind_i64 = event_kind.map(|k| k.as_u16() as i64);
        let author_hex = author.map(|pk| pk.to_hex());

        // Use INSERT OR IGNORE to handle potential race conditions
        sqlx::query("INSERT OR IGNORE INTO processed_events (event_id, account_id, event_created_at, event_kind, author) VALUES (?, ?, ?, ?, ?)")
            .bind(event_id.to_hex())
            .bind(account_id)
            .bind(event_timestamp_ms)
            .bind(event_kind_i64)
            .bind(author_hex)
            .execute(&database.pool)
            .await?;

        Ok(())
    }

    /// Checks if we already processed a specific event
    /// - account_id: Some(id) for account-specific processing, None for global processing
    pub(crate) async fn exists(
        event_id: &EventId,
        account_id: Option<i64>,
        database: &Database,
    ) -> Result<bool, DatabaseError> {
        let (query, bind_account_id) = match account_id {
            Some(id) => (
                "SELECT EXISTS(SELECT 1 FROM processed_events WHERE event_id = ? AND account_id = ?)",
                Some(id),
            ),
            None => (
                "SELECT EXISTS(SELECT 1 FROM processed_events WHERE event_id = ? AND account_id IS NULL)",
                None,
            ),
        };

        let mut query_builder = sqlx::query_as(query).bind(event_id.to_hex());

        if let Some(id) = bind_account_id {
            query_builder = query_builder.bind(id);
        }

        let result: Option<(bool,)> = query_builder.fetch_optional(&database.pool).await?;

        Ok(result.map(|(exists,)| exists).unwrap_or(false))
    }

    /// Gets the newest event timestamp for specific event kind and account
    /// Optionally filters by author for global events (when account_id is None)
    pub(crate) async fn newest_event_timestamp_for_kind(
        account_id: Option<i64>,
        event_kind: u16,
        author_pubkey: Option<&PublicKey>,
        database: &Database,
    ) -> Result<Option<DateTime<Utc>>, DatabaseError> {
        // Build query based on parameters
        let query = match (account_id.is_some(), author_pubkey.is_some()) {
            (true, _) => {
                // Account-specific events (author filter not applicable)
                "SELECT MAX(event_created_at) FROM processed_events WHERE account_id = ? AND event_kind = ?"
            }
            (false, true) => {
                // Global events with author filter
                "SELECT MAX(event_created_at) FROM processed_events WHERE account_id IS NULL AND event_kind = ? AND author = ?"
            }
            (false, false) => {
                // Global events without author filter
                "SELECT MAX(event_created_at) FROM processed_events WHERE account_id IS NULL AND event_kind = ?"
            }
        };

        let mut query_builder = sqlx::query_scalar::<_, Option<i64>>(query);

        // Bind parameters based on query type
        if let Some(id) = account_id {
            query_builder = query_builder.bind(id);
            query_builder = query_builder.bind(event_kind as i64);
        } else {
            query_builder = query_builder.bind(event_kind as i64);
            if let Some(author) = author_pubkey {
                query_builder = query_builder.bind(author.to_hex());
            }
        }

        let result: Option<Option<i64>> = query_builder.fetch_optional(&database.pool).await?;

        Ok(result.flatten().and_then(DateTime::from_timestamp_millis))
    }

    /// Gets the newest relay event timestamp for a user
    pub(crate) async fn newest_relay_event_timestamp(
        user_pubkey: &PublicKey,
        relay_type: RelayType,
        database: &Database,
    ) -> Result<Option<DateTime<Utc>>, WhitenoiseError> {
        // Map relay types to their corresponding event kinds
        let kind = match relay_type {
            RelayType::Nip65 => 10002,
            RelayType::Inbox => 10050,
            RelayType::KeyPackage => 10051,
        };

        // Note: For global events (user data), account_id will be NULL
        // Filter by user pubkey to get the newest timestamp for this specific user
        Self::newest_event_timestamp_for_kind(None, kind, Some(user_pubkey), database)
            .await
            .map_err(WhitenoiseError::Database)
    }

    /// Gets the newest contact list event timestamp for an account
    pub(crate) async fn newest_contact_list_timestamp(
        account_id: i64,
        database: &Database,
    ) -> Result<Option<DateTime<Utc>>, WhitenoiseError> {
        // Query processed_events for kind 3 events with specific account_id
        Self::newest_event_timestamp_for_kind(Some(account_id), 3, None, database)
            .await
            .map_err(WhitenoiseError::Database)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nostr_sdk::{EventId, Keys};
    use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
    use std::str::FromStr;

    // Helper function to create a test database with the required tables
    async fn setup_test_db() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        // Create accounts table (referenced by foreign keys)
        sqlx::query(
            "CREATE TABLE accounts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pubkey TEXT NOT NULL,
                user_id INTEGER NOT NULL,
                last_synced_at INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create processed_events table
        sqlx::query(
            "CREATE TABLE processed_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL
                    CHECK (length(event_id) = 64 AND event_id GLOB '[0-9a-fA-F]*'),
                account_id INTEGER,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                event_created_at INTEGER DEFAULT NULL,
                event_kind INTEGER DEFAULT NULL,
                author TEXT DEFAULT NULL,
                FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE CASCADE,
                UNIQUE(event_id, account_id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Add partial unique index for global events (matching production schema)
        sqlx::query(
            "CREATE UNIQUE INDEX idx_processed_events_global_unique
             ON processed_events(event_id)
             WHERE account_id IS NULL",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create test account
        sqlx::query(
            "INSERT INTO accounts (pubkey, user_id, created_at, updated_at)
             VALUES (?, 1, ?, ?)",
        )
        .bind("test_pubkey")
        .bind(Utc::now().timestamp())
        .bind(Utc::now().timestamp())
        .execute(&pool)
        .await
        .unwrap();

        pool
    }

    // Helper function to create a test event ID
    fn create_test_event_id() -> EventId {
        let keys = Keys::generate();
        EventId::from_str(&keys.public_key().to_string()).unwrap_or_else(|_| {
            // Fallback to a valid hex string
            EventId::from_hex("1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")
                .unwrap()
        })
    }

    // Helper function to wrap pool in Database struct
    fn wrap_pool_in_database(pool: SqlitePool) -> Database {
        Database {
            pool,
            path: std::path::PathBuf::from(":memory:"),
            last_connected: std::time::SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn test_processed_event_from_row_valid_data() {
        let pool = setup_test_db().await;
        let event_id = create_test_event_id();
        let account_id = 1i64;
        let timestamp = Utc::now().timestamp_millis();

        // Insert a test record
        sqlx::query(
            "INSERT INTO processed_events (event_id, account_id, created_at, event_created_at, event_kind) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(event_id.to_hex())
        .bind(account_id)
        .bind(timestamp)
        .bind(Some(timestamp + 1000)) // Different event timestamp
        .bind(Some(1i64)) // Kind 1 (text note)
        .execute(&pool)
        .await
        .unwrap();

        // Fetch and verify
        let row: ProcessedEvent = sqlx::query_as(
            "SELECT id, event_id, account_id, created_at, event_created_at, event_kind, author FROM processed_events WHERE account_id = ?",
        )
        .bind(account_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row.event_id, event_id);
        assert_eq!(row.account_id, Some(account_id));
        assert_eq!(row.created_at.timestamp_millis(), timestamp);
        assert_eq!(
            row.event_created_at.unwrap().timestamp_millis(),
            timestamp + 1000
        );
        assert_eq!(row.event_kind, Some(Kind::TextNote));
    }

    #[tokio::test]
    async fn test_processed_event_create() {
        let pool = setup_test_db().await;
        let database = wrap_pool_in_database(pool);
        let event_id = create_test_event_id();
        let account_id = 1i64;
        let event_timestamp = Some(Utc::now());
        let event_kind = Some(Kind::TextNote);

        // Create a processed event
        let result = ProcessedEvent::create(
            &event_id,
            Some(account_id),
            event_timestamp,
            event_kind,
            None, // No author for test
            &database,
        )
        .await;
        assert!(result.is_ok());

        // Verify it was inserted
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM processed_events WHERE event_id = ? AND account_id = ?",
        )
        .bind(event_id.to_hex())
        .bind(account_id)
        .fetch_one(&database.pool)
        .await
        .unwrap();

        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn test_processed_event_create_duplicate_ignored() {
        let pool = setup_test_db().await;
        let database = wrap_pool_in_database(pool);
        let event_id = create_test_event_id();
        let account_id = 1i64;
        let event_timestamp = Some(Utc::now());
        let event_kind = Some(Kind::TextNote);

        // Create the same processed event twice
        let result1 = ProcessedEvent::create(
            &event_id,
            Some(account_id),
            event_timestamp,
            event_kind,
            None, // No author for test
            &database,
        )
        .await;
        let result2 = ProcessedEvent::create(
            &event_id,
            Some(account_id),
            event_timestamp,
            event_kind,
            None, // No author for test
            &database,
        )
        .await;

        assert!(result1.is_ok());
        assert!(result2.is_ok());

        // Verify only one record exists (INSERT OR IGNORE behavior)
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM processed_events WHERE event_id = ? AND account_id = ?",
        )
        .bind(event_id.to_hex())
        .bind(account_id)
        .fetch_one(&database.pool)
        .await
        .unwrap();

        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn test_processed_event_exists_true() {
        let pool = setup_test_db().await;
        let database = wrap_pool_in_database(pool);
        let event_id = create_test_event_id();
        let account_id = 1i64;

        // Create a processed event
        ProcessedEvent::create(
            &event_id,
            Some(account_id),
            Some(Utc::now()),
            Some(Kind::TextNote),
            None, // No author for test
            &database,
        )
        .await
        .unwrap();

        // Check if it exists
        let exists = ProcessedEvent::exists(&event_id, Some(account_id), &database)
            .await
            .unwrap();

        assert!(exists);
    }

    #[tokio::test]
    async fn test_processed_event_exists_false() {
        let pool = setup_test_db().await;
        let database = wrap_pool_in_database(pool);
        let event_id = create_test_event_id();
        let account_id = 1i64;

        // Check if non-existent event exists
        let exists = ProcessedEvent::exists(&event_id, Some(account_id), &database)
            .await
            .unwrap();

        assert!(!exists);
    }

    #[tokio::test]
    async fn test_processed_event_exists_different_account() {
        let pool = setup_test_db().await;
        let database = wrap_pool_in_database(pool);
        let event_id = create_test_event_id();
        let account_id1 = 1i64;
        let account_id2 = 999i64; // Non-existent account

        // Create a processed event for account 1
        ProcessedEvent::create(
            &event_id,
            Some(account_id1),
            Some(Utc::now()),
            Some(Kind::TextNote),
            None, // No author for test
            &database,
        )
        .await
        .unwrap();

        // Check if it exists for account 2 (should be false)
        let exists = ProcessedEvent::exists(&event_id, Some(account_id2), &database)
            .await
            .unwrap();

        assert!(!exists);
    }

    #[tokio::test]
    async fn test_multiple_accounts_same_event() {
        let pool = setup_test_db().await;
        let database = wrap_pool_in_database(pool);
        let event_id = create_test_event_id();

        // Create another test account
        sqlx::query(
            "INSERT INTO accounts (pubkey, user_id, created_at, updated_at)
             VALUES (?, 2, ?, ?)",
        )
        .bind("test_pubkey_2")
        .bind(Utc::now().timestamp())
        .bind(Utc::now().timestamp())
        .execute(&database.pool)
        .await
        .unwrap();

        let account_id1 = 1i64;
        let account_id2 = 2i64;

        ProcessedEvent::create(
            &event_id,
            Some(account_id1),
            Some(Utc::now()),
            Some(Kind::TextNote),
            None, // No author for test
            &database,
        )
        .await
        .unwrap();
        ProcessedEvent::create(
            &event_id,
            Some(account_id2),
            Some(Utc::now()),
            Some(Kind::TextNote),
            None, // No author for test
            &database,
        )
        .await
        .unwrap();

        // Verify both accounts have their records
        assert!(
            ProcessedEvent::exists(&event_id, Some(account_id1), &database)
                .await
                .unwrap()
        );
        assert!(
            ProcessedEvent::exists(&event_id, Some(account_id2), &database)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_processed_event_struct_clone_debug_eq() {
        let event_id = create_test_event_id();
        let now = Utc::now();

        let event1 = ProcessedEvent {
            id: 1,
            event_id,
            account_id: Some(123),
            created_at: now,
            event_created_at: Some(now),
            event_kind: Some(Kind::TextNote),
            author: None,
        };

        let event2 = event1.clone();
        assert_eq!(event1, event2);

        // Test Debug trait
        let debug_str = format!("{:?}", event1);
        assert!(debug_str.contains("ProcessedEvent"));
        assert!(debug_str.contains(&event_id.to_hex()));
    }

    #[tokio::test]
    async fn test_newest_event_timestamp_for_kind() {
        let pool = setup_test_db().await;
        let database = wrap_pool_in_database(pool);
        let event_id1 = create_test_event_id();
        let event_id2 = create_test_event_id();
        let account_id = 1i64;
        let older_timestamp = Utc::now();
        let newer_timestamp = older_timestamp + chrono::Duration::milliseconds(1000);

        // Create two events with different timestamps
        ProcessedEvent::create(
            &event_id1,
            Some(account_id),
            Some(older_timestamp),
            Some(Kind::ContactList),
            None, // No author for test
            &database,
        )
        .await
        .unwrap();
        ProcessedEvent::create(
            &event_id2,
            Some(account_id),
            Some(newer_timestamp),
            Some(Kind::ContactList),
            None, // No author for test
            &database,
        )
        .await
        .unwrap();

        // Test getting newest timestamp for kind 3
        let result =
            ProcessedEvent::newest_event_timestamp_for_kind(Some(account_id), 3, None, &database)
                .await
                .unwrap();

        assert!(result.is_some());
        assert_eq!(
            result.unwrap().timestamp_millis(),
            newer_timestamp.timestamp_millis()
        );

        // Test with different kind (should return None)
        let result =
            ProcessedEvent::newest_event_timestamp_for_kind(Some(account_id), 1, None, &database)
                .await
                .unwrap();

        assert!(result.is_none());

        // Test with different account (should return None)
        let result = ProcessedEvent::newest_event_timestamp_for_kind(Some(999), 3, None, &database)
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_newest_contact_list_timestamp() {
        let pool = setup_test_db().await;
        let database = wrap_pool_in_database(pool);
        let event_id = create_test_event_id();
        let account_id = 1i64;
        let timestamp = Utc::now();

        // Create a contact list event (kind 3)
        ProcessedEvent::create(
            &event_id,
            Some(account_id),
            Some(timestamp),
            Some(Kind::ContactList),
            None, // No author for test
            &database,
        )
        .await
        .unwrap();

        // Test getting newest contact list timestamp
        let result = ProcessedEvent::newest_contact_list_timestamp(account_id, &database)
            .await
            .unwrap();

        assert!(result.is_some());
        assert_eq!(
            result.unwrap().timestamp_millis(),
            timestamp.timestamp_millis()
        );

        // Test with different account (should return None)
        let result = ProcessedEvent::newest_contact_list_timestamp(999, &database)
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_newest_relay_event_timestamp() {
        let pool = setup_test_db().await;
        let database = wrap_pool_in_database(pool);
        let event_id = create_test_event_id();
        let user_pubkey = nostr_sdk::Keys::generate().public_key();
        let timestamp = Utc::now();

        // Create a NIP-65 relay event (kind 10002) as global event
        ProcessedEvent::create(
            &event_id,
            None,
            Some(timestamp),
            Some(Kind::RelayList),
            Some(&user_pubkey),
            &database,
        )
        .await
        .unwrap();

        // Test getting newest relay timestamp for NIP-65
        let result =
            ProcessedEvent::newest_relay_event_timestamp(&user_pubkey, RelayType::Nip65, &database)
                .await
                .unwrap();

        assert!(result.is_some());
        assert_eq!(
            result.unwrap().timestamp_millis(),
            timestamp.timestamp_millis()
        );

        // Test with different relay type (should return None)
        let result =
            ProcessedEvent::newest_relay_event_timestamp(&user_pubkey, RelayType::Inbox, &database)
                .await
                .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_newest_event_timestamp_for_kind_with_author_filter() {
        let pool = setup_test_db().await;
        let database = wrap_pool_in_database(pool);
        let event_id1 = create_test_event_id();
        let event_id2 = create_test_event_id();

        // Create test authors
        let author1 = Keys::generate().public_key();
        let author2 = Keys::generate().public_key();

        let older_timestamp = Utc::now() - chrono::Duration::minutes(10);
        let newer_timestamp = Utc::now();

        // Create metadata events (kind 0) from different authors
        ProcessedEvent::create(
            &event_id1,
            None, // Global event
            Some(older_timestamp),
            Some(Kind::Metadata), // Metadata kind
            Some(&author1),
            &database,
        )
        .await
        .unwrap();

        ProcessedEvent::create(
            &event_id2,
            None, // Global event
            Some(newer_timestamp),
            Some(Kind::Metadata), // Metadata kind
            Some(&author2),
            &database,
        )
        .await
        .unwrap();

        // Test getting newest timestamp for kind 0 with author1 filter
        let result =
            ProcessedEvent::newest_event_timestamp_for_kind(None, 0, Some(&author1), &database)
                .await
                .unwrap();

        assert!(result.is_some());
        assert_eq!(
            result.unwrap().timestamp_millis(),
            older_timestamp.timestamp_millis()
        );

        // Test getting newest timestamp for kind 0 with author2 filter
        let result =
            ProcessedEvent::newest_event_timestamp_for_kind(None, 0, Some(&author2), &database)
                .await
                .unwrap();

        assert!(result.is_some());
        assert_eq!(
            result.unwrap().timestamp_millis(),
            newer_timestamp.timestamp_millis()
        );

        // Test getting newest timestamp for kind 0 without author filter (should get the newer one)
        let result = ProcessedEvent::newest_event_timestamp_for_kind(None, 0, None, &database)
            .await
            .unwrap();

        assert!(result.is_some());
        assert_eq!(
            result.unwrap().timestamp_millis(),
            newer_timestamp.timestamp_millis()
        );

        // Test with non-existent author
        let non_existent_author = Keys::generate().public_key();
        let result = ProcessedEvent::newest_event_timestamp_for_kind(
            None,
            0,
            Some(&non_existent_author),
            &database,
        )
        .await
        .unwrap();

        assert!(result.is_none());
    }
}
