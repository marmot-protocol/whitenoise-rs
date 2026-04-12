//! Error type for the `wn` and `wnd` command-line binaries.
//!
//! CLI surfaces a small set of failure modes: IPC with the daemon,
//! serialization, I/O, and ad-hoc operational errors (missing binaries,
//! cancelled prompts, etc). This module centralizes them without relying
//! on `anyhow` so the library stays free of dynamic error types.

use std::io;

use thiserror::Error;

use crate::whitenoise::error::WhitenoiseError;

pub type Result<T> = core::result::Result<T, CliError>;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("Whitenoise error: {0}")]
    Whitenoise(#[from] WhitenoiseError),

    /// The daemon is not reachable. The message is already user-facing.
    #[error("{0}")]
    DaemonUnavailable(String),

    /// Generic CLI-level failure with a human-readable message.
    #[error("{0}")]
    Message(String),
}

impl CliError {
    /// Convenience constructor for ad-hoc CLI errors.
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}
