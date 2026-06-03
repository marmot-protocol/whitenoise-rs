use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use cgka_traits::storage::{StorageError, StorageResult};
use zeroize::Zeroizing;

use super::WhitenoiseMarmotStorage;
use super::keyring::{delete_db_key, get_db_key, get_or_create_db_key};
use super::migrations::run_migrations;
use super::openmls_storage::WhitenoiseOpenMlsStorage;
use super::result_ext::RusqliteResultExt;

pub(crate) struct SqlCipherKey(pub(super) Zeroizing<[u8; 32]>);

impl SqlCipherKey {
    pub(super) fn generate() -> StorageResult<Self> {
        let mut key = [0_u8; 32];
        getrandom::fill(&mut key)
            .map_err(|err| StorageError::Backend(format!("key generation failed: {err}")))?;
        Ok(Self(Zeroizing::new(key)))
    }

    pub(super) fn from_slice(key: &[u8]) -> StorageResult<Self> {
        let key: [u8; 32] = key.try_into().map_err(|_| {
            StorageError::Backend("stored SQLCipher key has invalid length".to_string())
        })?;
        Ok(Self(Zeroizing::new(key)))
    }

    fn sqlcipher_pragma_value(&self) -> String {
        format!("x'{}'", hex::encode(self.0.as_ref()))
    }
}

impl fmt::Debug for SqlCipherKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SqlCipherKey").field(&"<redacted>").finish()
    }
}

impl WhitenoiseMarmotStorage {
    pub(crate) fn open_for_account(
        path: impl AsRef<Path>,
        keyring_service_id: &str,
        db_key_id: &str,
    ) -> StorageResult<Self> {
        let key = get_or_create_db_key(keyring_service_id, db_key_id)?;
        Self::open_encrypted(path, &key)
    }

    pub(crate) fn open_existing_for_account(
        path: impl AsRef<Path>,
        keyring_service_id: &str,
        db_key_id: &str,
    ) -> StorageResult<Option<Self>> {
        let path = path.as_ref();
        if !path.try_exists().map_err(|err| {
            StorageError::Backend(format!(
                "failed to inspect Marmot database path {}: {err}",
                path.display()
            ))
        })? {
            return Ok(None);
        }

        let Some(key) = get_db_key(keyring_service_id, db_key_id)? else {
            return Err(StorageError::Backend(format!(
                "existing Marmot database {} is missing keyring key {db_key_id}",
                path.display()
            )));
        };

        Self::open_encrypted(path, &key).map(Some)
    }

    pub(crate) fn delete_account_key(
        keyring_service_id: &str,
        db_key_id: &str,
    ) -> StorageResult<()> {
        delete_db_key(keyring_service_id, db_key_id)
    }

    pub(crate) fn open_encrypted(
        path: impl AsRef<Path>,
        key: &SqlCipherKey,
    ) -> StorageResult<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| StorageError::Backend(err.to_string()))?;
        }

        let mut connection = rusqlite::Connection::open(path).storage()?;
        apply_sqlcipher_key(&connection, key)?;
        run_migrations(&mut connection)?;

        Ok(Self::from_connection(connection))
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> StorageResult<Self> {
        let mut connection = rusqlite::Connection::open_in_memory().storage()?;
        run_migrations(&mut connection)?;

        Ok(Self::from_connection(connection))
    }

    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn from_connection(connection: rusqlite::Connection) -> Self {
        let connection = Arc::new(Mutex::new(connection));
        let openmls = WhitenoiseOpenMlsStorage::new(connection.clone());
        Self {
            connection,
            openmls,
        }
    }
}

fn apply_sqlcipher_key(connection: &rusqlite::Connection, key: &SqlCipherKey) -> StorageResult<()> {
    connection
        .pragma_update(None, "cipher_compatibility", 4_i64)
        .storage()?;
    connection
        .pragma_update(None, "key", key.sqlcipher_pragma_value())
        .storage()?;
    let _: i64 = connection
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
        .storage()?;
    Ok(())
}
