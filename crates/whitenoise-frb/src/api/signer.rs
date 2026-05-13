//! External signer support for NIP-55 (Amber) integration.
//!
//! This module provides a bridge between Rust's `NostrSigner` trait and
//! Flutter/Dart callbacks, allowing external signers like Amber to be used
//! for signing Nostr events.

use flutter_rust_bridge::DartFnFuture;
use flutter_rust_bridge::frb;
use nostr_sdk::prelude::*;
use std::borrow::Cow;
use std::fmt::Debug;
use std::sync::Arc;

use crate::api::accounts::LoginResult;
use crate::api::error::ApiError;
use crate::api::wn;

/// An external signer that delegates signing operations to Dart callbacks.
///
/// This struct implements the `NostrSigner` trait by calling back into Dart
/// for all cryptographic operations. This allows external signers like Amber
/// (NIP-55) to be used without the private key ever touching the Rust code.
#[derive(Clone)]
pub struct DartSigner {
    pubkey: PublicKey,
    sign_event: Arc<dyn Fn(String) -> DartFnFuture<String> + Send + Sync>,
    nip04_encrypt: Arc<dyn Fn(String, String) -> DartFnFuture<String> + Send + Sync>,
    nip04_decrypt: Arc<dyn Fn(String, String) -> DartFnFuture<String> + Send + Sync>,
    nip44_encrypt: Arc<dyn Fn(String, String) -> DartFnFuture<String> + Send + Sync>,
    nip44_decrypt: Arc<dyn Fn(String, String) -> DartFnFuture<String> + Send + Sync>,
}

impl Debug for DartSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DartSigner")
            .field("pubkey", &self.pubkey)
            .finish()
    }
}

impl DartSigner {
    /// Creates a new DartSigner with the given callbacks.
    pub fn new(
        pubkey: PublicKey,
        sign_event: impl Fn(String) -> DartFnFuture<String> + Send + Sync + 'static,
        nip04_encrypt: impl Fn(String, String) -> DartFnFuture<String> + Send + Sync + 'static,
        nip04_decrypt: impl Fn(String, String) -> DartFnFuture<String> + Send + Sync + 'static,
        nip44_encrypt: impl Fn(String, String) -> DartFnFuture<String> + Send + Sync + 'static,
        nip44_decrypt: impl Fn(String, String) -> DartFnFuture<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            pubkey,
            sign_event: Arc::new(sign_event),
            nip04_encrypt: Arc::new(nip04_encrypt),
            nip04_decrypt: Arc::new(nip04_decrypt),
            nip44_encrypt: Arc::new(nip44_encrypt),
            nip44_decrypt: Arc::new(nip44_decrypt),
        }
    }
}

impl NostrSigner for DartSigner {
    fn backend(&self) -> SignerBackend<'_> {
        SignerBackend::Custom(Cow::Borrowed("DartSigner (NIP-55/Amber)"))
    }

    fn get_public_key(&self) -> BoxedFuture<'_, Result<PublicKey, SignerError>> {
        Box::pin(async move { Ok(self.pubkey) })
    }

    fn sign_event(&self, unsigned: UnsignedEvent) -> BoxedFuture<'_, Result<Event, SignerError>> {
        let sign_fn = self.sign_event.clone();
        Box::pin(async move {
            // Serialize the unsigned event to JSON
            let unsigned_json = serde_json::to_string(&unsigned).map_err(SignerError::backend)?;

            // Call the Dart callback to sign
            let signed_json = sign_fn(unsigned_json).await;

            // Parse the signed event
            let event: Event = serde_json::from_str(&signed_json).map_err(SignerError::backend)?;

            Ok(event)
        })
    }

    fn nip04_encrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, SignerError>> {
        let encrypt_fn = self.nip04_encrypt.clone();
        let pubkey_hex = public_key.to_hex();
        let content_owned = content.to_owned();
        Box::pin(async move {
            let result = encrypt_fn(content_owned, pubkey_hex).await;
            Ok(result)
        })
    }

    fn nip04_decrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        encrypted_content: &'a str,
    ) -> BoxedFuture<'a, Result<String, SignerError>> {
        let decrypt_fn = self.nip04_decrypt.clone();
        let pubkey_hex = public_key.to_hex();
        let content_owned = encrypted_content.to_owned();
        Box::pin(async move {
            let result = decrypt_fn(content_owned, pubkey_hex).await;
            Ok(result)
        })
    }

    fn nip44_encrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, SignerError>> {
        let encrypt_fn = self.nip44_encrypt.clone();
        let pubkey_hex = public_key.to_hex();
        let content_owned = content.to_owned();
        Box::pin(async move {
            let result = encrypt_fn(content_owned, pubkey_hex).await;
            Ok(result)
        })
    }

    fn nip44_decrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        payload: &'a str,
    ) -> BoxedFuture<'a, Result<String, SignerError>> {
        let decrypt_fn = self.nip44_decrypt.clone();
        let pubkey_hex = public_key.to_hex();
        let payload_owned = payload.to_owned();
        Box::pin(async move {
            let result = decrypt_fn(payload_owned, pubkey_hex).await;
            Ok(result)
        })
    }
}

