use nostr_sdk::{RelayMessage, RelayUrl};

use crate::relay_control::observability::RelayFailureCategory;

/// Normalized relay notification surface for future `RelaySession` wiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayNotification {
    Notice {
        relay_url: RelayUrl,
        message: String,
        failure_category: Option<RelayFailureCategory>,
    },
    Closed {
        relay_url: RelayUrl,
        message: String,
        failure_category: Option<RelayFailureCategory>,
    },
    Auth {
        relay_url: RelayUrl,
        challenge: String,
        failure_category: Option<RelayFailureCategory>,
    },
    #[allow(dead_code)]
    Connected {
        relay_url: RelayUrl,
    },
    #[allow(dead_code)]
    Disconnected {
        relay_url: RelayUrl,
        failure_category: Option<RelayFailureCategory>,
    },
    #[allow(dead_code)]
    Shutdown,
}

impl RelayNotification {
    pub(crate) fn from_message(
        relay_url: RelayUrl,
        message: RelayMessage<'static>,
    ) -> Option<Self> {
        // This normalization is telemetry-oriented. For message types such as
        // `OK` and `EOSE` we intentionally collapse structured payloads into a
        // coarse notification shape instead of preserving every field.
        match message {
            RelayMessage::Event { .. } => None,
            RelayMessage::Notice(message) => Some(Self::Notice {
                relay_url,
                failure_category: Some(RelayFailureCategory::classify_notice(&message)),
                message: message.into_owned(),
            }),
            RelayMessage::Closed {
                subscription_id: _,
                message,
            } => Some(Self::Closed {
                relay_url,
                failure_category: Some(RelayFailureCategory::classify_closed(&message)),
                message: message.into_owned(),
            }),
            RelayMessage::Auth { challenge } => Some(Self::Auth {
                relay_url,
                failure_category: Some(RelayFailureCategory::classify_auth(&challenge)),
                challenge: challenge.into_owned(),
            }),
            RelayMessage::Ok { .. } => Some(Self::Notice {
                relay_url,
                message: "ok".to_string(),
                failure_category: None,
            }),
            RelayMessage::EndOfStoredEvents(_) => Some(Self::Notice {
                relay_url,
                message: "eose".to_string(),
                failure_category: None,
            }),
            RelayMessage::Count { .. } => Some(Self::Notice {
                relay_url,
                message: "count".to_string(),
                failure_category: None,
            }),
            RelayMessage::NegMsg { .. } => Some(Self::Notice {
                relay_url,
                message: "negmsg".to_string(),
                failure_category: None,
            }),
            RelayMessage::NegErr { .. } => Some(Self::Notice {
                relay_url,
                message: "negerr".to_string(),
                failure_category: None,
            }),
        }
    }
}
