use crate::api::error::ApiError;
use flutter_rust_bridge::frb;
use nostr_sdk::prelude::RelayUrl;

pub const DEFAULT_RELAY_URLS: [&str; 3] = [
    "wss://nos.lol",
    "wss://relay.primal.net",
    "wss://relay.damus.io",
];

#[frb]
pub fn default_relay_urls_parsed() -> Result<Vec<RelayUrl>, ApiError> {
    DEFAULT_RELAY_URLS
        .into_iter()
        .map(RelayUrl::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(ApiError::from)
}

#[frb(sync)]
pub fn default_relay_urls() -> Vec<String> {
    DEFAULT_RELAY_URLS
        .iter()
        .map(|url| (*url).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_RELAY_URLS, default_relay_urls, default_relay_urls_parsed};

    #[test]
    fn default_relay_urls_parse() {
        let relays = default_relay_urls_parsed()
            .expect("default relay URLs must stay valid")
            .into_iter()
            .map(|relay| relay.to_string())
            .collect::<Vec<_>>();

        assert_eq!(relays, DEFAULT_RELAY_URLS);
    }

    #[test]
    fn default_relay_urls_strings_match_constant() {
        let relay_urls = default_relay_urls();
        let expected = DEFAULT_RELAY_URLS
            .iter()
            .map(|url| (*url).to_string())
            .collect::<Vec<_>>();
        assert_eq!(relay_urls, expected);
    }
}
