use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use mdk_sqlite_storage::{EncryptionConfig, keyring};
use sqlx::{ConnectOptions, Connection, Row, sqlite::SqliteConnectOptions};

use super::DatabaseError;

pub(super) const WHITENOISE_DB_KEY_ID: &str = "whitenoise.db.key.v1";

const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";
const MIGRATION_LOCK_STALE_SECS: u64 = 15 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DatabaseFileState {
    MissingOrEmpty,
    Plaintext,
    Encrypted,
}

pub(super) async fn prepare_sqlcipher_database(
    db_path: &Path,
    keyring_service_id: &str,
    key_id: &str,
) -> Result<EncryptionConfig, DatabaseError> {
    assert_sqlcipher_available().await?;

    let mut state = database_file_state(db_path)?;
    let config = match state {
        DatabaseFileState::Encrypted => existing_key(db_path, keyring_service_id, key_id)?,
        DatabaseFileState::MissingOrEmpty | DatabaseFileState::Plaintext => {
            get_or_create_key(keyring_service_id, key_id)?
        }
    };

    recover_interrupted_migration(db_path, &config).await?;
    state = database_file_state(db_path)?;

    match state {
        DatabaseFileState::Plaintext => migrate_plaintext_database(db_path, &config).await?,
        DatabaseFileState::Encrypted | DatabaseFileState::MissingOrEmpty => {}
    }

    Ok(config)
}

pub(super) fn cleanup_completed_migration(db_path: &Path) -> Result<(), DatabaseError> {
    if database_file_state(db_path)? == DatabaseFileState::Encrypted {
        remove_file_if_exists(&sidecar_path(db_path, ".encrypted.tmp"))?;
        remove_file_if_exists(&sidecar_path(db_path, ".plaintext.backup"))?;
    }

    Ok(())
}

pub(super) fn key_pragma_value(config: &EncryptionConfig) -> String {
    format!("\"x'{}'\"", hex::encode(config.key()))
}

pub(super) async fn validate_encrypted_database(
    db_path: &Path,
    config: &EncryptionConfig,
) -> Result<(), DatabaseError> {
    let mut conn = encrypted_connect_options(db_path, config, false)
        .connect()
        .await?;
    validate_encrypted_connection(&mut conn).await
}

#[cfg(test)]
pub(super) fn is_plaintext_sqlite_database(db_path: &Path) -> Result<bool, DatabaseError> {
    Ok(database_file_state(db_path)? == DatabaseFileState::Plaintext)
}

#[cfg(test)]
pub(super) fn is_sqlcipher_database(db_path: &Path) -> Result<bool, DatabaseError> {
    Ok(database_file_state(db_path)? == DatabaseFileState::Encrypted)
}

async fn assert_sqlcipher_available() -> Result<(), DatabaseError> {
    let mut conn = SqliteConnectOptions::new()
        .in_memory(true)
        .connect()
        .await?;
    let version = sqlcipher_version(&mut conn).await?;

    if version.is_empty() {
        return Err(DatabaseError::EncryptionMigration(
            "SQLite was built without SQLCipher support".to_string(),
        ));
    }

    Ok(())
}

async fn migrate_plaintext_database(
    db_path: &Path,
    config: &EncryptionConfig,
) -> Result<(), DatabaseError> {
    let lock_path = sidecar_path(db_path, ".encryption.lock");
    let _lock = MigrationLock::acquire(&lock_path)?;

    if database_file_state(db_path)? != DatabaseFileState::Plaintext {
        return Ok(());
    }

    let temp_path = sidecar_path(db_path, ".encrypted.tmp");
    let backup_path = sidecar_path(db_path, ".plaintext.backup");

    remove_file_if_exists(&temp_path)?;
    File::create(&temp_path)?;
    checkpoint_plaintext_database(db_path).await?;
    remove_sqlite_sidecars(db_path)?;

    export_plaintext_to_encrypted(db_path, &temp_path, config).await?;
    validate_encrypted_database(&temp_path, config).await?;

    remove_file_if_exists(&backup_path)?;
    fs::rename(db_path, &backup_path)?;

    if let Err(err) = fs::rename(&temp_path, db_path) {
        let _ = fs::rename(&backup_path, db_path);
        return Err(err.into());
    }

    if let Err(err) = validate_encrypted_database(db_path, config).await {
        fs::rename(&backup_path, db_path).map_err(|rollback_err| {
            DatabaseError::EncryptionMigration(format!(
                "Encrypted database validation failed ({err}); rollback from {} to {} failed: {rollback_err}",
                backup_path.display(),
                db_path.display(),
            ))
        })?;
        return Err(err);
    }

    remove_file_if_exists(&backup_path)?;

    Ok(())
}

