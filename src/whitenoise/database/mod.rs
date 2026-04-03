use std::{
    path::PathBuf,
    sync::LazyLock,
    time::{Duration, SystemTime},
};

use sqlx::{
    ConnectOptions, Sqlite, SqlitePool,
    migrate::{MigrateDatabase, Migrator},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use thiserror::Error;

pub mod account_settings;
pub mod accounts;
pub mod accounts_groups;
pub mod aggregated_messages;
pub mod app_settings;
pub mod cached_graph_users;
pub mod content_search;
pub mod drafts;
pub mod group_information;
pub mod group_push_tokens;
pub mod media_files;
pub mod mute_list;
pub mod processed_events;
pub mod published_events;
pub mod published_key_packages;
pub mod push_registrations;
pub mod relay_events;
pub mod relay_status;
pub mod relays;
pub mod user_relays;
pub mod users;
pub mod utils;

pub static MIGRATOR: LazyLock<Migrator> = LazyLock::new(|| sqlx::migrate!("./db_migrations"));

const DB_ACQUIRE_TIMEOUT_SECS: u64 = 5;
const DB_MAX_CONNECTIONS: u32 = 10;
const DB_BUSY_TIMEOUT_MS: u32 = 5000;

#[derive(Error, Debug)]
pub enum DatabaseError {
    #[error("SQLx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("Migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("File system error: {0}")]
    FileSystem(#[from] std::io::Error),
    #[error("Invalid timestamp: {timestamp} cannot be converted to DateTime")]
    InvalidTimestamp { timestamp: i64 },
    #[error("Invalid cursor: {reason}")]
    InvalidCursor { reason: &'static str },
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Search query did not return a position column for message {message_id}")]
    MissingSearchPosition { message_id: String },
}

impl DatabaseError {
    /// Returns `true` for transient SQLite lock errors.
    ///
    /// Handles both primary codes (5 = SQLITE_BUSY, 6 = SQLITE_LOCKED) and
    /// extended codes (e.g. 517 = SQLITE_BUSY_SNAPSHOT) by masking with 0xFF
    /// to extract the primary result code.
    pub fn is_sqlite_lock_error(&self) -> bool {
        let Self::Sqlx(sqlx::Error::Database(db_err)) = self else {
            return false;
        };
        db_err
            .code()
            .and_then(|c| c.parse::<u32>().ok())
            .is_some_and(|code| matches!(code & 0xFF, 5 | 6))
    }
}

#[derive(Clone, Debug)]
pub struct Database {
    pub pool: SqlitePool,
    pub path: PathBuf,
    pub last_connected: SystemTime,
}

impl Database {
    pub async fn new(db_path: PathBuf) -> Result<Self, DatabaseError> {
        // Create parent directories if they don't exist
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let db_url = format!("sqlite://{}", db_path.display());

        // Create database if it doesn't exist
        tracing::debug!("Checking if DB exists...{:?}", db_url);
        match Sqlite::database_exists(&db_url).await {
            Ok(true) => {
                tracing::debug!("DB exists");
            }
            Ok(false) => {
                tracing::debug!("DB does not exist, creating...");
                Sqlite::create_database(&db_url).await.map_err(|e| {
                    tracing::error!("Error creating DB: {:?}", e);
                    DatabaseError::Sqlx(e)
                })?;
                tracing::debug!("DB created");
            }
            Err(e) => {
                tracing::warn!(
                    "Could not check if database exists: {:?}, attempting to create",
                    e
                );
                Sqlite::create_database(&db_url).await.map_err(|e| {
                    tracing::error!("Error creating DB: {:?}", e);
                    DatabaseError::Sqlx(e)
                })?;
            }
        }

        let pool = Self::create_connection_pool(&db_url).await?;

        // Automatically run migrations
        MIGRATOR.run(&pool).await?;

        Ok(Self {
            pool,
            path: db_path,
            last_connected: SystemTime::now(),
        })
    }

    /// Creates and configures a SQLite connection pool
    async fn create_connection_pool(db_url: &str) -> Result<SqlitePool, DatabaseError> {
        tracing::debug!("Creating connection pool...");

        // Log every SQL statement only when explicitly opted in (e.g. benchmarks).
        // Slow-query logging stays unconditional as a safety net.
        let log_statements_level = if std::env::var("SQLX_LOG_STATEMENTS").is_ok() {
            tracing::log::LevelFilter::Info
        } else {
            tracing::log::LevelFilter::Off
        };

        let connect_options = format!("{db_url}?mode=rwc")
            .parse::<SqliteConnectOptions>()?
            .log_statements(log_statements_level)
            .log_slow_statements(tracing::log::LevelFilter::Warn, Duration::from_millis(500));

        let pool = SqlitePoolOptions::new()
            .acquire_timeout(Duration::from_secs(DB_ACQUIRE_TIMEOUT_SECS))
            .max_connections(DB_MAX_CONNECTIONS)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    let conn = &mut *conn;
                    // Enable WAL mode for better concurrent access
                    sqlx::query("PRAGMA journal_mode=WAL")
                        .execute(&mut *conn)
                        .await?;
                    // Set busy timeout for lock contention
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "PRAGMA busy_timeout={DB_BUSY_TIMEOUT_MS}"
                    )))
                    .execute(&mut *conn)
                    .await?;
                    // Enable foreign keys and triggers
                    sqlx::query("PRAGMA foreign_keys = ON")
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query("PRAGMA recursive_triggers = ON")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(connect_options)
            .await?;
        Ok(pool)
    }

    /// Runs all pending database migrations
    ///
    /// This method is idempotent - it's safe to call multiple times.
    /// Only new migrations will be applied.
    pub async fn migrate_up(&self) -> Result<(), DatabaseError> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// Deletes all data from all tables while preserving the schema
    ///
    /// This method:
    /// - Temporarily disables foreign key constraints
    /// - Deletes all rows from user tables (preserving schema and migrations)
    /// - Re-enables foreign key constraints
    /// - Resets SQLite auto-increment counters
    /// - Uses a transaction to ensure atomicity
    ///
    /// This is safer than DROP TABLE + migrate because:
    /// 1. It's fully atomic (no two-phase operation)
    /// 2. Schema is preserved even if interrupted
    /// 3. No risk of migration failures leaving database in broken state
    pub async fn delete_all_data(&self) -> Result<(), DatabaseError> {
        retry_on_lock(|| self.delete_all_data_inner()).await
    }

    /// Inner implementation of delete_all_data
    async fn delete_all_data_inner(&self) -> Result<(), DatabaseError> {
        let mut txn = self.pool.begin().await?;

        // Disable foreign key constraints temporarily to allow deleting in any order
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *txn)
            .await?;

        // Get all user tables (excluding SQLite system tables and migration tracking)
        let tables: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master
             WHERE type='table'
             AND name NOT LIKE 'sqlite_%'
             AND name != '_sqlx_migrations'",
        )
        .fetch_all(&mut *txn)
        .await?;

        // Delete all rows from each table (preserves schema)
        for (table_name,) in &tables {
            let delete_query = format!("DELETE FROM {}", table_name);
            sqlx::query(sqlx::AssertSqlSafe(delete_query))
                .execute(&mut *txn)
                .await?;
        }

        // Reset auto-increment counters for all tables
        for (table_name,) in &tables {
            // This resets the ROWID counter
            // Ignore errors - table might not have auto-increment
            let _ = sqlx::query("DELETE FROM sqlite_sequence WHERE name = ?")
                .bind(table_name)
                .execute(&mut *txn)
                .await;
        }

        // Re-enable foreign key constraints
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&mut *txn)
            .await?;

        txn.commit().await?;

        Ok(())
    }

    /// Deletes aggregated messages that all accounts have cleared.
    ///
    /// Only deletes messages whose `created_at` is at or before the minimum
    /// `chat_cleared_at` across all accounts in the group, and only when
    /// every account has set a `chat_cleared_at` value.
    ///
    /// Returns the number of rows deleted.
    pub(crate) async fn try_cleanup_cleared_messages(
        &self,
        mls_group_id: &mdk_core::prelude::GroupId,
    ) -> Result<u64, DatabaseError> {
        retry_on_lock(|| self.try_cleanup_cleared_messages_inner(mls_group_id)).await
    }

    async fn try_cleanup_cleared_messages_inner(
        &self,
        mls_group_id: &mdk_core::prelude::GroupId,
    ) -> Result<u64, DatabaseError> {
        let result = sqlx::query(
            "DELETE FROM aggregated_messages
             WHERE mls_group_id = ?
               AND created_at <= (
                 SELECT MIN(chat_cleared_at) FROM accounts_groups
                 WHERE mls_group_id = ?
                 HAVING COUNT(*) = COUNT(chat_cleared_at)
               )",
        )
        .bind(mls_group_id.as_slice())
        .bind(mls_group_id.as_slice())
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// Delete all data for an account's association with a group.
    ///
    /// Removes the per-account row and data (accounts_groups, drafts, push tokens,
    /// media files). If no other accounts reference this group after the deletion,
    /// also removes shared data (group_information, which cascades to
    /// aggregated_messages and drafts).
    ///
    /// Returns `true` if shared group data was also deleted (i.e., this was the
    /// last account).
    pub(crate) async fn delete_chat_data(
        &self,
        account_pubkey: &nostr_sdk::prelude::PublicKey,
        mls_group_id: &mdk_core::prelude::GroupId,
    ) -> Result<bool, DatabaseError> {
        retry_on_lock(|| self.delete_chat_data_inner(account_pubkey, mls_group_id)).await
    }

    async fn delete_chat_data_inner(
        &self,
        account_pubkey: &nostr_sdk::prelude::PublicKey,
        mls_group_id: &mdk_core::prelude::GroupId,
    ) -> Result<bool, DatabaseError> {
        let mut txn = self.pool.begin().await?;
        let pubkey_hex = account_pubkey.to_hex();

        // 1. Delete per-account data
        sqlx::query(
            "DELETE FROM accounts_groups
             WHERE account_pubkey = ? AND mls_group_id = ?",
        )
        .bind(&pubkey_hex)
        .bind(mls_group_id.as_slice())
        .execute(&mut *txn)
        .await?;

        sqlx::query(
            "DELETE FROM drafts
             WHERE account_pubkey = ? AND mls_group_id = ?",
        )
        .bind(&pubkey_hex)
        .bind(mls_group_id.as_slice())
        .execute(&mut *txn)
        .await?;

        sqlx::query(
            "DELETE FROM group_push_tokens
             WHERE account_pubkey = ? AND mls_group_id = ?",
        )
        .bind(&pubkey_hex)
        .bind(mls_group_id.as_slice())
        .execute(&mut *txn)
        .await?;

        sqlx::query(
            "DELETE FROM media_files
             WHERE account_pubkey = ? AND mls_group_id = ?",
        )
        .bind(&pubkey_hex)
        .bind(mls_group_id.as_slice())
        .execute(&mut *txn)
        .await?;

        // 2. Check if any other accounts still reference this group
        let (remaining,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM accounts_groups WHERE mls_group_id = ?")
                .bind(mls_group_id.as_slice())
                .fetch_one(&mut *txn)
                .await?;

        let last_account = remaining == 0;

        if last_account {
            // 3. Delete shared data (group_information cascades to
            //    aggregated_messages and drafts via FK)
            sqlx::query("DELETE FROM group_information WHERE mls_group_id = ?")
                .bind(mls_group_id.as_slice())
                .execute(&mut *txn)
                .await?;

            sqlx::query("DELETE FROM group_push_tokens WHERE mls_group_id = ?")
                .bind(mls_group_id.as_slice())
                .execute(&mut *txn)
                .await?;

            sqlx::query("DELETE FROM media_files WHERE mls_group_id = ?")
                .bind(mls_group_id.as_slice())
                .execute(&mut *txn)
                .await?;
        }

        txn.commit().await?;

        Ok(last_account)
    }
}

/// Retry an async database operation on transient SQLite lock errors.
///
/// Uses linear backoff (100 ms × attempt) for up to 3 attempts.
/// Returns on first success or first non-lock error.
pub(crate) async fn retry_on_lock<F, Fut, T>(mut op: F) -> std::result::Result<T, DatabaseError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, DatabaseError>>,
{
    const MAX_ATTEMPTS: u32 = 3;
    let mut attempt: u32 = 0;

    loop {
        attempt += 1;
        match op().await {
            Ok(val) => return Ok(val),
            Err(e) if e.is_sqlite_lock_error() && attempt < MAX_ATTEMPTS => {
                tracing::warn!(
                    target: "whitenoise::database",
                    "SQLite lock on attempt {attempt}/{MAX_ATTEMPTS}, \
                     retrying...",
                );
                tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt))).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    async fn create_test_db() -> (Database, TempDir) {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path)
            .await
            .expect("Failed to create test database");
        (db, temp_dir)
    }

    #[tokio::test]
    async fn test_database_creation() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("test.db");

        // Database should be created successfully
        let db = Database::new(db_path.clone()).await;
        assert!(db.is_ok());

        let db = db.unwrap();
        assert_eq!(db.path, db_path);
        assert!(db.last_connected.elapsed().unwrap().as_secs() < 2);
    }

    #[tokio::test]
    async fn test_database_creation_with_nested_path() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("nested").join("path").join("test.db");

        // Database should be created successfully even with nested directories
        let db = Database::new(db_path.clone()).await;
        assert!(db.is_ok());

        let db = db.unwrap();
        assert_eq!(db.path, db_path);
        assert!(db_path.exists());
    }

    #[tokio::test]
    async fn test_database_migrations_applied() {
        let (db, _temp_dir) = create_test_db().await;

        // Check that the accounts table exists (from migration 0001)
        let result =
            sqlx::query("SELECT name FROM sqlite_master WHERE type='table' AND name='accounts'")
                .fetch_optional(&db.pool)
                .await;

        assert!(result.is_ok());
        assert!(result.unwrap().is_some());

        // Check that the media_files table exists (from migration 0002)
        let result =
            sqlx::query("SELECT name FROM sqlite_master WHERE type='table' AND name='media_files'")
                .fetch_optional(&db.pool)
                .await;

        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_database_pragma_settings() {
        let (db, _temp_dir) = create_test_db().await;

        // Check that foreign keys are enabled
        let foreign_keys: (i64,) = sqlx::query_as("PRAGMA foreign_keys")
            .fetch_one(&db.pool)
            .await
            .expect("Failed to check foreign_keys pragma");
        assert_eq!(foreign_keys.0, 1);

        // Check that recursive triggers are enabled
        let recursive_triggers: (i64,) = sqlx::query_as("PRAGMA recursive_triggers")
            .fetch_one(&db.pool)
            .await
            .expect("Failed to check recursive_triggers pragma");
        assert_eq!(recursive_triggers.0, 1);

        // Check that WAL mode is enabled
        let journal_mode: (String,) = sqlx::query_as("PRAGMA journal_mode")
            .fetch_one(&db.pool)
            .await
            .expect("Failed to check journal_mode pragma");
        assert_eq!(journal_mode.0.to_lowercase(), "wal");
    }

    #[tokio::test]
    async fn test_delete_all_data() {
        let (db, _temp_dir) = create_test_db().await;

        // First create a user (required for foreign key)
        sqlx::query("INSERT INTO users (pubkey, metadata) VALUES ('test-pubkey', '{}')")
            .execute(&db.pool)
            .await
            .expect("Failed to insert test user");

        // Get the user ID
        let (user_id,): (i64,) =
            sqlx::query_as("SELECT id FROM users WHERE pubkey = 'test-pubkey'")
                .fetch_one(&db.pool)
                .await
                .expect("Failed to get user ID");

        // Insert account with correct schema (Note: settings column was dropped in migration 0007)
        sqlx::query("INSERT INTO accounts (pubkey, user_id, last_synced_at) VALUES ('test-pubkey', ?, NULL)")
            .bind(user_id)
            .execute(&db.pool)
            .await
            .expect("Failed to insert test account");

        sqlx::query("INSERT INTO media_files (mls_group_id, account_pubkey, file_path, original_file_hash, encrypted_file_hash, mime_type, media_type, created_at) VALUES (x'deadbeef', 'test-pubkey', '/path/test.jpg', NULL, 'testhash', 'image/jpeg', 'test', 1234567890)")
            .execute(&db.pool)
            .await
            .expect("Failed to insert test media file");

        // Verify data exists
        let user_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&db.pool)
            .await
            .expect("Failed to count users");
        assert_eq!(user_count.0, 1);

        let account_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
            .fetch_one(&db.pool)
            .await
            .expect("Failed to count accounts");
        assert_eq!(account_count.0, 1);

        let media_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM media_files")
            .fetch_one(&db.pool)
            .await
            .expect("Failed to count media files");
        assert_eq!(media_count.0, 1);

        // Delete all data
        let result = db.delete_all_data().await;
        assert!(result.is_ok());

        // Verify data is deleted
        let user_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&db.pool)
            .await
            .expect("Failed to count users after deletion");
        assert_eq!(user_count.0, 0);

        let account_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
            .fetch_one(&db.pool)
            .await
            .expect("Failed to count accounts after deletion");
        assert_eq!(account_count.0, 0);

        let media_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM media_files")
            .fetch_one(&db.pool)
            .await
            .expect("Failed to count media files after deletion");
        assert_eq!(media_count.0, 0);
    }

    #[tokio::test]
    async fn test_database_connection_pool() {
        let (db, _temp_dir) = create_test_db().await;

        // Test that we can acquire multiple connections
        let mut connections = Vec::new();

        for _ in 0..5 {
            let conn = db.pool.acquire().await;
            assert!(conn.is_ok());
            connections.push(conn.unwrap());
        }

        // Test that we can execute queries on different connections
        for mut conn in connections {
            let result: (i64,) = sqlx::query_as("SELECT 1")
                .fetch_one(&mut *conn)
                .await
                .expect("Failed to execute query on connection");
            assert_eq!(result.0, 1);
        }
    }

    #[tokio::test]
    async fn test_database_reopen_existing() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("test.db");

        // Create database first time
        let db1 = Database::new(db_path.clone())
            .await
            .expect("Failed to create database");

        // Insert some data with correct schema
        sqlx::query("INSERT INTO users (pubkey, metadata) VALUES ('test-pubkey', '{}')")
            .execute(&db1.pool)
            .await
            .expect("Failed to insert test user");

        let (user_id,): (i64,) =
            sqlx::query_as("SELECT id FROM users WHERE pubkey = 'test-pubkey'")
                .fetch_one(&db1.pool)
                .await
                .expect("Failed to get user ID");

        sqlx::query("INSERT INTO accounts (pubkey, user_id, last_synced_at) VALUES ('test-pubkey', ?, NULL)")
            .bind(user_id)
            .execute(&db1.pool)
            .await
            .expect("Failed to insert test account");

        drop(db1);

        // Reopen the same database
        let db2 = Database::new(db_path.clone())
            .await
            .expect("Failed to reopen database");

        // Verify data persists
        let user_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&db2.pool)
            .await
            .expect("Failed to count users");
        assert_eq!(user_count.0, 1);

        let account_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
            .fetch_one(&db2.pool)
            .await
            .expect("Failed to count accounts");
        assert_eq!(account_count.0, 1);

        // Verify the account data (current schema without settings column)
        let account: (String, i64) = sqlx::query_as("SELECT pubkey, user_id FROM accounts")
            .fetch_one(&db2.pool)
            .await
            .expect("Failed to fetch account");
        assert_eq!(account.0, "test-pubkey");
        assert_eq!(account.1, user_id); // Should match the user_id we created
    }

    #[tokio::test]
    async fn test_database_error_handling() {
        // Test with invalid path (this should still work as SQLite is quite permissive)
        let invalid_path = PathBuf::from("/invalid/path/that/should/fail.db");
        let result = Database::new(invalid_path).await;

        // This might succeed or fail depending on permissions, but shouldn't panic
        match result {
            Ok(_) => {
                // If it succeeds, that's fine too (SQLite might create the path)
            }
            Err(e) => {
                // Should be a proper DatabaseError
                match e {
                    DatabaseError::FileSystem(_) | DatabaseError::Sqlx(_) => {
                        // Expected error types
                    }
                    _ => panic!("Unexpected error type: {:?}", e),
                }
            }
        }
    }

    #[tokio::test]
    async fn test_migrate_up() {
        let (db, _temp_dir) = create_test_db().await;

        // Migrate up should work without errors (migrations already applied during creation)
        let result = db.migrate_up().await;
        assert!(result.is_ok());

        // Verify that all expected tables still exist (contacts table was dropped in migration 0007)
        let tables: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .fetch_all(&db.pool)
                .await
                .expect("Failed to fetch table names");

        let table_names: Vec<String> = tables.into_iter().map(|t| t.0).collect();
        assert!(table_names.contains(&"accounts".to_string()));
        assert!(table_names.contains(&"media_files".to_string()));
        assert!(table_names.contains(&"app_settings".to_string()));
    }

    /// Minimal mock implementing `sqlx::error::DatabaseError` for testing
    /// `is_sqlite_lock_error()` with specific error codes.
    #[derive(Debug)]
    struct MockDbError {
        code: Option<String>,
    }

    impl std::fmt::Display for MockDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "mock db error")
        }
    }

    impl std::error::Error for MockDbError {}

    impl sqlx::error::DatabaseError for MockDbError {
        fn message(&self) -> &str {
            "mock db error"
        }

        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            self.code.as_deref().map(std::borrow::Cow::Borrowed)
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn make_lock_error(code: &str) -> DatabaseError {
        DatabaseError::Sqlx(sqlx::Error::Database(Box::new(MockDbError {
            code: Some(code.to_string()),
        })))
    }

    #[test]
    fn test_is_sqlite_lock_error_true_for_busy() {
        assert!(make_lock_error("5").is_sqlite_lock_error());
    }

    #[test]
    fn test_is_sqlite_lock_error_true_for_locked() {
        assert!(make_lock_error("6").is_sqlite_lock_error());
    }

    #[test]
    fn test_is_sqlite_lock_error_true_for_extended_busy() {
        // 517 = SQLITE_BUSY_SNAPSHOT (517 & 0xFF == 5)
        assert!(make_lock_error("517").is_sqlite_lock_error());
    }

    #[test]
    fn test_is_sqlite_lock_error_true_for_extended_locked() {
        // 262 = SQLITE_LOCKED_SHAREDCACHE (262 & 0xFF == 6)
        assert!(make_lock_error("262").is_sqlite_lock_error());
    }

    #[test]
    fn test_is_sqlite_lock_error_false_for_row_not_found() {
        let err = DatabaseError::Sqlx(sqlx::Error::RowNotFound);
        assert!(!err.is_sqlite_lock_error());
    }

    #[test]
    fn test_is_sqlite_lock_error_false_for_other_db_code() {
        assert!(!make_lock_error("19").is_sqlite_lock_error());
    }

    #[test]
    fn test_is_sqlite_lock_error_false_for_serialization() {
        let json_err = serde_json::from_str::<String>("invalid").unwrap_err();
        let err = DatabaseError::Serialization(json_err);
        assert!(!err.is_sqlite_lock_error());
    }

    #[test]
    fn test_is_sqlite_lock_error_false_for_non_numeric_code() {
        assert!(!make_lock_error("abc").is_sqlite_lock_error());
    }

    #[tokio::test]
    async fn test_retry_on_lock_succeeds_first_try() {
        let result = retry_on_lock(|| async { Ok::<_, DatabaseError>(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn test_retry_on_lock_succeeds_after_transient_lock() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = counter.clone();

        let result = retry_on_lock(move || {
            let c = counter_clone.clone();
            async move {
                let attempt = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if attempt == 0 {
                    Err(make_lock_error("5"))
                } else {
                    Ok(99)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 99);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_retry_on_lock_exhausts_on_persistent_lock() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = counter.clone();

        let result = retry_on_lock(move || {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err::<(), _>(make_lock_error("5"))
            }
        })
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().is_sqlite_lock_error());
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_on_lock_no_retry_on_non_lock_error() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = counter.clone();

        let result = retry_on_lock(move || {
            let c = counter_clone.clone();
            async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err::<(), _>(DatabaseError::Sqlx(sqlx::Error::RowNotFound))
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Sets up a group_information row so FK constraints are satisfied.
    async fn setup_group(db: &Database, group_id: &mdk_core::prelude::GroupId) {
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO group_information (mls_group_id, group_type, created_at, updated_at)
             VALUES (?, 'group', ?, ?)",
        )
        .bind(group_id.as_slice())
        .bind(now)
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    /// Sets up an account (user + account rows) so FK constraints are satisfied.
    async fn setup_account(db: &Database, pubkey_hex: &str) {
        sqlx::query("INSERT INTO users (pubkey, metadata) VALUES (?, '{}')")
            .bind(pubkey_hex)
            .execute(&db.pool)
            .await
            .unwrap();
        let (user_id,): (i64,) = sqlx::query_as("SELECT id FROM users WHERE pubkey = ?")
            .bind(pubkey_hex)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO accounts (pubkey, user_id, last_synced_at) VALUES (?, ?, NULL)")
            .bind(pubkey_hex)
            .bind(user_id)
            .execute(&db.pool)
            .await
            .unwrap();
    }

    /// Creates an accounts_groups row.
    async fn setup_account_group(
        db: &Database,
        pubkey_hex: &str,
        group_id: &mdk_core::prelude::GroupId,
    ) -> i64 {
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO accounts_groups
             (account_pubkey, mls_group_id, user_confirmation, created_at, updated_at)
             VALUES (?, ?, 1, ?, ?)",
        )
        .bind(pubkey_hex)
        .bind(group_id.as_slice())
        .bind(now)
        .bind(now)
        .execute(&db.pool)
        .await
        .unwrap();
        let (id,): (i64,) = sqlx::query_as(
            "SELECT id FROM accounts_groups WHERE account_pubkey = ? AND mls_group_id = ?",
        )
        .bind(pubkey_hex)
        .bind(group_id.as_slice())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        id
    }

    /// Inserts a minimal aggregated_messages row.
    async fn setup_message(
        db: &Database,
        group_id: &mdk_core::prelude::GroupId,
        message_id_hex: &str,
        created_at_ms: i64,
    ) {
        let author_hex = "aa".repeat(32); // valid 64-char hex pubkey
        sqlx::query(
            "INSERT INTO aggregated_messages
             (message_id, mls_group_id, author, created_at, kind, content, tags,
              content_tokens, reactions, media_attachments)
             VALUES (?, ?, ?, ?, 9, '', '[]', '[]', '{}', '[]')",
        )
        .bind(message_id_hex)
        .bind(group_id.as_slice())
        .bind(&author_hex)
        .bind(created_at_ms)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    /// Returns the message count for a group.
    async fn message_count(db: &Database, group_id: &mdk_core::prelude::GroupId) -> i64 {
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM aggregated_messages WHERE mls_group_id = ?")
                .bind(group_id.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        count
    }

    #[tokio::test]
    async fn test_try_cleanup_cleared_messages_deletes_when_all_accounts_cleared() {
        let (db, _temp) = create_test_db().await;
        let group_id = mdk_core::prelude::GroupId::from_slice(&[1; 32]);
        let pk1 = "aa".repeat(32);
        let pk2 = "bb".repeat(32);

        setup_group(&db, &group_id).await;
        setup_account(&db, &pk1).await;
        setup_account(&db, &pk2).await;
        let ag1_id = setup_account_group(&db, &pk1, &group_id).await;
        let ag2_id = setup_account_group(&db, &pk2, &group_id).await;

        // Insert messages at times 100 and 200
        setup_message(&db, &group_id, &format!("{:0>64}", "1"), 100).await;
        setup_message(&db, &group_id, &format!("{:0>64}", "2"), 200).await;

        // Both accounts clear at 150 -- should delete only the message at 100
        sqlx::query("UPDATE accounts_groups SET chat_cleared_at = ? WHERE id = ?")
            .bind(150_i64)
            .bind(ag1_id)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE accounts_groups SET chat_cleared_at = ? WHERE id = ?")
            .bind(150_i64)
            .bind(ag2_id)
            .execute(&db.pool)
            .await
            .unwrap();

        let deleted = db.try_cleanup_cleared_messages(&group_id).await.unwrap();
        assert_eq!(deleted, 1, "only the message at t=100 should be deleted");
        assert_eq!(message_count(&db, &group_id).await, 1);
    }

    #[tokio::test]
    async fn test_try_cleanup_cleared_messages_respects_min() {
        let (db, _temp) = create_test_db().await;
        let group_id = mdk_core::prelude::GroupId::from_slice(&[2; 32]);
        let pk1 = "cc".repeat(32);
        let pk2 = "dd".repeat(32);

        setup_group(&db, &group_id).await;
        setup_account(&db, &pk1).await;
        setup_account(&db, &pk2).await;
        let ag1_id = setup_account_group(&db, &pk1, &group_id).await;
        let ag2_id = setup_account_group(&db, &pk2, &group_id).await;

        // Messages at 100, 200, 300
        setup_message(&db, &group_id, &format!("{:0>64}", "a"), 100).await;
        setup_message(&db, &group_id, &format!("{:0>64}", "b"), 200).await;
        setup_message(&db, &group_id, &format!("{:0>64}", "c"), 300).await;

        // Account 1 clears at 250, account 2 clears at 150
        // MIN = 150, so only message at 100 should be deleted
        sqlx::query("UPDATE accounts_groups SET chat_cleared_at = ? WHERE id = ?")
            .bind(250_i64)
            .bind(ag1_id)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE accounts_groups SET chat_cleared_at = ? WHERE id = ?")
            .bind(150_i64)
            .bind(ag2_id)
            .execute(&db.pool)
            .await
            .unwrap();

        let deleted = db.try_cleanup_cleared_messages(&group_id).await.unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(message_count(&db, &group_id).await, 2);
    }

    #[tokio::test]
    async fn test_try_cleanup_cleared_messages_noop_when_not_all_cleared() {
        let (db, _temp) = create_test_db().await;
        let group_id = mdk_core::prelude::GroupId::from_slice(&[3; 32]);
        let pk1 = "ee".repeat(32);
        let pk2 = "ff".repeat(32);

        setup_group(&db, &group_id).await;
        setup_account(&db, &pk1).await;
        setup_account(&db, &pk2).await;
        let ag1_id = setup_account_group(&db, &pk1, &group_id).await;
        setup_account_group(&db, &pk2, &group_id).await;

        setup_message(&db, &group_id, &format!("{:0>64}", "d"), 100).await;

        // Only account 1 clears -- account 2 still has chat_cleared_at = NULL
        sqlx::query("UPDATE accounts_groups SET chat_cleared_at = ? WHERE id = ?")
            .bind(200_i64)
            .bind(ag1_id)
            .execute(&db.pool)
            .await
            .unwrap();

        let deleted = db.try_cleanup_cleared_messages(&group_id).await.unwrap();
        assert_eq!(
            deleted, 0,
            "must not delete when some accounts have not cleared"
        );
        assert_eq!(message_count(&db, &group_id).await, 1);
    }

    #[tokio::test]
    async fn test_delete_chat_data_sole_account() {
        let (db, _temp) = create_test_db().await;
        let group_id = mdk_core::prelude::GroupId::from_slice(&[4; 32]);
        let pk1 = "11".repeat(32);

        setup_group(&db, &group_id).await;
        setup_account(&db, &pk1).await;
        setup_account_group(&db, &pk1, &group_id).await;
        setup_message(&db, &group_id, &format!("{:0>64}", "e"), 100).await;

        let pubkey = nostr_sdk::prelude::PublicKey::parse(&pk1).unwrap();
        let was_last = db.delete_chat_data(&pubkey, &group_id).await.unwrap();
        assert!(was_last, "sole account should return true");

        // Verify everything is gone
        let (gi_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM group_information WHERE mls_group_id = ?")
                .bind(group_id.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(gi_count, 0);

        let (ag_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM accounts_groups WHERE mls_group_id = ?")
                .bind(group_id.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(ag_count, 0);

        // Messages cascade from group_information delete
        assert_eq!(message_count(&db, &group_id).await, 0);
    }

    #[tokio::test]
    async fn test_delete_chat_data_multi_account() {
        let (db, _temp) = create_test_db().await;
        let group_id = mdk_core::prelude::GroupId::from_slice(&[5; 32]);
        let pk1 = "33".repeat(32);
        let pk2 = "44".repeat(32);

        setup_group(&db, &group_id).await;
        setup_account(&db, &pk1).await;
        setup_account(&db, &pk2).await;
        setup_account_group(&db, &pk1, &group_id).await;
        setup_account_group(&db, &pk2, &group_id).await;
        setup_message(&db, &group_id, &format!("{:0>64}", "f"), 100).await;

        let pubkey1 = nostr_sdk::prelude::PublicKey::parse(&pk1).unwrap();
        let pubkey2 = nostr_sdk::prelude::PublicKey::parse(&pk2).unwrap();

        // Account 1 deletes -- shared data should remain
        let was_last = db.delete_chat_data(&pubkey1, &group_id).await.unwrap();
        assert!(!was_last, "first account should return false");

        // A's row gone
        let (ag1_count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM accounts_groups
             WHERE account_pubkey = ? AND mls_group_id = ?",
        )
        .bind(&pk1)
        .bind(group_id.as_slice())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(ag1_count, 0, "A's account_group must be deleted");

        // B's row intact
        let (ag2_count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM accounts_groups
             WHERE account_pubkey = ? AND mls_group_id = ?",
        )
        .bind(&pk2)
        .bind(group_id.as_slice())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(ag2_count, 1, "B's account_group must still exist");

        // Shared data preserved
        let (gi_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM group_information WHERE mls_group_id = ?")
                .bind(group_id.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(gi_count, 1);
        assert_eq!(message_count(&db, &group_id).await, 1);

        // Account 2 deletes -- everything should be gone
        let was_last = db.delete_chat_data(&pubkey2, &group_id).await.unwrap();
        assert!(was_last, "last account should return true");

        let (gi_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM group_information WHERE mls_group_id = ?")
                .bind(group_id.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(gi_count, 0);
        assert_eq!(message_count(&db, &group_id).await, 0);
    }
}
