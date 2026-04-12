//! Errors raised while initializing a streaming subscription.
//!
//! These originate from `tokio::sync::broadcast` receivers used by the
//! streaming layer (messages, users, chat list, archived chat list,
//! notifications). The receiver variants we surface are:
//! - `Lagged(n)` — the subscription missed `n` messages before we could
//!   drain them, so the initial snapshot is incomplete and the caller
//!   should retry.
//! - `Closed` — the sender was dropped, which is unreachable while we
//!   hold a receiver. Reported defensively so callers see the problem
//!   instead of silently exiting.

use thiserror::Error;

/// Identifies which stream raised a streaming error. Used purely for
/// diagnostic messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Message,
    User,
    ChatList,
    ArchivedChatList,
}

impl StreamKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Message => "Message",
            Self::User => "User",
            Self::ChatList => "Chat list",
            Self::ArchivedChatList => "Archived chat list",
        }
    }
}

impl std::fmt::Display for StreamKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Error)]
pub enum StreamingError {
    /// The broadcast channel lagged during snapshot initialization; the
    /// subscription drained less than was published, so the initial
    /// snapshot would be incomplete. Caller should retry.
    #[error(
        "{stream} stream lagged by {missed} messages during subscription initialization, retry needed"
    )]
    Lagged { stream: StreamKind, missed: u64 },

    /// The broadcast sender was dropped while a receiver was still held;
    /// this is unreachable under normal operation.
    #[error("{stream} stream closed unexpectedly during subscription")]
    Closed { stream: StreamKind },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lagged_message_includes_stream_and_count() {
        let err = StreamingError::Lagged {
            stream: StreamKind::Message,
            missed: 42,
        };
        let msg = err.to_string();
        assert!(msg.contains("Message"));
        assert!(msg.contains("42"));
        assert!(msg.contains("retry needed"));
    }

    #[test]
    fn closed_message_includes_stream() {
        let err = StreamingError::Closed {
            stream: StreamKind::ChatList,
        };
        assert_eq!(
            err.to_string(),
            "Chat list stream closed unexpectedly during subscription"
        );
    }
}
