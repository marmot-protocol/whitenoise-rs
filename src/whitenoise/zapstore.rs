use std::time::Duration;

use nostr_sdk::prelude::*;

use super::error::{Result, WhitenoiseError};

const ZAPSTORE_RELAY: &str = "wss://relay.zapstore.dev";
const ZAPSTORE_APP_PUBKEY: &str =
    "75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7";
const ZAPSTORE_APP_IDENTIFIER: &str = "org.parres.whitenoise";
const FETCH_TIMEOUT_SECS: u64 = 10;
const ZAPSTORE_A_TAG_PREFIX: &str = concat!(
    "30063:",
    "75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7",
    ":",
    "org.parres.whitenoise",
    "@",
);

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
    tracing::debug!(
        target: "whitenoise::zapstore",
        relay = ZAPSTORE_RELAY,
        "fetching latest zapstore version"
    );

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

    let fetch_result = client
        .fetch_events(filter, Duration::from_secs(FETCH_TIMEOUT_SECS))
        .await;

    client.disconnect().await;

    let events = fetch_result.map_err(|e| {
        tracing::warn!(
            target: "whitenoise::zapstore",
            error = %e,
            "failed to fetch app event from relay"
        );
        WhitenoiseError::from(e)
    })?;

    let event = match events.first() {
        Some(e) => e,
        None => {
            tracing::warn!(
                target: "whitenoise::zapstore",
                relay = ZAPSTORE_RELAY,
                "no app event found on relay"
            );
            return Ok(None);
        }
    };

    Ok(extract_version_from_app_event(event))
}

/// Extracts the current release version from a kind-32267 Software Application event.
///
/// The `a` tag on the app event points at the current release:
/// `30063:<pubkey>:<identifier>@<version>`
///
/// Returns `None` if the event fails any of the expected identity checks
/// (kind, pubkey, `d` tag), if no valid `a` tag referencing a kind-30063
/// release for this app is present, or if the version suffix is missing.
fn extract_version_from_app_event(event: &Event) -> Option<String> {
    // Guard: must be a kind-32267 Software Application event.
    if event.kind != Kind::Custom(32267) {
        return None;
    }

    // Guard: must be authored by the known Zapstore pubkey.
    let expected_pubkey = PublicKey::from_hex(ZAPSTORE_APP_PUBKEY).ok()?;
    if event.pubkey != expected_pubkey {
        return None;
    }

    // Guard: the `d` tag must match our app identifier.
    if !has_matching_d_tag(&event.tags) {
        return None;
    }

    event
        .tags
        .iter()
        .find_map(|tag| extract_version_from_a_tag(tag, ZAPSTORE_A_TAG_PREFIX))
}

/// Returns `true` if `tags` contains a `d` tag whose value equals `ZAPSTORE_APP_IDENTIFIER`.
fn has_matching_d_tag(tags: &Tags) -> bool {
    tags.iter().any(|tag| {
        let vec = tag.as_slice();
        vec.first().map(|s| s.as_str()) == Some("d")
            && vec.get(1).map(|s| s.as_str()) == Some(ZAPSTORE_APP_IDENTIFIER)
    })
}

