use nostr_sdk::prelude::PublicKey;
use thiserror::Error;

use crate::{
    nostr_manager::NostrManagerError,
    whitenoise::{
        accounts::{AccountError, LoginError},
        database::DatabaseError,
        message_aggregator::ProcessingError,
        secrets_store::SecretsStoreError,
    },
};

pub type Result<T> = core::result::Result<T, WhitenoiseError>;

#[derive(Error, Debug)]
pub enum WhitenoiseError {
    #[error("Failed to initialize Whitenoise")]
    Initialization,

    #[error("Filesystem error: {0}")]
    Filesystem(#[from] std::io::Error),

    #[error("Logging setup error: {0}")]
    LoggingSetup(String),

    #[error("Configuration error: {0}")]
    Configuration(String),

    #[error("Contact list error: {0}")]
    ContactList(String),

    #[error("MDK SQLite storage error: {0}")]
    MdkSqliteStorage(#[from] mdk_sqlite_storage::error::Error),

    #[error("Group not found")]
    GroupNotFound,

    #[error("Message not found")]
    MessageNotFound,

    #[error("Group has no relays configured")]
    GroupMissingRelays,

    #[error("Account has no key package relays configured")]
    AccountMissingKeyPackageRelays,

    #[error("Account not found")]
    AccountNotFound,

    #[error("User not found")]
    UserNotFound,

    #[error("User not persisted - save the user before performing this operation")]
    UserNotPersisted,

    #[error("Contact not found")]
    ContactNotFound,

    #[error("Relay not found")]
    RelayNotFound,

    #[error("User relay not found")]
    UserRelayNotFound,

    #[error("Account not authorized")]
    AccountNotAuthorized,

    #[error("Cannot export nsec for external signer account - private key is not stored locally")]
    ExternalSignerCannotExportNsec,

    #[error("Cannot register external signer for a non-external account")]
    NotExternalSignerAccount,

    #[error("MDK error: {0}")]
    MdkCoreError(#[from] mdk_core::Error),

    #[error("Invalid event: {0}")]
    InvalidEvent(String),

    #[error("Invalid public key")]
    InvalidPublicKey,

    #[error("Secrets store error: {0}")]
    SecretsStore(#[from] SecretsStoreError),

    #[error("Nostr client error: {0}")]
    NostrClient(#[from] nostr_sdk::client::Error),

    #[error("Nostr key error: {0}")]
    NostrKey(#[from] nostr_sdk::key::Error),

    #[error("Nostr url error: {0}")]
    NostrUrl(#[from] nostr_sdk::types::url::Error),

    #[error("Nostr tag error: {0}")]
    NostrTag(#[from] nostr_sdk::event::tag::Error),

    #[error("Database error: {0}")]
    Database(#[from] DatabaseError),

    #[error("Account error: {0}")]
    Account(#[from] AccountError),

    #[error("Login error: {0}")]
    Login(#[from] LoginError),

    #[error("SQLx error: {0}")]
    SqlxError(#[from] sqlx::Error),

    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Nostr manager error: {0}")]
    NostrManager(#[from] NostrManagerError),

    #[error("One or more members to remove are not in the group")]
    MembersNotInGroup,

    #[error("Welcome not found")]
    WelcomeNotFound,

    #[error("nip04 direct message error")]
    Nip04Error(#[from] nostr_sdk::nips::nip04::Error),

    #[error("join error due to spawn blocking")]
    JoinError(#[from] tokio::task::JoinError),

    #[error("Event handler error: {0}")]
    EventProcessor(String),

    #[error("Message aggregation error: {0}")]
    MessageAggregation(#[from] ProcessingError),

    #[error("Other error: {0}")]
    Other(#[from] anyhow::Error),

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Invalid timestamp")]
    InvalidTimestamp,

    #[error("Media cache operation failed: {0}")]
    MediaCache(String),

    #[error("Failed to download from Blossom server: {0}")]
    BlossomDownload(String),

    #[error("Key package publish failed: {0}")]
    KeyPackagePublishFailed(String),

    #[error("Image decryption failed: {0}")]
    ImageDecryptionFailed(String),

    #[error("Hash verification failed - expected: {expected}, actual: {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("Unsupported media format: {0}")]
    UnsupportedMediaFormat(String),

    #[error(
        "Cannot deliver MLS welcome for {member_pubkey}: no inbox/NIP-65 relays configured and account {account_pubkey} has no fallback relays"
    )]
    MissingWelcomeRelays {
        member_pubkey: PublicKey,
        account_pubkey: PublicKey,
    },
}

impl From<Box<dyn std::error::Error + Send + Sync>> for WhitenoiseError {
    fn from(err: Box<dyn std::error::Error + Send + Sync>) -> Self {
        WhitenoiseError::Other(anyhow::anyhow!(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use crate::whitenoise::accounts::LoginError;

    #[test]
    fn io_errors_convert_into_filesystem_variant() {
        let io_error = std::io::Error::other("disk error");
        let err: WhitenoiseError = io_error.into();
        assert!(matches!(err, WhitenoiseError::Filesystem(_)));
    }

    #[test]
    fn boxed_errors_map_to_other_variant() {
        let boxed: Box<dyn std::error::Error + Send + Sync> = std::io::Error::other("boom").into();
        let err = WhitenoiseError::from(boxed);
        assert!(matches!(err, WhitenoiseError::Other(_)));
        assert!(format!("{err}").contains("boom"));
    }

    #[test]
    fn missing_welcome_relays_format_includes_pubkeys() {
        let member =
            PublicKey::from_str("1111111111111111111111111111111111111111111111111111111111111111")
                .unwrap();
        let account =
            PublicKey::from_str("2222222222222222222222222222222222222222222222222222222222222222")
                .unwrap();

        let message = WhitenoiseError::MissingWelcomeRelays {
            member_pubkey: member,
            account_pubkey: account,
        }
        .to_string();

        assert!(message.contains(&member.to_string()));
        assert!(message.contains(&account.to_string()));
    }

    #[test]
    fn test_simple_error_display_messages() {
        assert_eq!(
            WhitenoiseError::Initialization.to_string(),
            "Failed to initialize Whitenoise"
        );
        assert_eq!(
            WhitenoiseError::GroupNotFound.to_string(),
            "Group not found"
        );
        assert_eq!(
            WhitenoiseError::MessageNotFound.to_string(),
            "Message not found"
        );
        assert_eq!(
            WhitenoiseError::AccountNotFound.to_string(),
            "Account not found"
        );
        assert_eq!(WhitenoiseError::UserNotFound.to_string(), "User not found");
        assert_eq!(
            WhitenoiseError::ContactNotFound.to_string(),
            "Contact not found"
        );
        assert_eq!(
            WhitenoiseError::RelayNotFound.to_string(),
            "Relay not found"
        );
        assert_eq!(
            WhitenoiseError::UserRelayNotFound.to_string(),
            "User relay not found"
        );
        assert_eq!(
            WhitenoiseError::WelcomeNotFound.to_string(),
            "Welcome not found"
        );
        assert_eq!(
            WhitenoiseError::InvalidTimestamp.to_string(),
            "Invalid timestamp"
        );
        assert_eq!(
            WhitenoiseError::InvalidPublicKey.to_string(),
            "Invalid public key"
        );
        assert_eq!(
            WhitenoiseError::AccountNotAuthorized.to_string(),
            "Account not authorized"
        );
        assert_eq!(
            WhitenoiseError::GroupMissingRelays.to_string(),
            "Group has no relays configured"
        );
        assert_eq!(
            WhitenoiseError::AccountMissingKeyPackageRelays.to_string(),
            "Account has no key package relays configured"
        );
        assert_eq!(
            WhitenoiseError::MembersNotInGroup.to_string(),
            "One or more members to remove are not in the group"
        );
        assert_eq!(
            WhitenoiseError::UserNotPersisted.to_string(),
            "User not persisted - save the user before performing this operation"
        );
    }

    #[test]
    fn test_parameterized_error_display_messages() {
        assert_eq!(
            WhitenoiseError::LoggingSetup("init failed".to_string()).to_string(),
            "Logging setup error: init failed"
        );
        assert_eq!(
            WhitenoiseError::Configuration("bad config".to_string()).to_string(),
            "Configuration error: bad config"
        );
        assert_eq!(
            WhitenoiseError::ContactList("sync failed".to_string()).to_string(),
            "Contact list error: sync failed"
        );
        assert_eq!(
            WhitenoiseError::InvalidEvent("malformed".to_string()).to_string(),
            "Invalid event: malformed"
        );
        assert_eq!(
            WhitenoiseError::EventProcessor("handler failed".to_string()).to_string(),
            "Event handler error: handler failed"
        );
        assert_eq!(
            WhitenoiseError::InvalidInput("bad input".to_string()).to_string(),
            "Invalid input: bad input"
        );
        assert_eq!(
            WhitenoiseError::MediaCache("cache miss".to_string()).to_string(),
            "Media cache operation failed: cache miss"
        );
        assert_eq!(
            WhitenoiseError::BlossomDownload("timeout".to_string()).to_string(),
            "Failed to download from Blossom server: timeout"
        );
        assert_eq!(
            WhitenoiseError::KeyPackagePublishFailed("no relays accepted".to_string()).to_string(),
            "Key package publish failed: no relays accepted"
        );
        assert_eq!(
            WhitenoiseError::ImageDecryptionFailed("bad key".to_string()).to_string(),
            "Image decryption failed: bad key"
        );
        assert_eq!(
            WhitenoiseError::UnsupportedMediaFormat("webp".to_string()).to_string(),
            "Unsupported media format: webp"
        );
    }

    #[test]
    fn test_hash_mismatch_error_display() {
        let err = WhitenoiseError::HashMismatch {
            expected: "abc123".to_string(),
            actual: "def456".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("abc123"));
        assert!(msg.contains("def456"));
        assert!(msg.contains("Hash verification failed"));
    }

    #[test]
    fn test_json_error_conversion() {
        let json_err: serde_json::Error =
            serde_json::from_str::<String>("not valid json").unwrap_err();
        let err: WhitenoiseError = json_err.into();
        assert!(matches!(err, WhitenoiseError::SerializationError(_)));
    }

    #[test]
    fn test_sqlx_error_conversion() {
        // Create a SQLx error by trying to execute invalid SQL
        // We can't easily create a real SQLx error without a database, so we test the pattern
        let err = WhitenoiseError::SqlxError(sqlx::Error::RowNotFound);
        assert!(err.to_string().contains("SQLx error"));
    }

    #[test]
    fn test_error_debug_impl() {
        let err = WhitenoiseError::GroupNotFound;
        // Debug impl should work without panicking
        let debug_str = format!("{:?}", err);
        assert!(!debug_str.is_empty());
    }

    #[test]
    fn login_error_converts_to_whitenoise_error() {
        let login_err = LoginError::InvalidKeyFormat("bad key".to_string());
        let err: WhitenoiseError = login_err.into();
        assert!(matches!(err, WhitenoiseError::Login(_)));
        assert!(err.to_string().contains("bad key"));
    }

    #[test]
    fn login_error_display_messages() {
        assert_eq!(
            LoginError::InvalidKeyFormat("not an nsec".to_string()).to_string(),
            "Invalid private key format: not an nsec"
        );
        assert_eq!(
            LoginError::NoRelayConnections.to_string(),
            "Failed to connect to any relays"
        );
        assert_eq!(
            LoginError::Timeout("relay fetch".to_string()).to_string(),
            "Login operation timed out: relay fetch"
        );
        assert_eq!(
            LoginError::NoLoginInProgress.to_string(),
            "No login in progress for this account"
        );
        assert_eq!(
            LoginError::Internal("something broke".to_string()).to_string(),
            "Login error: something broke"
        );
    }
}
