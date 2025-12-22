//! NIP-55 Android Signer Application Support
//!
//! This module implements NIP-55 (Android Signer Application) to allow signing events
//! using external signer apps like Amber on Android. The Flutter layer handles the
//! actual communication with the signer app via Android Intents/Content Resolvers.
//!
//! See: https://github.com/nostr-protocol/nips/blob/master/55.md

use std::sync::Arc;

use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tokio::sync::RwLock;

/// Trait for Flutter callbacks to communicate with NIP-55 signer apps
///
/// The Flutter layer implements this trait to handle communication with external
/// signer applications (like Amber) via Android Intents and Content Resolvers.
pub trait Nip55FlutterCallback: Send + Sync + std::fmt::Debug {
    /// Call the Flutter layer to execute a NIP-55 signer method
    ///
    /// The Flutter layer will handle the actual Android Intent/Content Resolver
    /// communication with the signer app and return the result.
    ///
    /// # Arguments
    ///
    /// * `method` - The NIP-55 method to call (e.g., "get_public_key", "sign_event")
    /// * `params` - JSON-encoded parameters for the method
    ///
    /// # Returns
    ///
    /// JSON-encoded result from the signer app, or an error string
    fn call_nip55_method(&self, method: &str, params: &str) -> std::result::Result<String, String>;
}

/// NIP-55 signer that uses Flutter callbacks to communicate with external signer apps
#[derive(Clone, Debug)]
pub struct Nip55Signer {
    callback: Arc<dyn Nip55FlutterCallback>,
    current_user: Arc<RwLock<Option<PublicKey>>>,
    cached_pubkey: Arc<RwLock<Option<PublicKey>>>,
}

#[derive(Error, Debug)]
pub enum Nip55SignerError {
    #[error("Flutter callback error: {0}")]
    FlutterCallback(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    #[error("Current user not set")]
    CurrentUserNotSet,

    #[error("Signer rejected the request")]
    Rejected,
}

/// NIP-55 method request
#[derive(Debug, Serialize, Deserialize)]
struct Nip55Request {
    method: String,
    params: Vec<serde_json::Value>,
}

/// NIP-55 method response
#[derive(Debug, Serialize, Deserialize)]
struct Nip55Response {
    result: Option<String>,
    rejected: Option<bool>,
    error: Option<String>,
}

impl Nip55Signer {
    /// Create a new NIP-55 signer with a Flutter callback
    pub fn new(callback: Arc<dyn Nip55FlutterCallback>) -> Self {
        Self {
            callback,
            current_user: Arc::new(RwLock::new(None)),
            cached_pubkey: Arc::new(RwLock::new(None)),
        }
    }

    /// Set the current user's public key
    ///
    /// This is required for methods that need to know which account is making the request.
    pub async fn set_current_user(&self, pubkey: PublicKey) {
        let mut user = self.current_user.write().await;
        *user = Some(pubkey);
    }

    /// Clear the cached public key
    pub async fn clear_cache(&self) {
        let mut cached = self.cached_pubkey.write().await;
        *cached = None;
    }

    /// Set the cached public key directly (useful when we already have it)
    pub async fn set_cached_pubkey(&self, pubkey: PublicKey) {
        let mut cached = self.cached_pubkey.write().await;
        *cached = Some(pubkey);
    }

    /// Call a NIP-55 method via the Flutter callback
    async fn call_method(
        &self,
        method: &str,
        params: Vec<serde_json::Value>,
    ) -> std::result::Result<String, Nip55SignerError> {
        let request = Nip55Request {
            method: method.to_string(),
            params,
        };

        let params_json =
            serde_json::to_string(&request.params).map_err(Nip55SignerError::Serialization)?;

        let response_json = self
            .callback
            .call_nip55_method(method, &params_json)
            .map_err(Nip55SignerError::FlutterCallback)?;

        let response: Nip55Response =
            serde_json::from_str(&response_json).map_err(Nip55SignerError::Serialization)?;

        // Check if the request was rejected
        if response.rejected.unwrap_or(false) {
            return Err(Nip55SignerError::Rejected);
        }

        // Check for errors
        if let Some(error) = response.error {
            return Err(Nip55SignerError::FlutterCallback(error));
        }

        response
            .result
            .ok_or_else(|| Nip55SignerError::InvalidResponse("No result in response".to_string()))
    }

    /// Get the current user's public key
    async fn get_current_user(&self) -> std::result::Result<PublicKey, Nip55SignerError> {
        let user = self.current_user.read().await;
        user.ok_or(Nip55SignerError::CurrentUserNotSet)
    }

