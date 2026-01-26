//! Helper module for running MDK operations outside the tokio runtime.
//!
//! MDK uses `tokio::sync::Mutex::blocking_lock()` internally which panics when called
//! from within a tokio runtime context. This module provides a helper function to run
//! MDK operations on a separate thread, completely outside the runtime.

use std::path::Path;

use mdk_core::MDK;
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::PublicKey;

use crate::whitenoise::error::{Result, WhitenoiseError};

/// Runs an MDK operation on a separate thread outside the tokio runtime.
///
/// This function:
/// 1. Creates an MdkSqliteStorage for the given pubkey
/// 2. Wraps it in an MDK instance
/// 3. Passes it to the provided closure
/// 4. Returns the result asynchronously
///
/// # Arguments
///
/// * `pubkey` - The public key identifying the account
/// * `data_dir` - The data directory path
/// * `operation` - A closure that takes an MDK instance and returns a Result
///
/// # Example
///
/// ```ignore
/// let groups = run_mdk_operation(pubkey, &data_dir, |mdk| {
///     mdk.get_groups().map_err(Into::into)
/// }).await?;
/// ```
pub async fn run_mdk_operation<T, F>(pubkey: PublicKey, data_dir: &Path, operation: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(MDK<MdkSqliteStorage>) -> Result<T> + Send + 'static,
{
    let data_dir = data_dir.to_path_buf();

    // Use std::thread::spawn to completely exit the tokio runtime context.
    // spawn_blocking threads still have a runtime handle which causes
    // blocking_lock() to panic.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = create_mdk_and_run(pubkey, &data_dir, operation);
        let _ = tx.send(result);
    });

    // Wait for the result asynchronously using spawn_blocking to avoid
    // blocking the async runtime while waiting on the channel
    tokio::task::spawn_blocking(move || rx.recv())
        .await
        .map_err(WhitenoiseError::JoinError)?
        .map_err(|e| WhitenoiseError::Configuration(format!("Channel receive error: {}", e)))?
}

/// Creates an MDK instance and runs the operation synchronously.
fn create_mdk_and_run<T, F>(pubkey: PublicKey, data_dir: &Path, operation: F) -> Result<T>
where
    F: FnOnce(MDK<MdkSqliteStorage>) -> Result<T>,
{
    let mls_storage_dir = data_dir.join("mls").join(pubkey.to_hex());
    let db_key_id = format!("mdk.db.key.{}", pubkey.to_hex());
    let storage = MdkSqliteStorage::new(mls_storage_dir, "com.whitenoise.app", &db_key_id)
        .map_err(|e| WhitenoiseError::Configuration(format!("MdkSqliteStorage error: {}", e)))?;
    let mdk = MDK::new(storage);

    operation(mdk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::Keys;
    use std::sync::OnceLock;
    use tempfile::TempDir;

    /// Initialize mock keyring store for tests (only once per process)
    fn ensure_mock_keyring_store() {
        static MOCK_STORE_INIT: OnceLock<()> = OnceLock::new();
        MOCK_STORE_INIT.get_or_init(|| {
            keyring_core::set_default_store(keyring_core::mock::Store::new().unwrap());
        });
    }

    #[tokio::test]
    async fn test_run_mdk_operation_get_groups() {
        ensure_mock_keyring_store();

        let temp_dir = TempDir::new().unwrap();
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let groups = run_mdk_operation(pubkey, temp_dir.path(), |mdk| {
            mdk.get_groups().map_err(Into::into)
        })
        .await
        .unwrap();

        assert!(groups.is_empty());
    }
}
