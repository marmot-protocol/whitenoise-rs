use nostr_sdk::RelayUrl;

use super::sessions::RelaySessionReconnectPolicy;

/// Configuration for the long-lived discovery plane.
#[allow(dead_code)]
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

#[allow(dead_code)]
impl DiscoveryPlaneConfig {
    pub(crate) fn new(relays: Vec<RelayUrl>) -> Self {
        Self {
            relays,
            reconnect_policy: RelaySessionReconnectPolicy::Conservative,
        }
    }

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
        .map(|relay| {
            RelayUrl::parse(relay)
                .unwrap_or_else(|error| panic!("invalid curated relay {relay}: {error}"))
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_curated_default_relays_match_literal_count() {
        let relays = DiscoveryPlaneConfig::curated_default_relays();
        assert_eq!(relays.len(), 7);
        assert_eq!(
            relays[0],
            RelayUrl::parse("wss://index.hzrd149.com").unwrap()
        );
        assert_eq!(relays[6], RelayUrl::parse("wss://nos.lol").unwrap());
    }

    #[test]
    fn test_new_preserves_provided_relays() {
        let relays = vec![RelayUrl::parse("ws://localhost:8080").unwrap()];
        let config = DiscoveryPlaneConfig::new(relays.clone());

        assert_eq!(config.relays, relays);
        assert_eq!(
            config.reconnect_policy,
            RelaySessionReconnectPolicy::Conservative
        );
    }
}
