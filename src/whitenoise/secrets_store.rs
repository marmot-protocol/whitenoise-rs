use keyring_core::{Entry, Error as KeyringError};
use nostr_sdk::{Keys, PublicKey, SecretKey};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SecretsStoreError {
    #[error("Keyring error: {0}")]
    KeyringError(String),

    #[error("Keyring not initialized: {0}")]
    KeyringNotInitialized(String),

    #[error("Keyring storage unavailable: {0}")]
    KeyringUnavailable(String),

    #[error("Key error: {0}")]
    KeyError(#[from] nostr_sdk::key::Error),

    #[error("Key not found")]
    KeyNotFound,

    #[error("Malformed NIP-46 credentials in keychain: {0}")]
    MalformedNip46Blob(String),
}

/// Shape of the JSON blob stored in the keychain for NIP-46 accounts.
#[derive(serde::Serialize, serde::Deserialize)]
struct Nip46Payload {
    app_secret_key: String,
    bunker_uri: String,
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

    /// Saves the NIP-46 app secret key and bunker URI for an account.
    ///
    /// Both values get packed into a single JSON blob and written to the
    /// platform keychain. We need them later to rebuild the `NostrConnect`
    /// signer when the app restarts — without them the user would have to
    /// re-authorize every launch.
    pub fn store_nip46_credentials(
        &self,
        account_pubkey: &PublicKey,
        app_secret_key: &SecretKey,
        bunker_uri: &str,
    ) -> Result<(), SecretsStoreError> {
        let entry_key = format!("nip46:{}", account_pubkey.to_hex());
        let entry = Entry::new(&self.service_name, &entry_key).map_err(map_keyring_error)?;

        let payload = Nip46Payload {
            app_secret_key: app_secret_key.to_secret_hex(),
            bunker_uri: bunker_uri.to_string(),
        };

        let json = serde_json::to_string(&payload)
            .map_err(|e| SecretsStoreError::MalformedNip46Blob(e.to_string()))?;

        entry.set_password(&json).map_err(map_keyring_error)?;
        Ok(())
    }

    /// Loads the NIP-46 app secret key and bunker URI for an account.
    ///
    /// Returns `(SecretKey, bunker_uri_string)`. If there aren't any NIP-46
    /// credentials for this pubkey (the account is NIP-55 or uses local
    /// keys), you'll get `SecretsStoreError::KeyNotFound`.
    pub fn get_nip46_credentials(
        &self,
        account_pubkey: &PublicKey,
    ) -> Result<(SecretKey, String), SecretsStoreError> {
        let entry_key = format!("nip46:{}", account_pubkey.to_hex());
        let entry = Entry::new(&self.service_name, &entry_key).map_err(map_keyring_error)?;

        match entry.get_password() {
            Ok(json_str) => {
                let payload: Nip46Payload = serde_json::from_str(&json_str)
                    .map_err(|e| SecretsStoreError::MalformedNip46Blob(e.to_string()))?;

                let secret_key = SecretKey::parse(&payload.app_secret_key)?;
                Ok((secret_key, payload.bunker_uri))
            }
            Err(KeyringError::NoEntry) => Err(SecretsStoreError::KeyNotFound),
            Err(e) => Err(map_keyring_error(e)),
        }
    }

    /// Deletes the NIP-46 app key and bunker URI for an account.
    ///
    /// Called during logout. If there's nothing to delete (NIP-55 account,
    /// local keys, already cleaned up) it just returns `Ok(())`.
    pub fn delete_nip46_credentials(
        &self,
        account_pubkey: &PublicKey,
    ) -> Result<(), SecretsStoreError> {
        let entry_key = format!("nip46:{}", account_pubkey.to_hex());
        let entry = Entry::new(&self.service_name, &entry_key).map_err(map_keyring_error)?;

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
            SecretsStoreError::KeyringUnavailable(format_storage_access_error(err.as_ref()))
        }
        other => SecretsStoreError::KeyringError(other.to_string()),
    }
}

