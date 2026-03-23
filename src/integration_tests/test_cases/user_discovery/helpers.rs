use std::time::Duration;

use nostr_sdk::{Client, EventId, Filter, Kind, PublicKey};

use crate::WhitenoiseError;
use crate::integration_tests::core::retry;

pub async fn wait_for_relay_list_indexed(
    client: &Client,
    pubkey: PublicKey,
) -> Result<(), WhitenoiseError> {
    retry(
        30,
        Duration::from_millis(100),
        || async {
            let events = client
                .fetch_events(
                    Filter::new().author(pubkey).kind(Kind::RelayList),
                    Duration::from_secs(1),
                )
                .await?;

            if events.iter().next().is_some() {
                Ok(())
            } else {
                Err(WhitenoiseError::Other(anyhow::anyhow!(
                    "Relay-list event is not yet queryable for {}",
                    pubkey
                )))
            }
        },
        "wait for relay-list event to be queryable",
    )
    .await
}

/// Kind-0 is replaceable, so older events are not guaranteed to remain
/// queryable by exact ID once the relay has reconciled them. Poll the same
/// author+kind lookup shape used by targeted discovery instead.
pub async fn wait_for_latest_metadata_event(
    client: &Client,
    pubkey: PublicKey,
    expected_event_id: EventId,
    description: &str,
) -> Result<(), WhitenoiseError> {
    retry(
        30,
        Duration::from_millis(100),
        || async {
            let events = client
                .fetch_events(
                    Filter::new().author(pubkey).kind(Kind::Metadata),
                    Duration::from_secs(1),
                )
                .await?;

            if events.iter().any(|event| event.id == expected_event_id) {
                Ok(())
            } else {
                Err(WhitenoiseError::Other(anyhow::anyhow!(
                    "Latest metadata query does not yet return expected event {}",
                    expected_event_id.to_hex()
                )))
            }
        },
        description,
    )
    .await
}
