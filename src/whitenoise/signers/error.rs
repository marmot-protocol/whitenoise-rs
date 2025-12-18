//! Signer error types for the whitenoise signer abstraction layer.

use nostr_sdk::PublicKey;
use thiserror::Error;

/// Errors that can occur during signer operations.
#[derive(Error, Debug)]
pub enum SignerError {
    /// Amber signer application is not installed on the device.
    #[error("Amber signer not installed. Please install Amber from F-Droid or Play Store.")]
    AmberNotInstalled,

    /// Amber denied the requested permission.
    #[error("Amber permission denied: {0}")]
    PermissionDenied(String),

    /// User explicitly rejected the signing request in Amber.
    #[error("User rejected signing request")]
    UserRejected,

    /// Signer operation timed out waiting for response.
    #[error("Signer timeout")]
    Timeout,

    /// Invalid or unexpected response from the signer.
    #[error("Invalid signer response: {0}")]
    InvalidResponse(String),

    /// JNI error when communicating with Android.
    #[error("JNI error: {0}")]
    JniError(String),

    /// The requested key was not found.
    #[error("Key not found for pubkey: {0}")]
    KeyNotFound(PublicKey),

    /// Android context has not been initialized.
    #[error("Android context not initialized. Call Whitenoise::init_android_context() first.")]
    AndroidContextNotInitialized,

    /// Required feature is not enabled.
    #[error("Feature not enabled: {0}")]
    FeatureNotEnabled(String),

    /// Error from the underlying nostr-sdk signer.
    #[error("Nostr signer error: {0}")]
    NostrSigner(#[from] nostr_sdk::signer::SignerError),

    /// Serialization/deserialization error.
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
}
