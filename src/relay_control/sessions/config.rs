use std::time::Duration;

use nostr_sdk::PublicKey;

use crate::relay_control::RelayPlane;

/// Session-level auth policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum RelaySessionAuthPolicy {
    #[default]
    Disabled,
    Allowed,
}

/// Session-level reconnect policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum RelaySessionReconnectPolicy {
    Conservative,
    FreshnessBiased,
    #[default]
    Disabled,
}

/// Session-level relay membership policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum RelaySessionRelayPolicy {
    #[default]
    Dynamic,
}

/// Shared session configuration reused by all future relay planes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelaySessionConfig {
    pub(crate) plane: RelayPlane,
    pub(crate) telemetry_account_pubkey: Option<PublicKey>,
    pub(crate) auth_policy: RelaySessionAuthPolicy,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
    pub(crate) relay_policy: RelaySessionRelayPolicy,
    pub(crate) connect_timeout: Duration,
    /// Minimum number of relays that must be connected before proceeding.
    /// When set, `prepare_relay_urls` returns as soon as this threshold is
    /// reached instead of waiting for every relay. Remaining connections
    /// continue in the background and nostr-sdk auto-resubscribes them.
    /// When `None`, waits for all relays (the default).
    pub(crate) min_connected_relays: Option<usize>,
}

impl RelaySessionConfig {
    pub(crate) fn new(plane: RelayPlane) -> Self {
        Self {
            plane,
            telemetry_account_pubkey: None,
            auth_policy: RelaySessionAuthPolicy::Disabled,
            reconnect_policy: RelaySessionReconnectPolicy::Disabled,
            relay_policy: RelaySessionRelayPolicy::Dynamic,
            connect_timeout: Duration::from_secs(5),
            min_connected_relays: None,
        }
    }
}
