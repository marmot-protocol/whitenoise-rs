use nostr_sdk::RelayUrl;

use super::sessions::RelaySessionReconnectPolicy;

/// Configuration for the long-lived discovery plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveryPlaneConfig {
    pub(crate) relays: Vec<RelayUrl>,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
}

impl Default for DiscoveryPlaneConfig {
    fn default() -> Self {
        Self {
            relays: Self::curated_default_relays(),
            reconnect_policy: RelaySessionReconnectPolicy::Conservative,
        }
    }
}

impl DiscoveryPlaneConfig {
    /// Initial curated relay set from the planning doc.
    pub(crate) fn curated_default_relays() -> Vec<RelayUrl> {
        [
            "wss://index.hzrd149.com",
            "wss://indexer.coracle.social",
            "wss://purplepag.es",
            "wss://relay.primal.net",
            "wss://relay.damus.io",
            "wss://relay.ditto.pub",
            "wss://nos.lol",
        ]
        .into_iter()
        .filter_map(|relay| RelayUrl::parse(relay).ok())
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_curated_default_relays_not_empty() {
        assert!(!DiscoveryPlaneConfig::curated_default_relays().is_empty());
    }
}
