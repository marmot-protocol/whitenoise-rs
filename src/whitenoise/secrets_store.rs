use keyring_core::{Entry, Error as KeyringError};
use nostr_sdk::{Keys, PublicKey};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SecretsStoreError {
    #[error("Keyring error: {0}")]
    KeyringError(String),

    #[error("Keyring not initialized: {0}")]
    KeyringNotInitialized(String),

    #[error("Key error: {0}")]
    KeyError(#[from] nostr_sdk::key::Error),

    #[error("Key not found")]
    KeyNotFound,
}

pub struct SecretsStore {
    service_name: String,
}

impl SecretsStore {
    pub fn new(service_name: &str) -> Self {
        Self {
            service_name: service_name.to_string(),
        }
    }

    /// Stores the private key associated with the given Keys in the keyring.
    ///
    /// This function takes a reference to a `Keys` object and stores the private key
    /// using the platform's native credential store (via `keyring-core`), with the
    /// public key as an identifier.
    ///
    /// # Arguments
    ///
    /// * `keys` - A reference to a `Keys` object containing the keypair to store.
    ///
    /// # Returns
    ///
    /// * `Result<()>` - Ok(()) if the operation was successful, or an error if it failed.
    ///
    /// # Errors
    ///
    /// This function will return an error if:
    /// * No keyring store has been initialized
    /// * The Entry creation fails
    /// * Setting the password in the keyring fails
    pub fn store_private_key(&self, keys: &Keys) -> Result<(), SecretsStoreError> {
        let entry = Entry::new(&self.service_name, keys.public_key().to_hex().as_str())
            .map_err(map_keyring_error)?;
        entry
            .set_password(keys.secret_key().to_secret_hex().as_str())
            .map_err(map_keyring_error)?;
        Ok(())
    }

    /// Retrieves the Nostr keys associated with a given public key from the keyring.
    ///
    /// This function looks up the private key stored in the platform's native credential
    /// store using the provided public key as an identifier, and then constructs a `Keys`
    /// object from the retrieved private key.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - A reference to the PublicKey to look up.
    ///
    /// # Returns
    ///
    /// * `Result<Keys>` - A Result containing the `Keys` object if successful, or an error
    ///   if the operation fails.
    ///
    /// # Errors
    ///
    /// This function will return an error if:
    /// * No keyring store has been initialized
    /// * The Entry creation fails
    /// * Retrieving the password from the keyring fails
    /// * Parsing the private key into a `Keys` object fails
    pub fn get_nostr_keys_for_pubkey(&self, pubkey: &PublicKey) -> Result<Keys, SecretsStoreError> {
        let hex_pubkey = pubkey.to_hex();
        let entry =
            Entry::new(&self.service_name, hex_pubkey.as_str()).map_err(map_keyring_error)?;

        match entry.get_password() {
            Ok(private_key) => Keys::parse(&private_key).map_err(SecretsStoreError::KeyError),
            Err(KeyringError::NoEntry) => Err(SecretsStoreError::KeyNotFound),
            Err(e) => Err(map_keyring_error(e)),
        }
    }

    /// Removes the private key associated with a given public key from the keyring.
    ///
    /// This function attempts to delete the credential entry for the specified public key.
    /// If the entry doesn't exist, the function will still return Ok(()) to maintain
    /// idempotency.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - A reference to the PublicKey for which to remove the associated private key.
    ///
    /// # Returns
    ///
    /// * `Result<()>` - Ok(()) if the operation was successful or if the key didn't exist.
    ///
    /// # Errors
    ///
    /// This function will return an error if:
    /// * No keyring store has been initialized
    /// * The Entry creation fails
    pub fn remove_private_key_for_pubkey(
        &self,
        pubkey: &PublicKey,
    ) -> Result<(), SecretsStoreError> {
        let hex_pubkey = pubkey.to_hex();
        let entry =
            Entry::new(&self.service_name, hex_pubkey.as_str()).map_err(map_keyring_error)?;

        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(KeyringError::NoEntry) => Ok(()),
            Err(e) => Err(map_keyring_error(e)),
        }
    }
}

