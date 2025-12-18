//! Signer abstraction layer for whitenoise.
//!
//! This module provides a unified interface for different signing backends:
//! - `AmberSigner`: NIP-55 Android signer (via Amber app)
//! - `LocalSigner`: Local key storage (feature-gated, insecure)
//! - `EphemeralSigner`: In-memory keys for testing
//!
//! # Feature Flags
//!
//! - `insecure-local-signer`: Enables local key storage. This is disabled by default
//!   for security reasons. On Android, use `AmberSigner` instead.
//!
//! # Android Usage
//!
//! On Android, call `Whitenoise::init_android_context()` at app startup before
//! using any signer operations.

pub mod ephemeral;
pub mod error;

#[cfg(target_os = "android")]
pub mod amber;

#[cfg(feature = "insecure-local-signer")]
pub mod local;

// Re-exports
pub use ephemeral::EphemeralSigner;
pub use error::SignerError;

#[cfg(target_os = "android")]
pub use amber::AmberSigner;

#[cfg(feature = "insecure-local-signer")]
pub use local::LocalSigner;

use serde::{Deserialize, Serialize};

/// The default Amber package name.
pub const AMBER_PACKAGE_NAME: &str = "com.greenart7c3.nostrsigner";

/// Represents the type of signer being used for an account.
///
/// This enum is serialized and stored alongside account data to remember
/// which signing method was used for each account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignerKind {
    /// NIP-55 Android signer (Amber).
    ///
    /// Only available on Android. The signer communicates with Amber via
    /// Android's ContentResolver using the NIP-55 protocol.
    #[cfg(target_os = "android")]
    Amber {
        /// Package name of the signer app (e.g., "com.greenart7c3.nostrsigner")
        package_name: String,
    },

    /// Local insecure signer - keys stored in SecretsStore.
    ///
    /// This stores private keys locally using the platform's keyring or
    /// an obfuscated file on Android. This is inherently less secure than
    /// using an external signer like Amber.
    ///
    /// Only available with the `insecure-local-signer` feature enabled.
    #[cfg(feature = "insecure-local-signer")]
    LocalInsecure,

    /// Ephemeral in-memory signer for testing.
    ///
    /// Keys exist only in memory and cannot be restored after the process
    /// exits. This is primarily useful for testing.
    Ephemeral,
}

impl SignerKind {
    /// Returns `true` if this signer kind requires external app interaction.
    pub fn is_external(&self) -> bool {
        #[cfg(target_os = "android")]
        if matches!(self, SignerKind::Amber { .. }) {
            return true;
        }
        false
    }

    /// Returns `true` if this signer stores keys locally.
    #[cfg(feature = "insecure-local-signer")]
    pub fn is_local(&self) -> bool {
        matches!(self, SignerKind::LocalInsecure)
    }

    /// Returns `true` if this signer is ephemeral (in-memory only).
    pub fn is_ephemeral(&self) -> bool {
        matches!(self, SignerKind::Ephemeral)
    }

    /// Create an Amber signer kind with the default package name.
    #[cfg(target_os = "android")]
    pub fn amber() -> Self {
        SignerKind::Amber {
            package_name: AMBER_PACKAGE_NAME.to_string(),
        }
    }

    /// Create an Amber signer kind with a custom package name.
    #[cfg(target_os = "android")]
    pub fn amber_with_package(package_name: String) -> Self {
        SignerKind::Amber { package_name }
    }

    /// Get the default signer kind for the current platform.
    ///
    /// - On Android: Returns `Amber` (recommended)
    /// - On other platforms with `insecure-local-signer`: Returns `LocalInsecure`
    /// - Otherwise: Returns `Ephemeral`
    pub fn default_for_platform() -> Self {
        #[cfg(target_os = "android")]
        {
            SignerKind::amber()
        }

        #[cfg(all(not(target_os = "android"), feature = "insecure-local-signer"))]
        {
            SignerKind::LocalInsecure
        }

        #[cfg(all(not(target_os = "android"), not(feature = "insecure-local-signer")))]
        {
            SignerKind::Ephemeral
        }
    }
}

// Android JNI context storage (static, set once at startup)
#[cfg(target_os = "android")]
pub(crate) mod android_context {
    use super::amber::AmberJniContext;
    use std::sync::OnceLock;

    static ANDROID_CONTEXT: OnceLock<AmberJniContext> = OnceLock::new();

    /// Set the Android JNI context. Can only be called once.
    pub fn set(ctx: AmberJniContext) -> Result<(), AmberJniContext> {
        ANDROID_CONTEXT.set(ctx)
    }

    /// Get a reference to the Android JNI context.
    pub fn get() -> Option<&'static AmberJniContext> {
        ANDROID_CONTEXT.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signer_kind_is_ephemeral() {
        let ephemeral = SignerKind::Ephemeral;
        assert!(ephemeral.is_ephemeral());
    }

    #[test]
    #[cfg(feature = "insecure-local-signer")]
    fn test_signer_kind_is_local() {
        let local = SignerKind::LocalInsecure;
        assert!(local.is_local());
        assert!(!local.is_ephemeral());
    }

    #[test]
    #[cfg(target_os = "android")]
    fn test_signer_kind_amber() {
        let amber = SignerKind::amber();
        assert!(amber.is_external());
        assert!(!amber.is_ephemeral());

        if let SignerKind::Amber { package_name } = amber {
            assert_eq!(package_name, AMBER_PACKAGE_NAME);
        } else {
            panic!("Expected Amber variant");
        }
    }

    #[test]
    fn test_signer_kind_serialization() {
        let ephemeral = SignerKind::Ephemeral;
        let json = serde_json::to_string(&ephemeral).unwrap();
        let deserialized: SignerKind = serde_json::from_str(&json).unwrap();
        assert_eq!(ephemeral, deserialized);
    }

    #[test]
    #[cfg(feature = "insecure-local-signer")]
    fn test_signer_kind_local_serialization() {
        let local = SignerKind::LocalInsecure;
        let json = serde_json::to_string(&local).unwrap();
        let deserialized: SignerKind = serde_json::from_str(&json).unwrap();
        assert_eq!(local, deserialized);
    }
}
