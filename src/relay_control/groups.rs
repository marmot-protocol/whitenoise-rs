use nostr_sdk::RelayUrl;

use super::sessions::RelaySessionReconnectPolicy;

/// Configuration for the long-lived group-message plane.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct GroupPlaneConfig {
    pub(crate) relays: Vec<RelayUrl>,
    pub(crate) group_ids: Vec<String>,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
}
