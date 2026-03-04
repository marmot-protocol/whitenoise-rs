use nostr_sdk::prelude::*;

use crate::WhitenoiseError;

/// Pre-signed events fetched from public relays via `nak`.
///
/// These are real nostr events for the fallback seed user (Jeff) and one of
/// his follows (Max). Storing them as fixtures lets the fallback-seed
/// integration test run against local relays without touching the public
/// network.
///
/// To refresh, run from the repo root:
/// ```sh
/// nak req -k 0 -a 1739d937dc8c0c7370aa27585938c119e25c41f6c441a5d34c6d38503e3136ef wss://relay.damus.io | head -1 > src/integration_tests/test_cases/user_search/seeds/jeff_metadata.json
/// nak req -k 3 -a 1739d937dc8c0c7370aa27585938c119e25c41f6c441a5d34c6d38503e3136ef wss://relay.damus.io | head -1 > src/integration_tests/test_cases/user_search/seeds/jeff_contacts.json
/// nak req -k 0 -a b7ed68b062de6b4a12e51fd5285c1e1e0ed0e5128cda93ab11b4150b55ed32fc wss://relay.damus.io | head -1 > src/integration_tests/test_cases/user_search/seeds/max_metadata.json
/// ```
const JEFF_METADATA: &str = include_str!("seeds/jeff_metadata.json");
const JEFF_CONTACTS: &str = include_str!("seeds/jeff_contacts.json");
const MAX_METADATA: &str = include_str!("seeds/max_metadata.json");

fn parse_seed_events() -> Vec<Event> {
    [JEFF_METADATA, JEFF_CONTACTS, MAX_METADATA]
        .iter()
        .map(|json| Event::from_json(json).expect("valid seed event JSON"))
        .collect()
}

/// Publishes the fallback-seed fixture events to the given relays.
///
/// Connects a throwaway client, sends the pre-signed events, and disconnects.
pub async fn publish_fallback_seed_events(relays: &[&str]) -> Result<(), WhitenoiseError> {
    let client = Client::default();
    for relay in relays {
        client.add_relay(*relay).await?;
    }
    client.connect().await;

    for event in parse_seed_events() {
        client.send_event(&event).await?;
    }

    client.disconnect().await;
    Ok(())
}
