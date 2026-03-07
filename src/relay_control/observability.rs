use std::time::Duration;

use nostr_sdk::{PublicKey, RelayUrl};

use super::RelayPlane;

/// High-level relay failure classification to be persisted in later phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RelayFailureCategory {
    Transport,
    Timeout,
    AuthRequired,
    AuthFailed,
    RelayPolicy,
    InvalidFilter,
    RateLimited,
    ClosedByRelay,
    Unknown,
}

/// Structured relay telemetry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RelayTelemetryKind {
    Connected,
    Disconnected,
    Notice,
    Closed,
    AuthChallenge,
    PublishAttempt,
    PublishSuccess,
    PublishFailure,
    QueryAttempt,
    QuerySuccess,
    QueryFailure,
    SubscriptionAttempt,
    SubscriptionSuccess,
    SubscriptionFailure,
}

/// Normalized relay telemetry payload shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayTelemetryEvent {
    pub(crate) kind: RelayTelemetryKind,
    pub(crate) plane: RelayPlane,
    pub(crate) account_pubkey: Option<PublicKey>,
    pub(crate) relay_url: RelayUrl,
    pub(crate) subscription_id: Option<String>,
    pub(crate) failure_category: Option<RelayFailureCategory>,
    pub(crate) message: Option<String>,
}

/// Static observability configuration owned by the control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayObservabilityConfig {
    pub(crate) recent_event_limit: usize,
    pub(crate) status_staleness_window: Duration,
}

impl Default for RelayObservabilityConfig {
    fn default() -> Self {
        Self {
            recent_event_limit: 200,
            status_staleness_window: Duration::from_secs(60 * 5),
        }
    }
}

/// Relay-observability host for future persistence and aggregation logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayObservability {
    config: RelayObservabilityConfig,
}

impl RelayObservability {
    pub(crate) fn new(config: RelayObservabilityConfig) -> Self {
        Self { config }
    }

    pub(crate) fn config(&self) -> &RelayObservabilityConfig {
        &self.config
    }
}
