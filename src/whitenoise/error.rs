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

    #[error("Already departed from group")]
    AlreadyDepartedFromGroup,

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

    #[error("Missing user reference: account exists but linked user row is absent (broken FK)")]
    MissingUserReference,

    #[error("Failed to resolve account ID for pubkey")]
    ResolveAccountId,

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

    #[error("MIP-05 error: {0}")]
    Mip05(#[from] mdk_core::mip05::Mip05Error),

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

    #[error("Invalid event kind: expected {expected}, got {got}")]
    InvalidEventKind { expected: String, got: String },

    #[error("Missing required encoding tag ['encoding','base64']")]
    MissingEncodingTag,

    #[error("Invalid base64 key package content: {0}")]
    InvalidBase64(#[from] base64ct::Error),

    #[error(
        "Incompatible mls_ciphersuite: expected {expected}, advertised [{}]",
        advertised.join(", ")
    )]
    IncompatibleMlsCiphersuite {
        expected: String,
        advertised: Vec<String>,
    },

    #[error("Missing required mls_extensions [{}]", missing.join(", "))]
    MissingMlsExtensions { missing: Vec<String> },

    #[error("Invalid timestamp")]
    InvalidTimestamp,

    #[error("Invalid cursor: {reason}")]
    InvalidCursor { reason: &'static str },

    #[error("Media cache operation failed: {0}")]
    MediaCache(String),

    #[error("Failed to download from Blossom server: {0}")]
    BlossomDownload(String),

    #[error("Blossom URL must use HTTPS: {0}")]
    BlossomInsecureUrl(String),

    #[error("Key package publish failed: {0}")]
    KeyPackagePublishFailed(String),

    #[error("MLS message unprocessable: {0}")]
    MlsMessageUnprocessable(String),

    #[error("MLS message previously failed and cannot be reprocessed")]
    MlsMessagePreviouslyFailed,

    #[error("Event publish failed: no relay accepted the event")]
    EventPublishNoRelayAccepted,

    #[error("Image decryption failed: {0}")]
    ImageDecryptionFailed(String),

    #[error("Hash verification failed - expected: {expected}, actual: {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("Unsupported media format: {0}")]
    UnsupportedMediaFormat(String),

    #[error("Download rejected: response body exceeds the {limit} byte size limit")]
    DownloadSizeLimitExceeded { limit: usize },

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
