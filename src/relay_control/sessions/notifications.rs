use nostr_sdk::{Event, RelayUrl, SubscriptionId};

use crate::relay_control::observability::RelayFailureCategory;

/// Normalized relay notification surface for future `RelaySession` wiring.
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
