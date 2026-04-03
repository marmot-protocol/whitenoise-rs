//! NIP-50 relay search for supplementary keyword results.

use std::collections::HashMap;

use nostr_sdk::{Event, Filter, Kind, Metadata, PublicKey, RelayUrl};

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::error::Result;

/// Fetch metadata from a NIP-50 search relay.
///
/// Sends a `search` filter (NIP-50) to the relay and returns the
/// pubkey->metadata map for immediate matching. Does not cache results --
/// NIP-50 pubkeys outside the social graph never enter the graph pipeline
/// cache, and those inside it are cached by `try_fetch_network_metadata`
/// independently. Callers treat this as best-effort.
#[perf_instrument("user_search")]
pub(super) async fn try_nip50_search(
    whitenoise: &Whitenoise,
    query: &str,
    relay: &RelayUrl,
    limit: usize,
) -> Result<HashMap<PublicKey, Metadata>> {
    let filter = Filter::new()
        .kinds([Kind::Metadata])
        .search(query)
        .limit(limit);

    let events = whitenoise
        .relay_control
        .ephemeral()
        .fetch_events_from(std::slice::from_ref(relay), filter)
        .await
        .inspect_err(|e| {
            tracing::debug!(
                target: "whitenoise::user_search::nip50",
                "NIP-50 fetch from {} failed: {}",
                relay,
                e
            );
        })?;

    Ok(parse_metadata_from_events(events.iter()))
}

/// Deduplicate events by author (keep latest) and parse metadata.
///
/// Shared logic between fetch and tests. Skips events with empty or
/// malformed metadata content.
fn parse_metadata_from_events<'a, I>(events: I) -> HashMap<PublicKey, Metadata>
where
    I: IntoIterator<Item = &'a Event>,
{
    let mut latest_by_author: HashMap<PublicKey, &Event> = HashMap::new();
    for event in events {
        latest_by_author
            .entry(event.pubkey)
            .and_modify(|existing| {
                if event.created_at > existing.created_at {
                    *existing = event;
                }
            })
            .or_insert(event);
    }

    let mut found = HashMap::new();
    for (pk, event) in latest_by_author {
        if let Some(metadata) = serde_json::from_str::<Metadata>(&event.content)
            .ok()
            .filter(|m| *m != Metadata::new())
        {
            found.insert(pk, metadata);
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::{EventBuilder, Keys, Timestamp};

    fn metadata_event(keys: &Keys, metadata: &Metadata, created_at: u64) -> Event {
        EventBuilder::metadata(metadata)
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn parses_valid_metadata() {
        let keys = Keys::generate();
        let metadata = Metadata::new().name("alice");
        let event = metadata_event(&keys, &metadata, 1000);

        let result = parse_metadata_from_events(&[event]);

        assert_eq!(result.len(), 1);
        let parsed = &result[&keys.public_key()];
        assert_eq!(parsed.name, Some("alice".to_string()));
    }

    #[test]
    fn dedup_keeps_latest_event_per_author() {
        let keys = Keys::generate();
        let old = metadata_event(&keys, &Metadata::new().name("old"), 1000);
        let new = metadata_event(&keys, &Metadata::new().name("new"), 2000);

        // Pass old first, new second — should keep "new"
        let result = parse_metadata_from_events(&[old, new]);

        assert_eq!(result.len(), 1);
        assert_eq!(result[&keys.public_key()].name, Some("new".to_string()));
    }

    #[test]
    fn dedup_keeps_latest_regardless_of_order() {
        let keys = Keys::generate();
        let old = metadata_event(&keys, &Metadata::new().name("old"), 1000);
        let new = metadata_event(&keys, &Metadata::new().name("new"), 2000);

        // Pass new first, old second — should still keep "new"
        let result = parse_metadata_from_events(&[new, old]);

        assert_eq!(result.len(), 1);
        assert_eq!(result[&keys.public_key()].name, Some("new".to_string()));
    }

    #[test]
    fn skips_empty_metadata() {
        let keys = Keys::generate();
        let event = metadata_event(&keys, &Metadata::new(), 1000);

        let result = parse_metadata_from_events(&[event]);

        assert!(result.is_empty());
    }

    #[test]
    fn skips_malformed_content() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Metadata, "not json {{{")
            .custom_created_at(Timestamp::from(1000u64))
            .sign_with_keys(&keys)
            .unwrap();

        let result = parse_metadata_from_events(&[event]);

        assert!(result.is_empty());
    }

    #[test]
    fn multiple_authors_parsed_independently() {
        let alice_keys = Keys::generate();
        let bob_keys = Keys::generate();
        let alice_event = metadata_event(&alice_keys, &Metadata::new().name("alice"), 1000);
        let bob_event = metadata_event(&bob_keys, &Metadata::new().name("bob"), 2000);

        let result = parse_metadata_from_events(&[alice_event, bob_event]);

        assert_eq!(result.len(), 2);
        assert_eq!(
            result[&alice_keys.public_key()].name,
            Some("alice".to_string())
        );
        assert_eq!(result[&bob_keys.public_key()].name, Some("bob".to_string()));
    }

    #[test]
    fn empty_events_returns_empty_map() {
        let result = parse_metadata_from_events(&[] as &[Event]);
        assert!(result.is_empty());
    }
}
