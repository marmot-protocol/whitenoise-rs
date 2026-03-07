use std::time::Duration;

use super::sessions::{RelaySessionAuthPolicy, RelaySessionReconnectPolicy};

/// Configuration for short-lived, targeted relay work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EphemeralPlaneConfig {
    pub(crate) timeout: Duration,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
    pub(crate) auth_policy: RelaySessionAuthPolicy,
}

impl Default for EphemeralPlaneConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            reconnect_policy: RelaySessionReconnectPolicy::Disabled,
            auth_policy: RelaySessionAuthPolicy::Disabled,
        }
    }
}
