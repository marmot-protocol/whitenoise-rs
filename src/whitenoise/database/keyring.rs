use core::fmt;
use std::sync::{Mutex, OnceLock};

use base64ct::{Base64, Encoding};
use keyring_core::{Entry, Error as KeyringError};
use zeroize::Zeroizing;

use super::DatabaseError;

static KEY_GENERATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

const DB_KEY_KEYRING_PREFIX: &str = "whitenoise-sqlite-key-v1:";

#[derive(Clone)]
pub(crate) struct EncryptionConfig {
    key: Zeroizing<[u8; 32]>,
}

impl EncryptionConfig {
    pub(crate) fn new(key: [u8; 32]) -> Self {
        Self {
            key: Zeroizing::new(key),
        }
    }

    fn from_slice(key: &[u8]) -> Result<Self, DatabaseError> {
        let key: [u8; 32] = key.try_into().map_err(|_| {
            DatabaseError::EncryptionKey(format!(
                "Stored key has invalid length (expected 32 bytes, got {} bytes)",
                key.len()
            ))
        })?;
        Ok(Self::new(key))
    }

    fn generate() -> Result<Self, DatabaseError> {
        let mut key = [0_u8; 32];
        getrandom::fill(&mut key)
            .map_err(|error| DatabaseError::EncryptionKey(error.to_string()))?;
        Ok(Self::new(key))
    }

    pub(crate) fn key(&self) -> &[u8; 32] {
        &self.key
    }
}

impl fmt::Debug for EncryptionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptionConfig")
            .field("key", &"[REDACTED]")
            .finish()
    }
}

pub(crate) fn get_or_create_db_key(
    service_id: &str,
    db_key_id: &str,
) -> Result<EncryptionConfig, DatabaseError> {
    if let Some(config) = get_db_key(service_id, db_key_id)? {
        return Ok(config);
    }

    let lock = KEY_GENERATION_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().map_err(|error| {
        DatabaseError::EncryptionKey(format!("Failed to acquire key generation lock: {error}"))
    })?;

    if let Some(config) = get_db_key(service_id, db_key_id)? {
        return Ok(config);
    }

    tracing::info!(
        service_id,
        db_key_id,
        "Generating new WhiteNoise database encryption key"
    );

    let config = EncryptionConfig::generate()?;
    save_db_key(service_id, db_key_id, &config)?;
    Ok(config)
}

pub(crate) fn create_fresh_db_key(
    service_id: &str,
    db_key_id: &str,
) -> Result<EncryptionConfig, DatabaseError> {
    let lock = KEY_GENERATION_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().map_err(|error| {
        DatabaseError::EncryptionKey(format!("Failed to acquire key generation lock: {error}"))
    })?;

    delete_db_key(service_id, db_key_id)?;

    tracing::info!(
        service_id,
        db_key_id,
        "Generating fresh WhiteNoise database encryption key"
    );

    let config = EncryptionConfig::generate()?;
    save_db_key(service_id, db_key_id, &config)?;
    Ok(config)
}

pub(crate) fn get_db_key(
    service_id: &str,
    db_key_id: &str,
) -> Result<Option<EncryptionConfig>, DatabaseError> {
    let entry = keyring_entry(service_id, db_key_id)?;
    match entry.get_secret() {
        Ok(secret) => Ok(Some(decode_db_key_from_keyring(&secret)?)),
        Err(KeyringError::NoEntry) => Ok(None),
        Err(error) => Err(map_keyring_read_error(error)),
    }
}

pub(crate) fn delete_db_key(service_id: &str, db_key_id: &str) -> Result<(), DatabaseError> {
    let entry = keyring_entry(service_id, db_key_id)?;
    match entry.delete_credential() {
        Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
        Err(error) => Err(map_keyring_delete_error(error)),
    }
}

fn save_db_key(
    service_id: &str,
    db_key_id: &str,
    config: &EncryptionConfig,
) -> Result<(), DatabaseError> {
    let entry = keyring_entry(service_id, db_key_id)?;
    entry
        .set_secret(encode_db_key_for_keyring(config).as_bytes())
        .map_err(map_keyring_write_error)
}