/// Register an external signer for an existing account.
///
/// This function is used to re-register an external signer after app restart.
/// Unlike the login functions, this does NOT perform any account setup or key
/// package publishing - it only registers the signer so that subsequent
/// signing operations will work.
///
/// Call this on app startup when restoring an external signer account session.
#[frb]
pub async fn register_external_signer(
    pubkey: String,
    sign_event: impl Fn(String) -> DartFnFuture<String> + Send + Sync + 'static,
    nip04_encrypt: impl Fn(String, String) -> DartFnFuture<String> + Send + Sync + 'static,
    nip04_decrypt: impl Fn(String, String) -> DartFnFuture<String> + Send + Sync + 'static,
    nip44_encrypt: impl Fn(String, String) -> DartFnFuture<String> + Send + Sync + 'static,
    nip44_decrypt: impl Fn(String, String) -> DartFnFuture<String> + Send + Sync + 'static,
) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;

    let signer = DartSigner::new(
        pubkey,
        sign_event,
        nip04_encrypt,
        nip04_decrypt,
        nip44_encrypt,
        nip44_decrypt,
    );

    whitenoise.register_external_signer(pubkey, signer).await?;
    Ok(())
}

// -----------------------------------------------------------------------
// Multi-step login API (external signer)
// -----------------------------------------------------------------------

/// Step 1 of the multi-step external signer login flow.
///
/// Creates an account for the given public key using the provided signer
/// callbacks, then attempts to discover existing relay lists from the network.
///
/// Returns `LoginStatus::Complete` on the happy path, or
/// `LoginStatus::NeedsRelayLists` if relay lists were not found.
#[frb]
pub async fn login_external_signer_start(
    pubkey: String,
    sign_event: impl Fn(String) -> DartFnFuture<String> + Send + Sync + 'static,
    nip04_encrypt: impl Fn(String, String) -> DartFnFuture<String> + Send + Sync + 'static,
    nip04_decrypt: impl Fn(String, String) -> DartFnFuture<String> + Send + Sync + 'static,
    nip44_encrypt: impl Fn(String, String) -> DartFnFuture<String> + Send + Sync + 'static,
    nip44_decrypt: impl Fn(String, String) -> DartFnFuture<String> + Send + Sync + 'static,
) -> Result<LoginResult, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;

    let signer = DartSigner::new(
        pubkey,
        sign_event,
        nip04_encrypt,
        nip04_decrypt,
        nip44_encrypt,
        nip44_decrypt,
    );

    let result = whitenoise
        .login_external_signer_start(pubkey, signer)
        .await?;
    Ok(result.into())
}

/// Step 2a for external signer: publish default relay lists and complete login.
///
/// Called after `login_external_signer_start` returned `NeedsRelayLists`.
#[frb]
pub async fn login_external_signer_publish_default_relays(
    pubkey: String,
) -> Result<LoginResult, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let result = whitenoise
        .login_external_signer_publish_default_relays(&pubkey)
        .await?;
    Ok(result.into())
}

/// Step 2b for external signer: search a user-provided relay for existing lists.
///
/// Called after `login_external_signer_start` returned `NeedsRelayLists`.
#[frb]
pub async fn login_external_signer_with_custom_relay(
    pubkey: String,
    relay_url: String,
) -> Result<LoginResult, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let relay_url = nostr_sdk::RelayUrl::parse(&relay_url)?;
    let result = whitenoise
        .login_external_signer_with_custom_relay(&pubkey, relay_url)
        .await?;
    Ok(result.into())
}
