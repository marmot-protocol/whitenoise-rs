//! Ephemeral in-memory signer for testing purposes.
//!
//! This signer holds keys in memory only and does not persist them.
//! Useful for unit tests, integration tests, and temporary accounts.

use nostr_sdk::prelude::*;

/// An in-memory signer that does not persist keys.
///
/// This signer is useful for testing scenarios where you need signing
/// capabilities but don't want keys persisted to disk or sent to external
/// signers.
///
/// # Example
///
/// ```ignore
/// let signer = EphemeralSigner::generate();
/// let pubkey = signer.public_key();
/// ```
#[derive(Debug)]
pub struct EphemeralSigner {
    keys: Keys,
}

impl EphemeralSigner {
    /// Generate a new ephemeral signer with random keys.
    pub fn generate() -> Self {
        Self {
            keys: Keys::generate(),
        }
    }

    /// Create an ephemeral signer from existing keys.
    ///
    /// Note: The keys will only exist in memory for the lifetime of this signer.
    pub fn from_keys(keys: Keys) -> Self {
        Self { keys }
    }

    /// Get the public key for this signer.
    pub fn public_key(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// Get a reference to the underlying keys.
    ///
    /// This is primarily useful for testing scenarios.
    pub fn keys(&self) -> &Keys {
        &self.keys
    }
}

impl NostrSigner for EphemeralSigner {
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
    async fn test_ephemeral_signer_generate() {
        let signer = EphemeralSigner::generate();
        let pubkey = signer.get_public_key().await.unwrap();
        assert_eq!(pubkey, signer.public_key());
    }

    #[tokio::test]
    async fn test_ephemeral_signer_from_keys() {
        let keys = Keys::generate();
        let expected_pubkey = keys.public_key();
        let signer = EphemeralSigner::from_keys(keys);
        assert_eq!(signer.public_key(), expected_pubkey);
    }

    #[tokio::test]
    async fn test_ephemeral_signer_sign_event() {
        let signer = EphemeralSigner::generate();
        let pubkey = signer.public_key();

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

    #[tokio::test]
    async fn test_ephemeral_signer_nip44_roundtrip() {
        let signer1 = EphemeralSigner::generate();
        let signer2 = EphemeralSigner::generate();

        let plaintext = "secret message";
        let encrypted = signer1
            .nip44_encrypt(&signer2.public_key(), plaintext)
            .await
            .unwrap();

        let decrypted = signer2
            .nip44_decrypt(&signer1.public_key(), &encrypted)
            .await
            .unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[tokio::test]
    async fn test_ephemeral_signer_nip04_roundtrip() {
        let signer1 = EphemeralSigner::generate();
        let signer2 = EphemeralSigner::generate();

        let plaintext = "secret message";
        let encrypted = signer1
            .nip04_encrypt(&signer2.public_key(), plaintext)
            .await
            .unwrap();

        let decrypted = signer2
            .nip04_decrypt(&signer1.public_key(), &encrypted)
            .await
            .unwrap();

        assert_eq!(decrypted, plaintext);
    }
}
