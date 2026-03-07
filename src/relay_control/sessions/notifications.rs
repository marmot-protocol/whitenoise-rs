use nostr_sdk::{Event, RelayMessage, RelayUrl, SubscriptionId};

use crate::relay_control::observability::RelayFailureCategory;

/// Normalized relay notification surface for future `RelaySession` wiring.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayNotification {
    Event {
        relay_url: RelayUrl,
        subscription_id: SubscriptionId,
        event: Event,
    },
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
    Connected {
        relay_url: RelayUrl,
    },
    Disconnected {
        relay_url: RelayUrl,
        failure_category: Option<RelayFailureCategory>,
    },
    Shutdown,
}

impl RelayNotification {
    pub(crate) fn from_message(relay_url: RelayUrl, message: RelayMessage<'static>) -> Self {
        // This normalization is telemetry-oriented. For message types such as
        // `OK` and `EOSE` we intentionally collapse structured payloads into a
        // coarse notification shape instead of preserving every field.
        match message {
            RelayMessage::Event {
                subscription_id,
                event,
            } => Self::Event {
                relay_url,
                subscription_id: subscription_id.into_owned(),
                event: event.into_owned(),
            },
            RelayMessage::Notice(message) => Self::Notice {
                relay_url,
                failure_category: Some(RelayFailureCategory::classify_notice(&message)),
                message: message.into_owned(),
            },
            RelayMessage::Closed {
                subscription_id: _,
                message,
            } => Self::Closed {
                relay_url,
                failure_category: Some(RelayFailureCategory::classify_closed(&message)),
                message: message.into_owned(),
            },
            RelayMessage::Auth { challenge } => Self::Auth {
                relay_url,
                failure_category: Some(RelayFailureCategory::classify_auth(&challenge)),
                challenge: challenge.into_owned(),
            },
            RelayMessage::Ok { .. } => Self::Notice {
                relay_url,
                message: "ok".to_string(),
                failure_category: None,
            },
            RelayMessage::EndOfStoredEvents(_) => Self::Notice {
                relay_url,
                message: "eose".to_string(),
                failure_category: None,
            },
            RelayMessage::Count { .. } => Self::Notice {
                relay_url,
                message: "count".to_string(),
                failure_category: None,
            },
            RelayMessage::NegMsg { .. } => Self::Notice {
                relay_url,
                message: "negmsg".to_string(),
                failure_category: None,
            },
            RelayMessage::NegErr { .. } => Self::Notice {
                relay_url,
                message: "negerr".to_string(),
                failure_category: None,
            },
        }
    }
}
