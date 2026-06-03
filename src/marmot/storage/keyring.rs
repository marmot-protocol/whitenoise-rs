use std::sync::{Mutex, OnceLock};

use cgka_traits::storage::{StorageError, StorageResult};
use keyring_core::{Entry, Error as KeyringError};

use super::connection::SqlCipherKey;

static KEY_GENERATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
const KEYRING_PAYLOAD_PREFIX: &str = "whitenoise-marmot-sqlcipher-key-v1:";

pub(super) fn get_or_create_db_key(
    service_id: &str,
    db_key_id: &str,
) -> StorageResult<SqlCipherKey> {
    if let Some(key) = get_db_key(service_id, db_key_id)? {
        return Ok(key);
    }

    let lock = KEY_GENERATION_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock
        .lock()
        .map_err(|err| StorageError::Backend(format!("key generation lock poisoned: {err}")))?;

    if let Some(key) = get_db_key(service_id, db_key_id)? {
        return Ok(key);
    }

    let key = SqlCipherKey::generate()?;
    save_db_key(service_id, db_key_id, &key)?;
    Ok(key)
}

pub(super) fn delete_db_key(service_id: &str, db_key_id: &str) -> StorageResult<()> {
    let entry = keyring_entry(service_id, db_key_id)?;
    match entry.delete_credential() {
        Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
        Err(KeyringError::NoStorageAccess(err)) => Err(StorageError::Backend(format!(
            "keyring not initialized: {err}"
        ))),
        Err(err) => Err(StorageError::Backend(format!(
            "failed to delete Marmot database key: {err}"
        ))),
    }
}

pub(super) fn get_db_key(service_id: &str, db_key_id: &str) -> StorageResult<Option<SqlCipherKey>> {
    let entry = keyring_entry(service_id, db_key_id)?;
    match entry.get_secret() {
        Ok(secret) => Ok(Some(decode_keyring_payload(&secret)?)),
        Err(KeyringError::NoEntry) => Ok(None),
        Err(KeyringError::NoStorageAccess(err)) => Err(StorageError::Backend(format!(
            "keyring not initialized: {err}"
        ))),
        Err(err) => Err(StorageError::Backend(format!(
            "failed to read Marmot database key: {err}"
        ))),
    }
}

fn save_db_key(service_id: &str, db_key_id: &str, key: &SqlCipherKey) -> StorageResult<()> {
    let entry = keyring_entry(service_id, db_key_id)?;
    entry
        .set_secret(encode_keyring_payload(key).as_bytes())
        .map_err(map_keyring_write_error)
}

fn keyring_entry(service_id: &str, db_key_id: &str) -> StorageResult<Entry> {
    Entry::new(service_id, db_key_id).map_err(|err| {
        StorageError::Backend(format!(
            "failed to create Marmot database keyring entry: {err}"
        ))
    })
}

fn encode_keyring_payload(key: &SqlCipherKey) -> String {
    format!("{KEYRING_PAYLOAD_PREFIX}{}", hex::encode(key.0.as_ref()))
}

fn decode_keyring_payload(secret: &[u8]) -> StorageResult<SqlCipherKey> {
    let secret = std::str::from_utf8(secret).map_err(|_| {
        StorageError::Backend("stored Marmot database key is not valid UTF-8".to_string())
    })?;
    let encoded_key = secret.strip_prefix(KEYRING_PAYLOAD_PREFIX).ok_or_else(|| {
        StorageError::Backend("stored Marmot database key has an unsupported format".to_string())
    })?;
    let key = hex::decode(encoded_key).map_err(|_| {
        StorageError::Backend("stored Marmot database key is not valid hex".to_string())
    })?;
    SqlCipherKey::from_slice(&key)
}

fn map_keyring_write_error(err: KeyringError) -> StorageError {
    match err {
        KeyringError::NoStorageAccess(err) => {
            StorageError::Backend(format!("keyring not initialized: {err}"))
        }
        err => StorageError::Backend(format!("failed to store Marmot database key: {err}")),
    }
}
