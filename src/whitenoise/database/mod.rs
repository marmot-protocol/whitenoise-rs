use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use mdk_sqlite_storage::EncryptionConfig;
use sqlx::{
    ConnectOptions, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use thiserror::Error;

pub mod account;
pub mod account_db;
mod encryption;

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
pub mod shared_db;
pub mod user_relays;
pub mod users;
pub mod utils;

pub mod rust_migrations;

const DB_ACQUIRE_TIMEOUT_SECS: u64 = 5;
const DB_MAX_CONNECTIONS: u32 = 10;
const DB_BUSY_TIMEOUT_MS: u32 = 5000;

#[derive(Error, Debug)]
pub enum DatabaseError {
    #[error("SQLx error: {0}")]
    Sqlx(#[from] sqlx::Error),
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
    #[error("User {0} has no database id; pass a persisted User from find_or_create_by_pubkey")]
    MissingUserId(String),
    #[error("Database encryption key error: {0}")]
    EncryptionKey(String),
    #[error("Encrypted database {path} is missing keyring key {key_id}")]
    MissingEncryptionKey { path: String, key_id: String },
    #[error("Database encryption migration error: {0}")]
    EncryptionMigration(String),
    #[error("Not found: {0}")]
    NotFound(String),
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
    /// Open or create the SQLite file at `db_path` and run the unified
    /// migrator in shared-only mode (`account = None`).
    ///
    /// **Not safe for per-account DB files.** In shared-only mode the pool
    /// passed here receives global migrations — including drop migrations
    /// (e.g. m0037) that destroy tables which local migrations created.
    /// Use `open_without_migrations` + `AccountDatabase::run_account_migrations`
    /// for account DBs instead.
    ///
    /// **Not safe for the shared DB once the unified timeline contains local
    /// migrations** (Phase 18c+): in shared-only mode the migrator skips
    /// locals but advances past their version numbers, so any global drop
    /// migration sitting after a local will fire before the local has run
    /// for any account on disk. Use `open_without_migrations` +
    /// `MIGRATOR.run_all` instead when there are accounts to migrate; the
    /// latter walks every per-account DB in lockstep with shared.
    ///
    /// Safe to use for: fresh installs where shared has no data yet (drops
    /// are no-ops on empty tables); test helpers that want a one-shot
    /// fully-migrated shared-shape DB.
    pub async fn new(db_path: PathBuf) -> Result<Self, DatabaseError> {
        let db = Self::open_without_migrations(db_path, None).await?;
        rust_migrations::MIGRATOR.run(&db.pool, None).await?;
        Ok(db)
    }

    pub async fn new_encrypted(
        db_path: PathBuf,
        keyring_service_id: &str,
    ) -> Result<Self, DatabaseError> {
        Self::open_encrypted(
            db_path,
            keyring_service_id,
            encryption::WHITENOISE_DB_KEY_ID,
        )
        .await
    }

    #[cfg(any(test, feature = "integration-tests", feature = "benchmark-tests"))]
    pub(crate) async fn new_encrypted_with_key_id(
        db_path: PathBuf,
        keyring_service_id: &str,
        key_id: &str,
    ) -> Result<Self, DatabaseError> {
        Self::open_encrypted(db_path, keyring_service_id, key_id).await
    }

    async fn open_encrypted(
        db_path: PathBuf,
        keyring_service_id: &str,
        key_id: &str,
    ) -> Result<Self, DatabaseError> {
        let encryption_config =
            encryption::prepare_sqlcipher_database(&db_path, keyring_service_id, key_id).await?;
        let database =
            Self::open_without_migrations(db_path.clone(), Some(&encryption_config)).await?;
        encryption::cleanup_completed_migration(&db_path)?;
        Ok(database)
    }

    /// Open or create the SQLite file at `db_path` without running any
    /// migrations. Pair with `MIGRATOR.run_all` (or
    /// `AccountDatabase::run_account_migrations`) to apply the timeline.
    pub(crate) async fn open_without_migrations(
        db_path: PathBuf,
        encryption_config: Option<&EncryptionConfig>,
    ) -> Result<Self, DatabaseError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let pool = Self::create_connection_pool(&db_path, encryption_config).await?;

        Ok(Self {
            pool,
            path: db_path,
            last_connected: SystemTime::now(),
        })
    }

    /// Creates and configures a SQLite connection pool
    async fn create_connection_pool(
        db_path: &Path,
        encryption_config: Option<&EncryptionConfig>,
    ) -> Result<SqlitePool, DatabaseError> {
        tracing::debug!("Creating connection pool...");

        // Log every SQL statement only when explicitly opted in (e.g. benchmarks).
        // Slow-query logging stays unconditional as a safety net.
        let log_statements_level = if std::env::var("SQLX_LOG_STATEMENTS").is_ok() {
            tracing::log::LevelFilter::Info
        } else {
            tracing::log::LevelFilter::Off
        };

        let connect_options = Self::connect_options(db_path, encryption_config)
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

    fn connect_options(
        db_path: &Path,
        encryption_config: Option<&EncryptionConfig>,
    ) -> SqliteConnectOptions {
        match encryption_config {
            Some(config) => encryption::encrypted_connect_options(db_path, config, true),
            None => SqliteConnectOptions::new()
                .filename(db_path)
                .create_if_missing(true)
                .busy_timeout(Duration::from_millis(u64::from(DB_BUSY_TIMEOUT_MS))),
        }
    }

    /// Runs all pending database migrations
    ///
    /// This method is idempotent - it's safe to call multiple times.
    /// Only new migrations will be applied.
    pub async fn migrate_up(&self) -> Result<(), DatabaseError> {
        rust_migrations::MIGRATOR.run(&self.pool, None).await?;
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
        // Acquire a single connection so the PRAGMA and transaction share the
        // same session. PRAGMA foreign_keys cannot be changed inside a
        // transaction in SQLite, so we set it before BEGIN.
        let mut conn = self.pool.acquire().await?;

        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *conn)
            .await?;

        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

        // Get all user tables (excluding SQLite system tables and migration tracking)
        let tables: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master
             WHERE type='table'
             AND name NOT LIKE 'sqlite_%'
             AND name NOT IN ('_sqlx_migrations', '_rust_migrations')",
        )
        .fetch_all(&mut *conn)
        .await?;

        // Delete all rows from each table (preserves schema)
        for (table_name,) in &tables {
            let delete_query = format!("DELETE FROM {}", table_name);
            sqlx::query(sqlx::AssertSqlSafe(delete_query))
                .execute(&mut *conn)
                .await?;
        }

        // Reset auto-increment counters for all tables
        for (table_name,) in &tables {
            // This resets the ROWID counter
            // Ignore errors - table might not have auto-increment
            let _ = sqlx::query("DELETE FROM sqlite_sequence WHERE name = ?")
                .bind(table_name)
                .execute(&mut *conn)
                .await;
        }

        sqlx::query("COMMIT").execute(&mut *conn).await?;

        // Re-enable foreign key constraints.
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&mut *conn)
            .await?;

        Ok(())
    }

    /// Delete aggregated messages whose `created_at <= threshold`.
    ///
    /// Intended for the per-account cleanup path: after `clear_chat` advances
    /// this account's `chat_cleared_at`, the caller passes that timestamp here
    /// to drop the now-invisible message rows from this account's per-account
    /// DB. Cross-account coordination is irrelevant because each account owns
    /// its own copy of `aggregated_messages` post-phase-18e.
    ///
    /// Pass `None` to make the call a no-op. Returns the number of rows
    /// deleted.
    pub(crate) async fn try_cleanup_cleared_messages(
        &self,
        mls_group_id: &mdk_core::prelude::GroupId,
        cleared_at_ms: Option<i64>,
    ) -> Result<u64, DatabaseError> {
        let Some(threshold) = cleared_at_ms else {
            return Ok(0);
        };
        retry_on_lock(|| self.try_cleanup_cleared_messages_inner(mls_group_id, threshold)).await
    }

    async fn try_cleanup_cleared_messages_inner(
        &self,
        mls_group_id: &mdk_core::prelude::GroupId,
        threshold: i64,
    ) -> Result<u64, DatabaseError> {
        let result = sqlx::query(
            "DELETE FROM aggregated_messages
             WHERE mls_group_id = ? AND created_at <= ?",
        )
        .bind(mls_group_id.as_slice())
        .bind(threshold)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// Delete shared-DB data for a group when the last account leaves it.
    ///
    /// All per-account data (accounts_groups, drafts, group_push_tokens,
    /// media_references, aggregated_messages, message_delivery_status) lives
    /// in per-account DBs and is removed by the caller. This method only
    /// removes the shared `group_information` row (and anything still
    /// cascading off it inside the shared DB).
    pub(crate) async fn delete_shared_group_data(
        &self,
        mls_group_id: &mdk_core::prelude::GroupId,
        is_last_account: bool,
    ) -> Result<(), DatabaseError> {
        if is_last_account {
            retry_on_lock(|| self.delete_shared_group_data_inner(mls_group_id)).await
        } else {
            Ok(())
        }
    }

    async fn delete_shared_group_data_inner(
        &self,
        mls_group_id: &mdk_core::prelude::GroupId,
    ) -> Result<(), DatabaseError> {
        sqlx::query("DELETE FROM group_information WHERE mls_group_id = ?")
            .bind(mls_group_id.as_slice())
            .execute(&self.pool)
            .await?;

        Ok(())
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
    use std::{
        fs::{self, File},
        io::Read,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use mdk_sqlite_storage::keyring;
    use tempfile::TempDir;

    use crate::whitenoise::Whitenoise;

    const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

    fn unique_service_id() -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("com.whitenoise.database-test.{id}")
    }

    fn read_db_header(db_path: &Path) -> [u8; 16] {
        let mut file = File::open(db_path).expect("Failed to open database file");
        let mut header = [0_u8; 16];
        file.read_exact(&mut header)
            .expect("Failed to read database header");
        header
    }

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
    async fn test_encrypted_database_creation() {
        Whitenoise::initialize_mock_keyring_store();
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("encrypted.db");
        let service_id = unique_service_id();

        let db = Database::new_encrypted(db_path.clone(), &service_id)
            .await
            .expect("Failed to create encrypted database");

        assert_eq!(db.path, db_path);
        assert!(db_path.exists());
        assert_ne!(read_db_header(&db_path), *SQLITE_HEADER);
        assert!(encryption::is_sqlcipher_database(&db_path).unwrap());

        let result =
            sqlx::query("SELECT name FROM sqlite_master WHERE type='table' AND name='accounts'")
                .fetch_optional(&db.pool)
                .await
                .expect("Failed to check accounts table");

        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_encrypted_database_reopen_existing() {
        Whitenoise::initialize_mock_keyring_store();
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("encrypted-reopen.db");
        let service_id = unique_service_id();
        let pubkey = "aa".repeat(32);

        let db = Database::new_encrypted(db_path.clone(), &service_id)
            .await
            .expect("Failed to create encrypted database");
        setup_account(&db, &pubkey).await;
        drop(db);

        let reopened = Database::new_encrypted(db_path.clone(), &service_id)
            .await
            .expect("Failed to reopen encrypted database");

        let account_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
            .fetch_one(&reopened.pool)
            .await
            .expect("Failed to count accounts");

        assert_eq!(account_count.0, 1);
        assert_ne!(read_db_header(&db_path), *SQLITE_HEADER);
    }

    #[tokio::test]
    async fn test_plaintext_database_migrates_to_encrypted_without_data_loss() {
        Whitenoise::initialize_mock_keyring_store();
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("migrate.db");
        let service_id = unique_service_id();
        let pubkey = "bb".repeat(32);

        let plaintext = Database::new(db_path.clone())
            .await
            .expect("Failed to create plaintext database");
        setup_account(&plaintext, &pubkey).await;
        drop(plaintext);

        assert_eq!(read_db_header(&db_path), *SQLITE_HEADER);
        assert!(encryption::is_plaintext_sqlite_database(&db_path).unwrap());

        let encrypted = Database::new_encrypted(db_path.clone(), &service_id)
            .await
            .expect("Failed to migrate plaintext database");

        let account_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
            .fetch_one(&encrypted.pool)
            .await
            .expect("Failed to count migrated accounts");

        assert_eq!(account_count.0, 1);
        assert_ne!(read_db_header(&db_path), *SQLITE_HEADER);
        assert!(encryption::is_sqlcipher_database(&db_path).unwrap());
        assert!(!PathBuf::from(format!("{}.plaintext.backup", db_path.display())).exists());
    }

    #[tokio::test]
    async fn test_interrupted_migration_restores_plaintext_backup_when_database_missing() {
        Whitenoise::initialize_mock_keyring_store();
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("recover-backup.db");
        let backup_path = PathBuf::from(format!("{}.plaintext.backup", db_path.display()));
        let service_id = unique_service_id();
        let pubkey = "cc".repeat(32);

        let plaintext = Database::new(db_path.clone())
            .await
            .expect("Failed to create plaintext database");
        setup_account(&plaintext, &pubkey).await;
        sqlx::query_as::<_, (i64, i64, i64)>("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&plaintext.pool)
            .await
            .expect("Failed to checkpoint plaintext database");
        drop(plaintext);

        fs::rename(&db_path, &backup_path).expect("Failed to move plaintext database to backup");

        assert!(!db_path.exists());
        assert_eq!(read_db_header(&backup_path), *SQLITE_HEADER);

        let encrypted = Database::new_encrypted(db_path.clone(), &service_id)
            .await
            .expect("Failed to recover and migrate plaintext backup");

        let account_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
            .fetch_one(&encrypted.pool)
            .await
            .expect("Failed to count recovered accounts");

        assert_eq!(account_count.0, 1);
        assert!(db_path.exists());
        assert!(!backup_path.exists());
        assert_ne!(read_db_header(&db_path), *SQLITE_HEADER);
        assert!(encryption::is_sqlcipher_database(&db_path).unwrap());
    }

    #[tokio::test]
    async fn test_encrypted_database_requires_existing_key() {
        Whitenoise::initialize_mock_keyring_store();
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let db_path = temp_dir.path().join("missing-key.db");
        let service_id = unique_service_id();

        let db = Database::new_encrypted(db_path.clone(), &service_id)
            .await
            .expect("Failed to create encrypted database");
        drop(db);

        keyring::delete_db_key(&service_id, encryption::WHITENOISE_DB_KEY_ID)
            .expect("Failed to delete database key");

        let err = Database::new_encrypted(db_path, &service_id)
            .await
            .expect_err("Encrypted database should require its existing key");

        assert!(matches!(err, DatabaseError::MissingEncryptionKey { .. }));
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

        // Check that the media_blobs table exists
        let result =
            sqlx::query("SELECT name FROM sqlite_master WHERE type='table' AND name='media_blobs'")
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

        sqlx::query("INSERT INTO media_blobs (encrypted_file_hash, file_path, mime_type, created_at) VALUES ('testhash', '/path/test.jpg', 'image/jpeg', 1234567890)")
            .execute(&db.pool)
            .await
            .expect("Failed to insert test media blob");

        // media_references now lives in per-account DBs (moved by v29, dropped
        // from shared by v30), so we only verify shared tables here.

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

        let blob_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM media_blobs")
            .fetch_one(&db.pool)
            .await
            .expect("Failed to count media blobs");
        assert_eq!(blob_count.0, 1);

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

        let blob_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM media_blobs")
            .fetch_one(&db.pool)
            .await
            .expect("Failed to count media blobs after deletion");
        assert_eq!(blob_count.0, 0);
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
        assert!(table_names.contains(&"media_blobs".to_string()));
        // media_references was moved to per-account DBs (v29) and dropped
        // from shared (v30), so it should NOT exist here.
        assert!(!table_names.contains(&"media_references".to_string()));
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

    /// Creates the per-account-shape `aggregated_messages` table on a generic
    /// test DB. Phase 18e moved this table out of the shared schema, so tests
    /// that exercise per-account-DB methods (`try_cleanup_cleared_messages`)
    /// must materialise the table themselves.
    async fn setup_aggregated_messages_table(db: &Database) {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS aggregated_messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id TEXT NOT NULL,
                mls_group_id BLOB NOT NULL,
                author TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                kind INTEGER NOT NULL,
                content TEXT NOT NULL DEFAULT '',
                tags JSONB NOT NULL,
                reply_to_id TEXT,
                deletion_event_id TEXT,
                content_tokens JSONB NOT NULL,
                reactions JSONB NOT NULL,
                media_attachments JSONB NOT NULL,
                content_normalized TEXT NOT NULL DEFAULT '',
                UNIQUE(message_id, mls_group_id)
            )",
        )
        .execute(&db.pool)
        .await
        .unwrap();
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

        setup_group(&db, &group_id).await;
        setup_aggregated_messages_table(&db).await;

        // Insert messages at times 100 and 200
        setup_message(&db, &group_id, &format!("{:0>64}", "1"), 100).await;
        setup_message(&db, &group_id, &format!("{:0>64}", "2"), 200).await;

        // Both accounts cleared at 150 — should delete only the message at 100
        let deleted = db
            .try_cleanup_cleared_messages(&group_id, Some(150))
            .await
            .unwrap();
        assert_eq!(deleted, 1, "only the message at t=100 should be deleted");
        assert_eq!(message_count(&db, &group_id).await, 1);
    }

    #[tokio::test]
    async fn test_try_cleanup_cleared_messages_respects_min() {
        let (db, _temp) = create_test_db().await;
        let group_id = mdk_core::prelude::GroupId::from_slice(&[2; 32]);

        setup_group(&db, &group_id).await;
        setup_aggregated_messages_table(&db).await;

        // Messages at 100, 200, 300
        setup_message(&db, &group_id, &format!("{:0>64}", "a"), 100).await;
        setup_message(&db, &group_id, &format!("{:0>64}", "b"), 200).await;
        setup_message(&db, &group_id, &format!("{:0>64}", "c"), 300).await;

        // Account 1 cleared at 250, account 2 cleared at 150.
        // Caller resolves MIN = 150, so only message at 100 should be deleted.
        let deleted = db
            .try_cleanup_cleared_messages(&group_id, Some(150))
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(message_count(&db, &group_id).await, 2);
    }

    #[tokio::test]
    async fn test_try_cleanup_cleared_messages_noop_when_not_all_cleared() {
        let (db, _temp) = create_test_db().await;
        let group_id = mdk_core::prelude::GroupId::from_slice(&[3; 32]);

        setup_group(&db, &group_id).await;
        setup_aggregated_messages_table(&db).await;
        setup_message(&db, &group_id, &format!("{:0>64}", "d"), 100).await;

        // Not all accounts cleared — caller passes None
        let deleted = db
            .try_cleanup_cleared_messages(&group_id, None)
            .await
            .unwrap();
        assert_eq!(
            deleted, 0,
            "must not delete when some accounts have not cleared"
        );
        assert_eq!(message_count(&db, &group_id).await, 1);
    }

    #[tokio::test]
    async fn test_delete_shared_group_data_last_account() {
        let (db, _temp) = create_test_db().await;
        let group_id = mdk_core::prelude::GroupId::from_slice(&[4; 32]);

        setup_group(&db, &group_id).await;

        // Delete shared data as last account.
        db.delete_shared_group_data(&group_id, true).await.unwrap();

        // Verify group_information is gone. Per-account aggregated_messages
        // cleanup is the caller's responsibility (delete_chat) post phase 18e —
        // this method only owns the shared-DB row.
        let (gi_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM group_information WHERE mls_group_id = ?")
                .bind(group_id.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(gi_count, 0);
    }

    #[tokio::test]
    async fn test_delete_shared_group_data_not_last_account() {
        let (db, _temp) = create_test_db().await;
        let group_id = mdk_core::prelude::GroupId::from_slice(&[5; 32]);

        setup_group(&db, &group_id).await;

        // Delete shared data as non-last account -- should be a no-op
        db.delete_shared_group_data(&group_id, false).await.unwrap();

        // Shared row preserved
        let (gi_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM group_information WHERE mls_group_id = ?")
                .bind(group_id.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(gi_count, 1);

        // Now delete as last account -- shared row should be gone
        db.delete_shared_group_data(&group_id, true).await.unwrap();

        let (gi_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM group_information WHERE mls_group_id = ?")
                .bind(group_id.as_slice())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(gi_count, 0);
    }
}