/// Produces a user-friendly message for `NoStorageAccess` errors, with
/// platform-specific remediation hints.
fn format_storage_access_error(inner: &dyn std::error::Error) -> String {
    #[cfg(target_os = "linux")]
    {
        format!(
            "Platform keyring is not available. On Linux, White Noise uses the kernel \
             keyutils subsystem (keyctl) to store secret keys. This error typically \
             occurs on headless systems, in SSH sessions, or in containers where no \
             session keyring is active. Try running `keyctl session` before starting \
             the daemon, or ensure your init system provides a session keyring. \
             (Original error: {inner})"
        )
    }

    #[cfg(not(target_os = "linux"))]
    {
        format!(
            "Platform keyring is not available: {inner}. \
             Ensure your system's credential storage service is running and accessible."
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::whitenoise::Whitenoise;

    use super::*;

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
    fn test_map_keyring_error_no_default_store() {
        let err = map_keyring_error(KeyringError::NoDefaultStore);
        assert!(
            matches!(err, SecretsStoreError::KeyringNotInitialized(_)),
            "Expected KeyringNotInitialized, got: {:?}",
            err
        );
    }

    #[test]
    fn test_map_keyring_error_no_storage_access() {
        let inner = std::io::Error::other("KeyRevoked");
        let err = map_keyring_error(KeyringError::NoStorageAccess(Box::new(inner)));
        assert!(
            matches!(err, SecretsStoreError::KeyringUnavailable(_)),
            "Expected KeyringUnavailable, got: {:?}",
            err
        );
        let msg = err.to_string();
        assert!(
            msg.contains("Platform keyring is not available"),
            "Expected actionable guidance, got: {msg}"
        );
        assert!(
            msg.contains("KeyRevoked"),
            "Expected original error in message, got: {msg}"
        );
    }

    #[test]
    fn test_map_keyring_error_other() {
        let err = map_keyring_error(KeyringError::NoEntry);
        assert!(
            matches!(err, SecretsStoreError::KeyringError(_)),
            "Expected KeyringError, got: {:?}",
            err
        );
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

    #[test]
    fn test_store_and_retrieve_nip46_credentials() {
        let secrets_store = create_test_secrets_store();
        let keys = Keys::generate();
        let account_pubkey = keys.public_key();
        let app_keys = Keys::generate();
        let bunker_uri = "bunker://79dff8f82963424e0bb02708a22e44b4980893e3a4be0fa3cb60a43b946764e3?relay=wss://relay.nsec.app";

        secrets_store
            .store_nip46_credentials(&account_pubkey, app_keys.secret_key(), bunker_uri)
            .unwrap();

        let (retrieved_secret, retrieved_uri) = secrets_store
            .get_nip46_credentials(&account_pubkey)
            .unwrap();

        assert_eq!(
            retrieved_secret.to_secret_hex(),
            app_keys.secret_key().to_secret_hex()
        );
        assert_eq!(retrieved_uri, bunker_uri);

        // Clean up
        secrets_store
            .delete_nip46_credentials(&account_pubkey)
            .unwrap();
    }

    #[test]
    fn test_get_nonexistent_nip46_credentials() {
        let secrets_store = create_test_secrets_store();
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let result = secrets_store.get_nip46_credentials(&pubkey);
        assert!(matches!(result, Err(SecretsStoreError::KeyNotFound)));
    }

    #[test]
    fn test_delete_nip46_credentials_idempotent() {
        let secrets_store = create_test_secrets_store();
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Deleting nonexistent credentials should succeed
        let result = secrets_store.delete_nip46_credentials(&pubkey);
        assert!(result.is_ok());
    }

    #[test]
    fn test_delete_nip46_credentials_removes_entry() {
        let secrets_store = create_test_secrets_store();
        let keys = Keys::generate();
        let account_pubkey = keys.public_key();
        let app_keys = Keys::generate();
        let bunker_uri = "bunker://79dff8f82963424e0bb02708a22e44b4980893e3a4be0fa3cb60a43b946764e3?relay=wss://relay.nsec.app";

        secrets_store
            .store_nip46_credentials(&account_pubkey, app_keys.secret_key(), bunker_uri)
            .unwrap();

        secrets_store
            .delete_nip46_credentials(&account_pubkey)
            .unwrap();

        let result = secrets_store.get_nip46_credentials(&account_pubkey);
        assert!(matches!(result, Err(SecretsStoreError::KeyNotFound)));
    }

    #[test]
    fn test_nip46_credentials_isolated_from_private_keys() {
        let secrets_store = create_test_secrets_store();
        let keys = Keys::generate();
        let pubkey = keys.public_key();
        let app_keys = Keys::generate();
        let bunker_uri = "bunker://79dff8f82963424e0bb02708a22e44b4980893e3a4be0fa3cb60a43b946764e3?relay=wss://relay.nsec.app";

        // Store both a private key and NIP-46 credentials for the same pubkey
        secrets_store.store_private_key(&keys).unwrap();
        secrets_store
            .store_nip46_credentials(&pubkey, app_keys.secret_key(), bunker_uri)
            .unwrap();

        // Deleting private key should not affect NIP-46 credentials
        secrets_store
            .remove_private_key_for_pubkey(&pubkey)
            .unwrap();
        let (retrieved_secret, _) = secrets_store.get_nip46_credentials(&pubkey).unwrap();
        assert_eq!(
            retrieved_secret.to_secret_hex(),
            app_keys.secret_key().to_secret_hex()
        );

        // Deleting NIP-46 credentials should not affect a re-stored private key
        secrets_store.store_private_key(&keys).unwrap();
        secrets_store.delete_nip46_credentials(&pubkey).unwrap();
        let retrieved_keys = secrets_store.get_nostr_keys_for_pubkey(&pubkey).unwrap();
        assert_eq!(retrieved_keys.public_key(), pubkey);
    }
}