async fn recover_interrupted_migration(
    db_path: &Path,
    config: &EncryptionConfig,
) -> Result<(), DatabaseError> {
    if db_path.exists() {
        let temp_path = sidecar_path(db_path, ".encrypted.tmp");
        if database_file_state(db_path)? == DatabaseFileState::Plaintext {
            remove_file_if_exists(&temp_path)?;
        }
        return Ok(());
    }

    let temp_path = sidecar_path(db_path, ".encrypted.tmp");
    let backup_path = sidecar_path(db_path, ".plaintext.backup");

    if temp_path.exists() {
        match validate_encrypted_database(&temp_path, config).await {
            Ok(()) => {
                fs::rename(&temp_path, db_path)?;
                return Ok(());
            }
            Err(err) => {
                if backup_path.exists() {
                    tracing::warn!(
                        target: "whitenoise::database",
                        "Encrypted temp database is invalid ({err}); restoring plaintext backup"
                    );
                    remove_file_if_exists(&temp_path)?;
                    fs::rename(&backup_path, db_path)?;
                    return Ok(());
                }

                return Err(err);
            }
        }
    }

    if backup_path.exists() {
        fs::rename(&backup_path, db_path)?;
    }

    Ok(())
}

async fn checkpoint_plaintext_database(db_path: &Path) -> Result<(), DatabaseError> {
    let mut conn = plaintext_connect_options(db_path, false).connect().await?;
    let (busy, log, checkpointed): (i64, i64, i64) =
        sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&mut conn)
            .await?;

    if busy != 0 || log != checkpointed {
        return Err(DatabaseError::EncryptionMigration(format!(
            "WAL checkpoint incomplete before encryption migration: busy={busy}, log={log}, checkpointed={checkpointed}"
        )));
    }

    conn.close().await?;
    Ok(())
}

async fn export_plaintext_to_encrypted(
    db_path: &Path,
    temp_path: &Path,
    config: &EncryptionConfig,
) -> Result<(), DatabaseError> {
    let mut conn = plaintext_connect_options(db_path, false).connect().await?;
    let temp_path_literal = sql_string_literal(&temp_path.display().to_string());
    let attach = format!(
        "ATTACH DATABASE {temp_path_literal} AS encrypted KEY {};",
        key_pragma_value(config)
    );

    sqlx::query(sqlx::AssertSqlSafe(attach))
        .execute(&mut conn)
        .await?;
    sqlx::query("PRAGMA encrypted.cipher_compatibility = 4")
        .execute(&mut conn)
        .await?;
    sqlx::query("SELECT sqlcipher_export('encrypted')")
        .execute(&mut conn)
        .await?;
    sqlx::query("DETACH DATABASE encrypted")
        .execute(&mut conn)
        .await?;

    conn.close().await?;
    Ok(())
}

async fn validate_encrypted_connection(
    conn: &mut sqlx::SqliteConnection,
) -> Result<(), DatabaseError> {
    let version = sqlcipher_version(conn).await?;

    if version.is_empty() {
        return Err(DatabaseError::EncryptionMigration(
            "SQLite connection did not report a SQLCipher version".to_string(),
        ));
    }

    sqlx::query("SELECT count(*) FROM sqlite_master")
        .fetch_one(&mut *conn)
        .await?;

    let row = sqlx::query("PRAGMA integrity_check")
        .fetch_one(&mut *conn)
        .await?;
    let integrity: String = row.try_get(0)?;

    if integrity != "ok" {
        return Err(DatabaseError::EncryptionMigration(format!(
            "Encrypted database integrity check failed: {integrity}"
        )));
    }

    Ok(())
}

