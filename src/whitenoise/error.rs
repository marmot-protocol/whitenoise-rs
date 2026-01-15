use nostr_sdk::prelude::PublicKey;
use thiserror::Error;

use crate::{
    nostr_manager::NostrManagerError,
    whitenoise::{
        accounts::AccountError, database::DatabaseError, message_aggregator::ProcessingError,
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
}
