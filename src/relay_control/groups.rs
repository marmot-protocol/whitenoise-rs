use nostr_sdk::RelayUrl;

use super::sessions::RelaySessionReconnectPolicy;

/// Configuration for the long-lived group-message plane.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupPlaneConfig {
    pub(crate) relays: Vec<RelayUrl>,
    pub(crate) group_ids: Vec<String>,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
}

impl Default for GroupPlaneConfig {
    fn default() -> Self {
        Self {
            relays: Vec::new(),
            group_ids: Vec::new(),
            reconnect_policy: RelaySessionReconnectPolicy::FreshnessBiased,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_uses_explicit_group_reconnect_policy() {
        let config = GroupPlaneConfig::default();
        assert_eq!(config.relays, Vec::<RelayUrl>::new());
        assert_eq!(config.group_ids, Vec::<String>::new());
        assert_eq!(
            config.reconnect_policy,
            RelaySessionReconnectPolicy::FreshnessBiased
        );
    }
}
