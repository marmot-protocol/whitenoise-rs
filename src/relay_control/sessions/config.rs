use std::time::Duration;

use nostr_sdk::PublicKey;

use crate::relay_control::RelayPlane;

/// Session-level auth policy.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum RelaySessionAuthPolicy {
    #[default]
    Disabled,
    Allowed,
    Required,
}

/// Session-level reconnect policy.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum RelaySessionReconnectPolicy {
    Conservative,
    FreshnessBiased,
    #[default]
    Disabled,
}

/// Session-level relay membership policy.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum RelaySessionRelayPolicy {
    #[default]
    Dynamic,
    ExplicitOnly,
}

/// Shared session configuration reused by all future relay planes.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelaySessionConfig {
    pub(crate) plane: RelayPlane,
    pub(crate) telemetry_account_pubkey: Option<PublicKey>,
    pub(crate) auth_policy: RelaySessionAuthPolicy,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
    pub(crate) relay_policy: RelaySessionRelayPolicy,
    pub(crate) connect_timeout: Duration,
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
        }
    }
}
