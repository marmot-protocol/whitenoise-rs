use nostr_sdk::prelude::*;

use crate::WhitenoiseError;

pub const JEFF_PUBKEY_HEX: &str =
    "1739d937dc8c0c7370aa27585938c119e25c41f6c441a5d34c6d38503e3136ef";
pub const MAX_PUBKEY_HEX: &str = "b7ed68b062de6b4a12e51fd5285c1e1e0ed0e5128cda93ab11b4150b55ed32fc";

/// Pre-signed Nostr events used by integration-style tests.
///
/// These are real events for Jeff and one of his follows (Max), plus Jeff's
/// contact list. They are stored as root-level fixtures so both the
/// integration-test harness and external CLI E2E tests can seed local relays
/// without touching the public network.
///
/// To refresh, run from the repo root:
/// ```sh
/// nak req -k 0 -a 1739d937dc8c0c7370aa27585938c119e25c41f6c441a5d34c6d38503e3136ef wss://relay.damus.io | head -1 > test_fixtures/nostr/jeff_metadata.json
/// nak req -k 3 -a 1739d937dc8c0c7370aa27585938c119e25c41f6c441a5d34c6d38503e3136ef wss://relay.damus.io | head -1 > test_fixtures/nostr/jeff_contacts.json
/// nak req -k 0 -a b7ed68b062de6b4a12e51fd5285c1e1e0ed0e5128cda93ab11b4150b55ed32fc wss://relay.damus.io | head -1 > test_fixtures/nostr/max_metadata.json
/// ```
const JEFF_METADATA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/test_fixtures/nostr/jeff_metadata.json"
));
const JEFF_CONTACTS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/test_fixtures/nostr/jeff_contacts.json"
));
const MAX_METADATA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/test_fixtures/nostr/max_metadata.json"
));

fn parse_seed_events() -> Vec<Event> {
    [JEFF_METADATA, JEFF_CONTACTS, MAX_METADATA]
        .iter()
        .map(|json| Event::from_json(json).expect("valid seed event JSON"))
        .collect()
}

/// Publishes the shared user-search seed events to the given relays.
pub async fn publish_user_search_seed_events(relays: &[&str]) -> Result<(), WhitenoiseError> {
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