async fn sqlcipher_version(conn: &mut sqlx::SqliteConnection) -> Result<String, DatabaseError> {
    let row = sqlx::query("PRAGMA cipher_version")
        .fetch_optional(conn)
        .await?;

    match row {
        Some(row) => Ok(row.try_get::<String, _>(0).unwrap_or_default()),
        None => Ok(String::new()),
    }
}

fn existing_key(
    db_path: &Path,
    keyring_service_id: &str,
    key_id: &str,
) -> Result<EncryptionConfig, DatabaseError> {
    let config = keyring::get_db_key(keyring_service_id, key_id)
        .map_err(|e| DatabaseError::EncryptionKey(e.to_string()))?;

    match config {
        Some(config) => Ok(config),
        None => Err(DatabaseError::MissingEncryptionKey {
            path: db_path.display().to_string(),
            key_id: key_id.to_string(),
        }),
    }
}

fn get_or_create_key(
    keyring_service_id: &str,
    key_id: &str,
) -> Result<EncryptionConfig, DatabaseError> {
    keyring::get_or_create_db_key(keyring_service_id, key_id)
        .map_err(|e| DatabaseError::EncryptionKey(e.to_string()))
}

fn database_file_state(db_path: &Path) -> Result<DatabaseFileState, DatabaseError> {
    if !db_path.exists() {
        return Ok(DatabaseFileState::MissingOrEmpty);
    }

    if db_path.metadata()?.len() == 0 {
        return Ok(DatabaseFileState::MissingOrEmpty);
    }

    let mut file = File::open(db_path)?;
    let mut header = [0_u8; 16];

    match file.read_exact(&mut header) {
        Ok(()) if header == *SQLITE_HEADER => Ok(DatabaseFileState::Plaintext),
        Ok(()) => Ok(DatabaseFileState::Encrypted),
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => Ok(DatabaseFileState::Encrypted),
        Err(err) => Err(err.into()),
    }
}

fn plaintext_connect_options(db_path: &Path, create: bool) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(create)
        .busy_timeout(Duration::from_millis(u64::from(super::DB_BUSY_TIMEOUT_MS)))
}

pub(super) fn encrypted_connect_options(
    db_path: &Path,
    config: &EncryptionConfig,
    create: bool,
) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(create)
        .pragma("key", key_pragma_value(config))
        .pragma("cipher_compatibility", "4")
        .pragma("temp_store", "MEMORY")
        .busy_timeout(Duration::from_millis(u64::from(super::DB_BUSY_TIMEOUT_MS)))
}

fn remove_sqlite_sidecars(db_path: &Path) -> Result<(), DatabaseError> {
    remove_file_if_exists(&sidecar_path(db_path, "-wal"))?;
    remove_file_if_exists(&sidecar_path(db_path, "-shm"))?;
    remove_file_if_exists(&sidecar_path(db_path, "-journal"))?;
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<(), DatabaseError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut path = db_path.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

// SQLite's ATTACH DATABASE does not accept bind parameters — only literal strings — so we
// must embed the path directly with proper SQL single-quote escaping.
fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

struct MigrationLock {
    path: PathBuf,
}

impl MigrationLock {
    fn acquire(path: &Path) -> Result<Self, DatabaseError> {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(_) => Ok(Self {
                path: path.to_path_buf(),
            }),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                if Self::is_stale(path)? {
                    remove_file_if_exists(path)?;
                    match OpenOptions::new().write(true).create_new(true).open(path) {
                        Ok(_) => {
                            return Ok(Self {
                                path: path.to_path_buf(),
                            });
                        }
                        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                            return Err(DatabaseError::EncryptionMigration(format!(
                                "Database encryption migration is already running: {}",
                                path.display()
                            )));
                        }
                        Err(err) => return Err(err.into()),
                    }
                }

                Err(DatabaseError::EncryptionMigration(format!(
                    "Database encryption migration is already running: {}",
                    path.display()
                )))
            }
            Err(err) => Err(err.into()),
        }
    }

    fn is_stale(path: &Path) -> Result<bool, DatabaseError> {
        let modified = path
            .metadata()?
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let age = modified.elapsed().unwrap_or_default();
        Ok(age.as_secs() > MIGRATION_LOCK_STALE_SECS)
    }
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