/// Maps a `keyring_core::Error` to a `SecretsStoreError`, distinguishing
/// uninitialized store errors from general keyring failures.
fn map_keyring_error(e: KeyringError) -> SecretsStoreError {
    match e {
        KeyringError::NoDefaultStore => SecretsStoreError::KeyringNotInitialized(e.to_string()),
        KeyringError::NoStorageAccess(ref err) => {
            SecretsStoreError::KeyringNotInitialized(err.to_string())
        }
        other => SecretsStoreError::KeyringError(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::Whitenoise;

    fn create_test_secrets_store() -> SecretsStore {
        Whitenoise::initialize_mock_keyring_store();
        SecretsStore::new("com.whitenoise.test")
    }

    #[tokio::test]
    async fn test_store_and_retrieve_private_key() -> Result<(), SecretsStoreError> {
        let secrets_store = create_test_secrets_store();
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Store the private key
        secrets_store.store_private_key(&keys)?;

        // Retrieve the keys
        let retrieved_keys = secrets_store.get_nostr_keys_for_pubkey(&pubkey)?;

        assert_eq!(keys.public_key(), retrieved_keys.public_key());
        assert_eq!(keys.secret_key(), retrieved_keys.secret_key());

        // Clean up
        secrets_store.remove_private_key_for_pubkey(&pubkey)?;

        Ok(())
    }

    #[tokio::test]
    async fn test_remove_private_key() -> Result<(), SecretsStoreError> {
        let secrets_store = create_test_secrets_store();
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Store the private key
        secrets_store.store_private_key(&keys)?;

        // Remove the private key
        secrets_store.remove_private_key_for_pubkey(&pubkey)?;

        // Attempt to retrieve the removed key
        let result = secrets_store.get_nostr_keys_for_pubkey(&pubkey);

        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_get_nonexistent_key() {
        let secrets_store = create_test_secrets_store();
        let keys = Keys::generate();
        let pubkey = keys.public_key();
        let result = secrets_store.get_nostr_keys_for_pubkey(&pubkey);

        assert!(result.is_err());
        assert!(matches!(result, Err(SecretsStoreError::KeyNotFound)));
    }

    #[test]
    fn test_remove_nonexistent_key_succeeds() {
        let secrets_store = create_test_secrets_store();
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Removing a nonexistent key should succeed (idempotent)
        let result = secrets_store.remove_private_key_for_pubkey(&pubkey);
        assert!(result.is_ok());
    }

    #[test]
    fn test_store_multiple_keys() {
        let secrets_store = create_test_secrets_store();

        let keys1 = Keys::generate();
        let keys2 = Keys::generate();
        let keys3 = Keys::generate();

        // Store multiple keys
        secrets_store.store_private_key(&keys1).unwrap();
        secrets_store.store_private_key(&keys2).unwrap();
        secrets_store.store_private_key(&keys3).unwrap();

        // All keys should be retrievable
        let retrieved1 = secrets_store
            .get_nostr_keys_for_pubkey(&keys1.public_key())
            .unwrap();
        let retrieved2 = secrets_store
            .get_nostr_keys_for_pubkey(&keys2.public_key())
            .unwrap();
        let retrieved3 = secrets_store
            .get_nostr_keys_for_pubkey(&keys3.public_key())
            .unwrap();

        assert_eq!(retrieved1.public_key(), keys1.public_key());
        assert_eq!(retrieved2.public_key(), keys2.public_key());
        assert_eq!(retrieved3.public_key(), keys3.public_key());
    }

    #[test]
    fn test_overwrite_existing_key() {
        let secrets_store = create_test_secrets_store();

        let keys1 = Keys::generate();
        let pubkey = keys1.public_key();

        // Store the first key
        secrets_store.store_private_key(&keys1).unwrap();

        // Store the same key again (should overwrite without error)
        secrets_store.store_private_key(&keys1).unwrap();

        // Key should still be retrievable
        let retrieved = secrets_store.get_nostr_keys_for_pubkey(&pubkey).unwrap();
        assert_eq!(retrieved.public_key(), pubkey);
    }
}
