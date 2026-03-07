use std::time::Duration;

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

/// Shared session configuration reused by all future relay planes.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelaySessionConfig {
    pub(crate) plane: RelayPlane,
    pub(crate) auth_policy: RelaySessionAuthPolicy,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
    pub(crate) connect_timeout: Duration,
}

impl RelaySessionConfig {
    pub(crate) fn new(plane: RelayPlane) -> Self {
        Self {
            plane,
            auth_policy: RelaySessionAuthPolicy::Disabled,
            reconnect_policy: RelaySessionReconnectPolicy::Disabled,
            connect_timeout: Duration::from_secs(5),
        }
    }
}
