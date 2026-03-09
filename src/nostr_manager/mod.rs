use nostr_sdk::prelude::*;
use thiserror::Error;

use crate::whitenoise::database::DatabaseError;

pub mod parser;
pub mod utils;

#[derive(Error, Debug)]
pub enum NostrManagerError {
    #[error("Whitenoise Instance Error: {0}")]
    WhitenoiseInstance(String),
    #[error("Client Error: {0}")]
    Client(nostr_sdk::client::Error),
    #[error("Database Error: {0}")]
    Database(#[from] DatabaseError),
    #[error("Signer Error: {0}")]
    Signer(#[from] nostr_sdk::signer::SignerError),
    #[error("Error with secrets store: {0}")]
    SecretsStoreError(String),
    #[error("Failed to queue event: {0}")]
    FailedToQueueEvent(String),
    #[error("Failed to shutdown event processor: {0}")]
    FailedToShutdownEventProcessor(String),
    #[error("Account error: {0}")]
    AccountError(String),
    #[error("Failed to connect to any relays")]
    NoRelayConnections,
    #[error("No relay accepted the event")]
    NoRelayAccepted,
    #[error("Relay operation timed out")]
    Timeout,
    #[error("Nostr Event error: {0}")]
    NostrEventBuilderError(#[from] nostr_sdk::event::builder::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
    #[error("Event processing error: {0}")]
    EventProcessingError(String),
    #[error("Failed to track published event: {0}")]
    FailedToTrackPublishedEvent(String),
    #[error("Invalid timestamp")]
    InvalidTimestamp,
}

impl From<nostr_sdk::client::Error> for NostrManagerError {
    fn from(err: nostr_sdk::client::Error) -> Self {
        match &err {
            nostr_sdk::client::Error::Relay(nostr_sdk::pool::relay::Error::Timeout) => {
                Self::Timeout
            }
            _ => Self::Client(err),
        }
    }
}

pub type Result<T> = std::result::Result<T, NostrManagerError>;

#[cfg(test)]
mod subscription_monitoring_tests {
    use super::*;

    #[test]
    fn test_client_relay_timeout_maps_to_timeout_variant() {
        let relay_timeout = nostr_sdk::client::Error::Relay(nostr_sdk::pool::relay::Error::Timeout);
        let err = NostrManagerError::from(relay_timeout);
        assert!(
            matches!(err, NostrManagerError::Timeout),
            "Expected Timeout variant, got: {:?}",
            err
        );
    }

    #[test]
    fn test_client_non_timeout_maps_to_client_variant() {
        let signer_err = nostr_sdk::client::Error::Signer(nostr_sdk::signer::SignerError::backend(
            std::io::Error::other("test error"),
        ));
        let err = NostrManagerError::from(signer_err);
        assert!(
            matches!(err, NostrManagerError::Client(_)),
            "Expected Client variant, got: {:?}",
            err
        );
    }
}
