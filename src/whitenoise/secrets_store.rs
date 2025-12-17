use std::{
    fs,
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose};
use keyring::Entry;
use nostr_sdk::{Keys, PublicKey};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum SecretsStoreError {
    #[error("Failed to parse JSON: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("UUID error: {0}")]
    UuidError(#[from] uuid::Error),

    #[error("File error: {0}")]
    FileError(#[from] std::io::Error),

    #[error("Base64 error: {0}")]
    Base64Error(#[from] base64::DecodeError),

    #[error("UTF-8 error: {0}")]
    Utf8Error(#[from] std::string::FromUtf8Error),

    #[error("Keyring error: {0}")]
    KeyringError(#[from] keyring::Error),

    #[error("Key error: {0}")]
    KeyError(#[from] nostr_sdk::key::Error),

    #[error("Key not found")]
    KeyNotFound,
}

const SERVICE_NAME: &str = "whitenoise";

pub struct SecretsStore {
    data_dir: PathBuf,
}

impl SecretsStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
        }
    }

    fn get_device_key(&self) -> Vec<u8> {
        let uuid_file = self.data_dir.join("whitenoise_uuid");

        let uuid = if uuid_file.exists() {
            // Read existing UUID
            std::fs::read_to_string(&uuid_file)
                .map_err(SecretsStoreError::FileError)
                .and_then(|s| s.parse::<Uuid>().map_err(SecretsStoreError::UuidError))
        } else {
            // Generate new UUID
            let new_uuid = Uuid::new_v4();
            let _ = std::fs::create_dir_all(&self.data_dir).map_err(SecretsStoreError::FileError);
            let _ = std::fs::write(uuid_file, new_uuid.to_string())
                .map_err(SecretsStoreError::FileError);
            Ok(new_uuid)
        };

        uuid.expect("Couldn't unwrap UUID").as_bytes().to_vec()
    }

    fn get_file_path(&self) -> PathBuf {
        self.data_dir.join("whitenoise.json")
    }

    fn obfuscate(&self, data: &str) -> String {
        let xored: Vec<u8> = data
            .as_bytes()
            .iter()
            .zip(self.get_device_key().iter().cycle())
            .map(|(&x1, &x2)| x1 ^ x2)
            .collect();
        general_purpose::STANDARD_NO_PAD.encode(xored)
    }

    fn deobfuscate(&self, data: &str) -> Result<String, SecretsStoreError> {
        let decoded = general_purpose::STANDARD_NO_PAD
            .decode(data)
            .map_err(SecretsStoreError::Base64Error)?;
        let xored: Vec<u8> = decoded
            .iter()
            .zip(self.get_device_key().iter().cycle())
            .map(|(&x1, &x2)| x1 ^ x2)
            .collect();
        String::from_utf8(xored).map_err(SecretsStoreError::Utf8Error)
    }

    fn read_secrets_file(&self) -> Result<Value, SecretsStoreError> {
        let content = match fs::read_to_string(self.get_file_path()) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::from("{}"),
            Err(e) => return Err(e.into()),
        };
        Ok(serde_json::from_str(&content)?)
    }

    fn write_secrets_file(&self, secrets: &Value) -> Result<(), SecretsStoreError> {
        let content = serde_json::to_string_pretty(secrets)?;
        fs::write(self.get_file_path(), content)?;
        Ok(())
    }

    /// Stores the private key associated with the given Keys in the system's keyring.
    ///
    /// This function takes a reference to a `Keys` object and stores the private key
    /// in the system's keyring, using the public key as an identifier.
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
    /// * The Entry creation fails
    /// * Setting the password in the keyring fails
    /// * The secret key cannot be retrieved from the keypair
    pub fn store_private_key(&self, keys: &Keys) -> Result<(), SecretsStoreError> {
        if cfg!(target_os = "android") || cfg!(test) {
            let mut secrets = self.read_secrets_file().unwrap_or(json!({}));
            let obfuscated_key = self.obfuscate(keys.secret_key().to_secret_hex().as_str());
            secrets[keys.public_key().to_hex()] = json!(obfuscated_key);
            self.write_secrets_file(&secrets)?;
        } else {
            let entry = Entry::new(SERVICE_NAME, keys.public_key().to_hex().as_str())
                .map_err(SecretsStoreError::KeyringError)?;
            entry
                .set_password(keys.secret_key().to_secret_hex().as_str())
                .map_err(SecretsStoreError::KeyringError)?;
        }

        Ok(())
    }

    /// Retrieves the Nostr keys associated with a given public key from the system's keyring.
    ///
    /// This function looks up the private key stored in the system's keyring using the provided
    /// public key as an identifier, and then constructs a `Keys` object from the retrieved private key.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - A reference to the PublicKey to look up.
    ///
    /// # Returns
    ///
    /// * `Result<Keys>` - A Result containing the `Keys` object if successful, or an error if the operation fails.
    ///
    /// # Errors
    ///
    /// This function will return an error if:
    /// * The Entry creation fails
    /// * Retrieving the password from the keyring fails
    /// * Parsing the private key into a `Keys` object fails
    pub fn get_nostr_keys_for_pubkey(&self, pubkey: &PublicKey) -> Result<Keys, SecretsStoreError> {
        let hex_pubkey = pubkey.to_hex();
        if cfg!(target_os = "android") || cfg!(test) {
            let secrets = self.read_secrets_file()?;
            let obfuscated_key = secrets[&hex_pubkey.as_str()]
                .as_str()
                .ok_or(SecretsStoreError::KeyNotFound)?;
            let private_key = self.deobfuscate(obfuscated_key)?;
            Keys::parse(&private_key).map_err(SecretsStoreError::KeyError)
        } else {
            let entry = Entry::new(SERVICE_NAME, hex_pubkey.as_str())
                .map_err(SecretsStoreError::KeyringError)?;
            let private_key = entry
                .get_password()
                .map_err(SecretsStoreError::KeyringError)?;
            Keys::parse(&private_key).map_err(SecretsStoreError::KeyError)
        }
    }

    /// Removes the private key associated with a given public key from the system's keyring.
    ///
    /// This function attempts to delete the credential entry for the specified public key
    /// from the system's keyring. If the entry doesn't exist or the deletion fails, the
    /// function will still return Ok(()) to maintain idempotency.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - A reference to the PublicKey for which to remove the associated private key.
    ///
    /// # Returns
    ///
    /// * `Result<()>` - Ok(()) if the operation was successful or if the key didn't exist, or an error if the Entry creation fails.
    ///
    /// # Errors
    ///
    /// This function will return an error if:
    /// * The Entry creation fails
    pub fn remove_private_key_for_pubkey(
        &self,
        pubkey: &PublicKey,
    ) -> Result<(), SecretsStoreError> {
        let hex_pubkey = pubkey.to_hex();
        if cfg!(target_os = "android") || cfg!(test) {
            let mut secrets = self.read_secrets_file()?;
            secrets
                .as_object_mut()
                .map(|obj| obj.remove(hex_pubkey.as_str()));
            self.write_secrets_file(&secrets)?;
        } else {
            let entry = Entry::new(SERVICE_NAME, hex_pubkey.as_str());
            if let Ok(entry) = entry {
                let _ = entry.delete_credential();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_secrets_store() -> (SecretsStore, TempDir) {
        let data_temp = TempDir::new().expect("Failed to create temp directory");
        let secrets_store = SecretsStore::new(data_temp.path());
        (secrets_store, data_temp)
    }

    #[tokio::test]
    async fn test_store_and_retrieve_private_key() -> Result<(), SecretsStoreError> {
        let (secrets_store, _temp_dir) = create_test_secrets_store();
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
        let (secrets_store, _temp_dir) = create_test_secrets_store();
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
        let (secrets_store, _temp_dir) = create_test_secrets_store();
        let keys = Keys::generate();
        let pubkey = keys.public_key();
        let result = secrets_store.get_nostr_keys_for_pubkey(&pubkey);

        assert!(result.is_err());
    }

    #[test]
    fn test_secrets_store_creation() {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let secrets_store = SecretsStore::new(temp_dir.path());

        // Test that the file path is constructed correctly
        assert_eq!(
            secrets_store.get_file_path(),
            temp_dir.path().join("whitenoise.json")
        );
    }

    #[tokio::test]
    #[cfg(target_os = "android")]
    async fn test_android_store_and_retrieve_private_key() -> Result<(), SecretsStoreError> {
        let (secrets_store, _temp_dir) = create_test_secrets_store();
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Store the private key
        secrets_store.store_private_key(&keys)?;

        // Retrieve the keys
        let retrieved_keys = secrets_store.get_nostr_keys_for_pubkey(&pubkey)?;

        assert_eq!(keys.public_key(), retrieved_keys.public_key());
        assert_eq!(keys.secret_key(), retrieved_keys.secret_key());

        // Verify that the key is stored in the file
        let secrets = secrets_store.read_secrets_file()?;
        assert!(secrets.get(&pubkey.to_hex()).is_some());

        // Clean up
        secrets_store.remove_private_key_for_pubkey(&pubkey)?;

        // Verify that the key is removed from the file
        let secrets = secrets_store.read_secrets_file()?;
        assert!(secrets.get(&pubkey.to_hex()).is_none());

        Ok(())
    }
}