/// Extracts the version string from a single `a` tag value given an expected prefix.
///
/// Returns `None` if the tag is not an `a` tag, its value does not start with
/// `expected_prefix`, or the version suffix is empty.
fn extract_version_from_a_tag(tag: &Tag, expected_prefix: &str) -> Option<String> {
    let vec = tag.as_slice();
    if vec.first().map(|s| s.as_str()) != Some("a") {
        return None;
    }
    let value = vec.get(1).map(|s| s.as_str())?;
    value
        .strip_prefix(expected_prefix)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tag(values: &[&str]) -> Tag {
        Tag::parse(values.iter().copied()).expect("valid tag")
    }

    /// Builds a kind-32267 event signed by a random key.
    ///
    /// Used only for event-level guard tests (kind, pubkey, d tag).  Tag-parsing
    /// tests call `extract_version_from_a_tag` directly to avoid coupling them
    /// to the signing key.
    fn make_event_with_tags(tags: Vec<Tag>) -> Event {
        let keys = Keys::generate();
        EventBuilder::new(Kind::Custom(32267), "")
            .tags(tags)
            .sign_with_keys(&keys)
            .expect("signing succeeds")
    }

    fn valid_prefix() -> &'static str {
        ZAPSTORE_A_TAG_PREFIX
    }

    // --- extract_version_from_a_tag: tag-parsing unit tests ---

    #[test]
    fn test_version_extracted_from_valid_a_tag() {
        let prefix = valid_prefix();
        let tag = make_tag(&[
            "a",
            "30063:75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7:org.parres.whitenoise@2026.3.5",
        ]);
        assert_eq!(
            extract_version_from_a_tag(&tag, prefix),
            Some("2026.3.5".to_string())
        );
    }

    #[test]
    fn test_non_a_tag_returns_none() {
        let prefix = valid_prefix();
        let tag = make_tag(&["d", "org.parres.whitenoise"]);
        assert_eq!(extract_version_from_a_tag(&tag, prefix), None);
    }

    #[test]
    fn test_a_tag_wrong_kind_returns_none() {
        // Points at kind-32267 instead of kind-30063 — prefix won't match.
        let prefix = valid_prefix();
        let tag = make_tag(&[
            "a",
            "32267:75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7:org.parres.whitenoise",
        ]);
        assert_eq!(extract_version_from_a_tag(&tag, prefix), None);
    }

    #[test]
    fn test_a_tag_wrong_pubkey_returns_none() {
        let prefix = valid_prefix();
        let tag = make_tag(&[
            "a",
            "30063:0000000000000000000000000000000000000000000000000000000000000001:org.parres.whitenoise@2026.3.5",
        ]);
        assert_eq!(extract_version_from_a_tag(&tag, prefix), None);
    }

    #[test]
    fn test_a_tag_wrong_identifier_returns_none() {
        let prefix = valid_prefix();
        let tag = make_tag(&[
            "a",
            "30063:75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7:org.someone.else@2026.3.5",
        ]);
        assert_eq!(extract_version_from_a_tag(&tag, prefix), None);
    }

    #[test]
    fn test_a_tag_missing_version_suffix_returns_none() {
        // Has the right prefix but no version after "@" — empty string is rejected.
        let prefix = valid_prefix();
        let tag = make_tag(&[
            "a",
            "30063:75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7:org.parres.whitenoise@",
        ]);
        assert_eq!(extract_version_from_a_tag(&tag, prefix), None);
    }

    #[test]
    fn test_first_matching_a_tag_wins() {
        // Two valid "a" tags with different versions; the first one must win.
        let prefix = valid_prefix();
        let tag1 = make_tag(&[
            "a",
            "30063:75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7:org.parres.whitenoise@2026.3.5",
        ]);
        let tag2 = make_tag(&[
            "a",
            "30063:75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7:org.parres.whitenoise@2026.1.0",
        ]);
        // Simulate what extract_version_from_app_event does over a tag list.
        let tags = vec![tag1, tag2];
        let result = tags
            .iter()
            .find_map(|t| extract_version_from_a_tag(t, prefix));
        assert_eq!(result, Some("2026.3.5".to_string()));
    }

    // --- extract_version_from_app_event: event-level guard tests ---

    #[test]
    fn test_wrong_event_kind_returns_none() {
        // A kind-30063 event (not 32267) must be rejected by the kind guard.
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(30063), "")
            .sign_with_keys(&keys)
            .expect("signing succeeds");
        assert_eq!(extract_version_from_app_event(&event), None);
    }

    #[test]
    fn test_wrong_pubkey_returns_none() {
        // An event from a random key must be rejected by the pubkey guard.
        let event = make_event_with_tags(vec![]);
        assert_eq!(extract_version_from_app_event(&event), None);
    }

    #[test]
    fn test_has_matching_d_tag() {
        let correct = make_tag(&["d", ZAPSTORE_APP_IDENTIFIER]);
        let wrong = make_tag(&["d", "org.someone.else"]);
        let unrelated = make_tag(&["e", ZAPSTORE_APP_IDENTIFIER]);

        assert!(has_matching_d_tag(&Tags::from_list(vec![correct])));
        assert!(!has_matching_d_tag(&Tags::from_list(vec![wrong])));
        assert!(!has_matching_d_tag(&Tags::from_list(vec![unrelated])));
        assert!(!has_matching_d_tag(&Tags::from_list(vec![])));
    }
}
