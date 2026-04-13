//! Errors raised by Blossom upload/download flows.
//!
//! These cover every failure mode specific to talking to a Blossom
//! server: URL parsing, HTTP transport, non-success responses,
//! request timeouts, and Nostr auth event signing for BUD-01.
//! Errors from the `nostr-blossom` crate's high-level client are
//! carried through as [`BlossomError::Client`].

use std::time::Duration;

use reqwest::StatusCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BlossomError {
    /// Invalid Blossom server or endpoint URL.
    #[error("Failed to build Blossom URL: {0}")]
    Url(#[from] nostr_sdk::types::url::ParseError),

    /// Transport-level HTTP error from reqwest.
    #[error("Blossom HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// Blossom server returned a non-success HTTP status.
    #[error("Blossom upload failed with status {0}")]
    UploadStatus(StatusCode),

    /// Failed to parse the Blossom server's JSON response.
    #[error("Failed to parse Blossom upload response: {0}")]
    ResponseBody(#[source] reqwest::Error),

    /// Upload exceeded the configured timeout.
    #[error("Blossom upload timed out after {}s", .0.as_secs())]
    Timeout(Duration),

    /// Signing the Nostr auth event for BUD-01 failed.
    #[error("Failed to sign Blossom auth event: {0}")]
    AuthSign(#[from] nostr_sdk::event::builder::Error),

    /// Error surfaced by the `nostr-blossom` high-level client.
    #[error("Blossom client error: {0}")]
    Client(String),
}

impl BlossomError {
    /// Construct a [`BlossomError::Client`] from the opaque
    /// `nostr_blossom::error::Error`, which does not implement `Display`
    /// cleanly across versions but does implement `std::error::Error`.
    pub fn client<E>(err: E) -> Self
    where
        E: std::error::Error,
    {
        Self::Client(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_message_includes_seconds() {
        let err = BlossomError::Timeout(Duration::from_secs(30));
        assert_eq!(err.to_string(), "Blossom upload timed out after 30s");
    }

    #[test]
    fn upload_status_message_includes_code() {
        let err = BlossomError::UploadStatus(StatusCode::BAD_REQUEST);
        assert!(err.to_string().contains("400"));
    }

    #[test]
    fn url_error_conversion() {
        let parse_err = "ht!tp://bad"
            .parse::<nostr_sdk::types::url::Url>()
            .unwrap_err();
        let err: BlossomError = parse_err.into();
        assert!(matches!(err, BlossomError::Url(_)));
    }
}
