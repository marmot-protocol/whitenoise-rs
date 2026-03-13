use std::time::Duration;

use nostr_sdk::prelude::*;

use super::error::{Result, WhitenoiseError};

const ZAPSTORE_RELAY: &str = "wss://relay.zapstore.dev";
const ZAPSTORE_APP_PUBKEY: &str =
    "75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7";
const ZAPSTORE_APP_IDENTIFIER: &str = "org.parres.whitenoise";
const FETCH_TIMEOUT_SECS: u64 = 10;

/// Fetches the latest version string published on Zapstore for White Noise.
///
/// The kind-32267 Software Application event is the source of truth: its `a`
/// tag points at the current release in the form
/// `30063:<pubkey>:<identifier>@<version>`.  We fetch that single addressable
/// event and extract the version from it, avoiding the race condition that
/// arises from scanning kind-30063 release events by recency.
///
/// Returns `None` when the app event is absent or carries no valid `a` tag.
pub async fn fetch_latest_zapstore_version() -> Result<Option<String>> {
    let pubkey = PublicKey::from_hex(ZAPSTORE_APP_PUBKEY).map_err(WhitenoiseError::from)?;

    // Fetch the single addressable kind-32267 Software Application event.
    // Using `d` tag + author gives us exactly one canonical event.
    let filter = Filter::new()
        .kind(Kind::Custom(32267))
        .author(pubkey)
        .identifier(ZAPSTORE_APP_IDENTIFIER);

    let client = Client::default();
    client
        .add_relay(ZAPSTORE_RELAY)
        .await
        .map_err(WhitenoiseError::from)?;
    client.connect().await;

    let events = client
        .fetch_events(filter, Duration::from_secs(FETCH_TIMEOUT_SECS))
        .await
        .map_err(WhitenoiseError::from)?;

    client.disconnect().await;

    let event = match events.first() {
        Some(e) => e,
        None => return Ok(None),
    };

    Ok(extract_version_from_app_event(event))
}

/// Extracts the current release version from a kind-32267 Software Application event.
///
/// The `a` tag on the app event points at the current release:
/// `30063:<pubkey>:<identifier>@<version>`
///
/// Returns `None` if no valid `a` tag referencing a kind-30063 release for
/// this app is present, or if the version suffix is missing.
fn extract_version_from_app_event(event: &Event) -> Option<String> {
    let expected_prefix = format!("30063:{}:{}@", ZAPSTORE_APP_PUBKEY, ZAPSTORE_APP_IDENTIFIER);

    event.tags.iter().find_map(|tag| {
        let vec = tag.as_slice();
        if vec.first().map(|s| s.as_str()) != Some("a") {
            return None;
        }
        let value = vec.get(1).map(|s| s.as_str())?;
        value
            .strip_prefix(expected_prefix.as_str())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tag(values: &[&str]) -> Tag {
        Tag::parse(values.iter().map(|s| s.to_string()).collect::<Vec<_>>()).expect("valid tag")
    }

    fn make_event_with_tags(tags: Vec<Tag>) -> Event {
        let keys = Keys::generate();
        EventBuilder::new(Kind::Custom(32267), "")
            .tags(tags)
            .sign_with_keys(&keys)
            .expect("signing succeeds")
    }

    #[test]
    fn test_version_extracted_from_valid_a_tag() {
        let tag = make_tag(&[
            "a",
            "30063:75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7:org.parres.whitenoise@2026.3.5",
        ]);
        let event = make_event_with_tags(vec![tag]);
        assert_eq!(
            extract_version_from_app_event(&event),
            Some("2026.3.5".to_string())
        );
    }

    #[test]
    fn test_no_a_tag_returns_none() {
        let tag = make_tag(&["d", "org.parres.whitenoise"]);
        let event = make_event_with_tags(vec![tag]);
        assert_eq!(extract_version_from_app_event(&event), None);
    }

    #[test]
    fn test_a_tag_wrong_kind_returns_none() {
        // Points at a kind-32267 instead of kind-30063 — should be ignored.
        let tag = make_tag(&[
            "a",
            "32267:75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7:org.parres.whitenoise",
        ]);
        let event = make_event_with_tags(vec![tag]);
        assert_eq!(extract_version_from_app_event(&event), None);
    }

    #[test]
    fn test_a_tag_wrong_pubkey_returns_none() {
        let tag = make_tag(&[
            "a",
            "30063:0000000000000000000000000000000000000000000000000000000000000001:org.parres.whitenoise@2026.3.5",
        ]);
        let event = make_event_with_tags(vec![tag]);
        assert_eq!(extract_version_from_app_event(&event), None);
    }

    #[test]
    fn test_a_tag_wrong_identifier_returns_none() {
        let tag = make_tag(&[
            "a",
            "30063:75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7:org.someone.else@2026.3.5",
        ]);
        let event = make_event_with_tags(vec![tag]);
        assert_eq!(extract_version_from_app_event(&event), None);
    }

    #[test]
    fn test_a_tag_missing_version_suffix_returns_none() {
        // Has the right prefix but no @version — empty version after strip
        let tag = make_tag(&[
            "a",
            "30063:75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7:org.parres.whitenoise@",
        ]);
        let event = make_event_with_tags(vec![tag]);
        assert_eq!(extract_version_from_app_event(&event), None);
    }

    #[test]
    fn test_first_matching_a_tag_wins() {
        // When there are multiple a tags, the first valid one is used.
        let tag1 = make_tag(&["d", "org.parres.whitenoise"]);
        let tag2 = make_tag(&[
            "a",
            "30063:75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7:org.parres.whitenoise@2026.3.5",
        ]);
        let event = make_event_with_tags(vec![tag1, tag2]);
        assert_eq!(
            extract_version_from_app_event(&event),
            Some("2026.3.5".to_string())
        );
    }
}