fn keyring_entry(service_id: &str, db_key_id: &str) -> Result<Entry, DatabaseError> {
    Entry::new(service_id, db_key_id).map_err(|error| {
        DatabaseError::EncryptionKey(format!(
            "Failed to create keyring entry for service='{service_id}', key='{db_key_id}': {error}"
        ))
    })
}

fn encode_db_key_for_keyring(config: &EncryptionConfig) -> String {
    format!(
        "{DB_KEY_KEYRING_PREFIX}{}",
        Base64::encode_string(config.key())
    )
}

fn decode_db_key_from_keyring(secret: &[u8]) -> Result<EncryptionConfig, DatabaseError> {
    let secret_text = std::str::from_utf8(secret).map_err(|_| {
        DatabaseError::EncryptionKey("Stored key has invalid encoded payload".to_string())
    })?;
    let encoded_key = secret_text
        .strip_prefix(DB_KEY_KEYRING_PREFIX)
        .ok_or_else(|| {
            DatabaseError::EncryptionKey("Stored key has invalid encoded payload".to_string())
        })?;
    let key = Base64::decode_vec(encoded_key).map_err(|_| {
        DatabaseError::EncryptionKey("Stored key has invalid encoded payload".to_string())
    })?;
    EncryptionConfig::from_slice(&key)
}

fn map_keyring_read_error(error: KeyringError) -> DatabaseError {
    match error {
        KeyringError::NoStorageAccess(error) => DatabaseError::EncryptionKey(format!(
            "Keyring storage unavailable while reading database key: {error}"
        )),
        error => DatabaseError::EncryptionKey(format!(
            "Failed to retrieve encryption key from keyring: {error}"
        )),
    }
}

fn map_keyring_write_error(error: KeyringError) -> DatabaseError {
    match error {
        KeyringError::NoStorageAccess(error) => DatabaseError::EncryptionKey(format!(
            "Keyring storage unavailable while storing database key: {error}"
        )),
        error => DatabaseError::EncryptionKey(format!(
            "Failed to store encryption key in keyring: {error}"
        )),
    }
}

fn map_keyring_delete_error(error: KeyringError) -> DatabaseError {
    match error {
        KeyringError::NoStorageAccess(error) => DatabaseError::EncryptionKey(format!(
            "Keyring storage unavailable while deleting database key: {error}"
        )),
        error => DatabaseError::EncryptionKey(format!(
            "Failed to delete encryption key from keyring: {error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use keyring_core::Entry;

    use super::*;
    use crate::whitenoise::Whitenoise;

    fn unique_service_id() -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("com.whitenoise.database-keyring-test.{id}")
    }

    #[test]
    fn get_or_create_db_key_persists_encoded_payload() {
        Whitenoise::initialize_mock_keyring_store();
        let service_id = unique_service_id();
        let key_id = "test.key";

        let created =
            get_or_create_db_key(&service_id, key_id).expect("create database encryption key");
        let loaded = get_db_key(&service_id, key_id)
            .expect("load database encryption key")
            .expect("database encryption key should exist");

        assert_eq!(created.key(), loaded.key());

        let entry = Entry::new(&service_id, key_id).expect("keyring entry");
        let payload = entry.get_secret().expect("stored payload");
        let payload = std::str::from_utf8(&payload).expect("payload should be utf-8");
        assert!(payload.starts_with(DB_KEY_KEYRING_PREFIX));
        assert!(payload.starts_with("whitenoise-sqlite-key-v1:"));
    }

    #[test]
    fn get_db_key_rejects_raw_payloads() {
        Whitenoise::initialize_mock_keyring_store();
        let service_id = unique_service_id();
        let key_id = "test.raw-key";
        let raw_key = [7_u8; 32];
        let entry = Entry::new(&service_id, key_id).expect("keyring entry");
        entry.set_secret(&raw_key).expect("store raw payload");

        let result = get_db_key(&service_id, key_id);
        assert!(matches!(result, Err(DatabaseError::EncryptionKey(_))));
    }

    #[test]
    fn encryption_config_debug_redacts_key_material() {
        let config = EncryptionConfig::new([7_u8; 32]);

        assert_eq!(
            format!("{config:?}"),
            "EncryptionConfig { key: \"[REDACTED]\" }"
        );
    }
}
