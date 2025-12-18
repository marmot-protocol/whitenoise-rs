//! Local signer using keys stored in the secrets store.
//!
//! **WARNING**: This signer stores private keys locally, which is inherently
//! less secure than using an external signer like Amber. This module is only
//! available when the `insecure-local-signer` feature is enabled.
//!
//! On Android, use `AmberSigner` instead for better security.

use nostr_sdk::prelude::*;

use super::error::SignerError;
use crate::whitenoise::secrets_store::SecretsStore;

/// A signer that uses locally stored keys via SecretsStore.
///
/// **WARNING**: This stores private keys locally which is inherently less secure
/// than using an external signer like Amber. Only use this for development,
/// testing, or when external signers are not available.
#[derive(Debug)]
pub struct LocalSigner {
    keys: Keys,
}

impl LocalSigner {
    /// Create a new LocalSigner by retrieving keys from the secrets store.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The public key to retrieve
    /// * `secrets_store` - The secrets store containing the private key
    ///
    /// # Errors
    ///
    /// Returns `SignerError::KeyNotFound` if no key is found for the given pubkey.
    pub fn from_secrets_store(
        pubkey: &PublicKey,
        secrets_store: &SecretsStore,
    ) -> Result<Self, SignerError> {
        let keys = secrets_store
            .get_nostr_keys_for_pubkey(pubkey)
            .map_err(|_| SignerError::KeyNotFound(*pubkey))?;
        Ok(Self { keys })
    }

    /// Create a new LocalSigner from existing keys.
    ///
    /// Note: This does not persist the keys. Use `SecretsStore::store_private_key`
    /// to persist them.
    pub fn from_keys(keys: Keys) -> Self {
        Self { keys }
    }

    /// Get the public key for this signer.
    pub fn public_key(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// Get a reference to the underlying keys.
    pub fn keys(&self) -> &Keys {
        &self.keys
    }
}

impl NostrSigner for LocalSigner {
    fn backend(&self) -> SignerBackend<'_> {
        SignerBackend::Keys
    }

    fn get_public_key(&self) -> BoxedFuture<'_, Result<PublicKey, nostr_sdk::signer::SignerError>> {
        self.keys.get_public_key()
    }

    fn sign_event(
        &self,
        unsigned: UnsignedEvent,
    ) -> BoxedFuture<'_, Result<Event, nostr_sdk::signer::SignerError>> {
        self.keys.sign_event(unsigned)
    }

    fn nip44_encrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, nostr_sdk::signer::SignerError>> {
        self.keys.nip44_encrypt(public_key, content)
    }

    fn nip44_decrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, nostr_sdk::signer::SignerError>> {
        self.keys.nip44_decrypt(public_key, content)
    }

    fn nip04_encrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, nostr_sdk::signer::SignerError>> {
        self.keys.nip04_encrypt(public_key, content)
    }

    fn nip04_decrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, nostr_sdk::signer::SignerError>> {
        self.keys.nip04_decrypt(public_key, content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_local_signer_from_keys() {
        let keys = Keys::generate();
        let expected_pubkey = keys.public_key();
        let signer = LocalSigner::from_keys(keys);
        assert_eq!(signer.public_key(), expected_pubkey);
    }

    #[tokio::test]
    async fn test_local_signer_sign_event() {
        let keys = Keys::generate();
        let pubkey = keys.public_key();
        let signer = LocalSigner::from_keys(keys);

        let unsigned = UnsignedEvent::new(
            pubkey,
            Timestamp::now(),
            Kind::TextNote,
            vec![],
            "test message".to_string(),
        );

        let signed = signer.sign_event(unsigned).await.unwrap();
        assert_eq!(signed.pubkey, pubkey);
        assert!(signed.verify().is_ok());
    }
}