    async fn get_public_key_async(&self) -> std::result::Result<PublicKey, SignerError> {
        // Check cache first
        {
            let cached = self.cached_pubkey.read().await;
            if let Some(pubkey) = *cached {
                return Ok(pubkey);
            }
        }

        // Call Flutter to get public key from signer app
        let result = self
            .call_method("get_public_key", vec![])
            .await
            .map_err(SignerError::backend)?;

        let pubkey = PublicKey::from_hex(&result).map_err(SignerError::backend)?;

        // Cache the pubkey
        {
            let mut cached = self.cached_pubkey.write().await;
            *cached = Some(pubkey);
        }

        Ok(pubkey)
    }
}

impl NostrSigner for Nip55Signer {
    fn get_public_key(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<PublicKey, SignerError>>
                + Send
                + '_,
        >,
    > {
        let this = self.clone();
        Box::pin(async move { this.get_public_key_async().await })
    }

    fn sign_event(
        &self,
        unsigned_event: UnsignedEvent,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<Event, SignerError>> + Send + '_>,
    > {
        let this = self.clone();
        Box::pin(async move {
            let current_user = this
                .get_current_user()
                .await
                .map_err(SignerError::backend)?;

            // Serialize the unsigned event
            let event_json =
                serde_json::to_string(&unsigned_event).map_err(SignerError::backend)?;

            // Call Flutter to sign the event
            let params = vec![json!(event_json), json!(current_user.to_hex())];

            let result = this
                .call_method("sign_event", params)
                .await
                .map_err(SignerError::backend)?;

            // Parse the signed event
            let event: Event = serde_json::from_str(&result).map_err(SignerError::backend)?;

            Ok(event)
        })
    }

    fn nip44_encrypt(
        &self,
        _receiver: &PublicKey,
        _content: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<String, SignerError>> + Send + '_>,
    > {
        Box::pin(async move {
            todo!("NIP-44 encryption is intentionally unimplemented - not used in whitenoise");
        })
    }

    fn nip44_decrypt(
        &self,
        _sender: &PublicKey,
        _content: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<String, SignerError>> + Send + '_>,
    > {
        Box::pin(async move {
            todo!("NIP-44 decryption is intentionally unimplemented - not used in whitenoise");
        })
    }

    fn nip04_encrypt(
        &self,
        _receiver: &PublicKey,
        _content: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<String, SignerError>> + Send + '_>,
    > {
        Box::pin(async move {
            todo!("NIP-04 encryption is insecure and not supported.");
        })
    }

    fn nip04_decrypt(
        &self,
        _sender: &PublicKey,
        _content: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<String, SignerError>> + Send + '_>,
    > {
        Box::pin(async move {
            todo!("NIP-04 decryption is insecure and not supported.");
        })
    }

    fn backend(&self) -> SignerBackend<'_> {
        SignerBackend::Custom("nip55".into())
    }
}

/// A wrapper enum for different signer types used in Whitenoise
#[derive(Clone, Debug)]
pub enum WhitenoiseSigner {
    Keys(Keys),
    Nip55(Nip55Signer),
}

impl NostrSigner for WhitenoiseSigner {
    fn get_public_key(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<PublicKey, SignerError>>
                + Send
                + '_,
        >,
    > {
        let this = self.clone();
        Box::pin(async move {
            match this {
                Self::Keys(k) => k.get_public_key().await,
                Self::Nip55(s) => s.get_public_key().await,
            }
        })
    }

    fn sign_event(
        &self,
        unsigned_event: UnsignedEvent,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<Event, SignerError>> + Send + '_>,
    > {
        let this = self.clone();
        Box::pin(async move {
            match this {
                Self::Keys(k) => k.sign_event(unsigned_event).await,
                Self::Nip55(s) => s.sign_event(unsigned_event).await,
            }
        })
    }

    fn nip44_encrypt(
        &self,
        receiver: &PublicKey,
        content: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<String, SignerError>> + Send + '_>,
    > {
        let this = self.clone();
        let receiver = *receiver;
        let content = content.to_string();
        Box::pin(async move {
            match this {
                Self::Keys(k) => k.nip44_encrypt(&receiver, &content).await,
                Self::Nip55(s) => s.nip44_encrypt(&receiver, &content).await,
            }
        })
    }

    fn nip44_decrypt(
        &self,
        sender: &PublicKey,
        content: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<String, SignerError>> + Send + '_>,
    > {
        let this = self.clone();
        let sender = *sender;
        let content = content.to_string();
        Box::pin(async move {
            match this {
                Self::Keys(k) => k.nip44_decrypt(&sender, &content).await,
                Self::Nip55(s) => s.nip44_decrypt(&sender, &content).await,
            }
        })
    }

    fn nip04_encrypt(
        &self,
        receiver: &PublicKey,
        content: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<String, SignerError>> + Send + '_>,
    > {
        let this = self.clone();
        let receiver = *receiver;
        let content = content.to_string();
        Box::pin(async move {
            match this {
                Self::Keys(k) => k.nip04_encrypt(&receiver, &content).await,
                Self::Nip55(s) => s.nip04_encrypt(&receiver, &content).await,
            }
        })
    }

    fn nip04_decrypt(
        &self,
        sender: &PublicKey,
        content: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<String, SignerError>> + Send + '_>,
    > {
        let this = self.clone();
        let sender = *sender;
        let content = content.to_string();
        Box::pin(async move {
            match this {
                Self::Keys(k) => k.nip04_decrypt(&sender, &content).await,
                Self::Nip55(s) => s.nip04_decrypt(&sender, &content).await,
            }
        })
    }

    fn backend(&self) -> SignerBackend<'_> {
        match self {
            Self::Keys(k) => k.backend(),
            Self::Nip55(s) => s.backend(),
        }
    }
}
