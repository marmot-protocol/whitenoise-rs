use std::{
    path::PathBuf,
    sync::LazyLock,
    time::{Duration, SystemTime},
};

use sqlx::{
    Sqlite, SqlitePool,
    migrate::{MigrateDatabase, Migrator},
    sqlite::SqlitePoolOptions,
};
use thiserror::Error;

pub mod accounts;
pub mod accounts_groups;
pub mod aggregated_messages;
pub mod app_settings;
pub mod group_information;
pub mod media_files;
pub mod processed_events;
pub mod published_events;
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
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
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
                    sqlx::query(&format!("PRAGMA busy_timeout={DB_BUSY_TIMEOUT_MS}"))
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
            .connect(&format!("{db_url}?mode=rwc"))
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

    /// Deletes all data by dropping and recreating all tables
    ///
    /// This method:
    /// - Temporarily disables foreign key constraints
    /// - Drops all user tables (including migration tracking)
    /// - Re-enables foreign key constraints
    /// - Re-runs migrations to recreate the current schema from scratch
    /// - Uses a transaction to ensure atomicity
    pub async fn delete_all_data(&self) -> Result<(), DatabaseError> {
        let mut txn = self.pool.begin().await?;

        // Disable foreign key constraints temporarily to allow dropping tables in any order
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *txn)
            .await?;

        // Get all user tables (excluding only SQLite system tables)
        let tables: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master
             WHERE type='table'
             AND name NOT LIKE 'sqlite_%'",
        )
        .fetch_all(&mut *txn)
        .await?;

        // Drop all user tables (order doesn't matter with FK constraints disabled)
        for (table_name,) in tables {
            let drop_query = format!("DROP TABLE IF EXISTS {}", table_name);
            sqlx::query(&drop_query).execute(&mut *txn).await?;
        }

        // Re-enable foreign key constraints
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&mut *txn)
            .await?;

        txn.commit().await?;

        // Re-run migrations to recreate the current schema
        MIGRATOR.run(&self.pool).await?;

        Ok(())
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

    #[tokio::test]
    async fn test_database_clone() {
        let (db, _temp_dir) = create_test_db().await;

        // Test that Database can be cloned
        let db_clone = db.clone();

        // Both should have the same path
        assert_eq!(db.path, db_clone.path);

        // Both should be able to execute queries
        let result1: (i64,) = sqlx::query_as("SELECT 1")
            .fetch_one(&db.pool)
            .await
            .expect("Failed to execute query on original");

        let result2: (i64,) = sqlx::query_as("SELECT 2")
            .fetch_one(&db_clone.pool)
            .await
            .expect("Failed to execute query on clone");

        assert_eq!(result1.0, 1);
        assert_eq!(result2.0, 2);
    }
}
