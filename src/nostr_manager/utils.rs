use std::{collections::HashSet, time::Duration};

use nostr_sdk::prelude::*;

use crate::nostr_manager::NostrManager;

/// Maximum allowed skew for event timestamps in the future (1 hour)
pub(crate) const MAX_FUTURE_SKEW: Duration = Duration::from_secs(60 * 60);

/// Additional lookback buffer for giftwrap subscriptions to account for NIP-59 backdating
/// NIP-59 recommends tweaking timestamps (usually to the past) to prevent timing analysis
pub(crate) const GIFTWRAP_LOOKBACK_BUFFER: Duration = Duration::from_secs(7 * 24 * 60 * 60); // 7 days

/// Checks if an event's timestamp is not too far in the future
pub(crate) fn is_event_timestamp_valid(event: &Event) -> bool {
    let cutoff = Timestamp::now() + MAX_FUTURE_SKEW;
    event.created_at <= cutoff
}

/// Adjusts a since timestamp for giftwrap subscriptions by subtracting the lookback buffer
/// to account for NIP-59 backdated timestamps
pub(crate) fn adjust_since_for_giftwrap(since: Option<Timestamp>) -> Option<Timestamp> {
    since.map(|ts| {
        let secs = ts.as_secs();
        let lookback = GIFTWRAP_LOOKBACK_BUFFER.as_secs();
        let adjusted = secs.saturating_sub(lookback);
        Timestamp::from(adjusted)
    })
}

/// Caps an event timestamp to the current time to prevent future timestamp corruption
pub(crate) fn cap_timestamp_to_now(event_timestamp: Timestamp) -> Timestamp {
    let now = Timestamp::now();
    if event_timestamp > now {
        now
    } else {
        event_timestamp
    }
}

impl NostrManager {
    /// Extracts public keys from an event's tags.
    pub(crate) fn pubkeys_from_event(event: &Event) -> Vec<PublicKey> {
        event
            .tags
            .iter()
            .filter(|tag| {
                tag.kind() == TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::P))
            })
            .filter_map(|tag| tag.content().and_then(|c| c.parse::<PublicKey>().ok()))
            .collect()
    }

    /// Extracts relay URLs from an event's tags.
    pub(crate) fn relay_urls_from_event(event: &Event) -> HashSet<RelayUrl> {
        event
            .tags
            .iter()
            .filter(|tag| Self::is_relay_list_tag_for_event_kind(tag, event.kind))
            .filter_map(|tag| {
                tag.content()
                    .and_then(|content| RelayUrl::parse(content).ok())
            })
            .collect()
    }

    /// Determines if a tag is relevant for the given relay list event kind.
    /// Different relay list kinds use different tag types:
    /// - Kind::RelayList (10002) uses "r" tags (TagKind::SingleLetter)
    /// - Kind::InboxRelays (10050) and Kind::MlsKeyPackageRelays (10051) use "relay" tags (TagKind::Relay)
    pub(crate) fn is_relay_list_tag_for_event_kind(tag: &Tag, kind: Kind) -> bool {
        match kind {
            Kind::RelayList => Self::is_r_tag(tag),
            Kind::InboxRelays | Kind::MlsKeyPackageRelays => Self::is_relay_tag(tag),
            _ => Self::is_relay_tag(tag) || Self::is_r_tag(tag), // backward compatibility
        }
    }

    /// Checks if a tag is an "r" tag.
    pub(crate) fn is_r_tag(tag: &Tag) -> bool {
        tag.kind() == TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::R))
    }

    /// Checks if a tag is a "relay" tag.
    pub(crate) fn is_relay_tag(tag: &Tag) -> bool {
        tag.kind() == TagKind::Relay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_relay_urls_from_event_relay_list() {
        use nostr_sdk::prelude::*;

        // Test Kind::RelayList (10002) with "r" tags
        let keys = Keys::generate();

        let r_tags = vec![
            Tag::reference("wss://relay1.example.com"),
            Tag::reference("wss://relay2.example.com"),
            // Add a relay tag that should be ignored for RelayList
            Tag::custom(TagKind::Relay, ["wss://should-be-ignored.com"]),
        ];

        let event = EventBuilder::new(Kind::RelayList, "")
            .tags(r_tags)
            .sign(&keys)
            .await
            .unwrap();

        let parsed_relays = NostrManager::relay_urls_from_event(&event);

        assert_eq!(parsed_relays.len(), 2);
        assert!(parsed_relays.contains(&RelayUrl::parse("wss://relay1.example.com").unwrap()));
        assert!(parsed_relays.contains(&RelayUrl::parse("wss://relay2.example.com").unwrap()));
        assert!(!parsed_relays.contains(&RelayUrl::parse("wss://should-be-ignored.com").unwrap()));
    }

    #[tokio::test]
    async fn test_relay_urls_from_event_inbox_relays() {
        use nostr_sdk::prelude::*;

        // Test Kind::InboxRelays (10050) with "relay" tags
        let keys = Keys::generate();

        let relay_tags = vec![
            Tag::custom(TagKind::Relay, ["wss://inbox1.example.com"]),
            Tag::custom(TagKind::Relay, ["wss://inbox2.example.com"]),
            // Add an "r" tag that should be ignored for InboxRelays
            Tag::reference("wss://should-be-ignored.com"),
        ];

        let event = EventBuilder::new(Kind::InboxRelays, "")
            .tags(relay_tags)
            .sign(&keys)
            .await
            .unwrap();

        let parsed_relays = NostrManager::relay_urls_from_event(&event);

        assert_eq!(parsed_relays.len(), 2);
        assert!(parsed_relays.contains(&RelayUrl::parse("wss://inbox1.example.com").unwrap()));
        assert!(parsed_relays.contains(&RelayUrl::parse("wss://inbox2.example.com").unwrap()));
        assert!(!parsed_relays.contains(&RelayUrl::parse("wss://should-be-ignored.com").unwrap()));
    }

    #[tokio::test]
    async fn test_relay_urls_from_event_key_package_relays() {
        use nostr_sdk::prelude::*;

        // Test Kind::MlsKeyPackageRelays (10051) with "relay" tags
        let keys = Keys::generate();

        let relay_tags = vec![
            Tag::custom(TagKind::Relay, ["wss://keypackage1.example.com"]),
            Tag::custom(TagKind::Relay, ["wss://keypackage2.example.com"]),
            // Add an "r" tag that should be ignored for MlsKeyPackageRelays
            Tag::reference("wss://should-be-ignored.com"),
        ];

        let event = EventBuilder::new(Kind::MlsKeyPackageRelays, "")
            .tags(relay_tags)
            .sign(&keys)
            .await
            .unwrap();

        let parsed_relays = NostrManager::relay_urls_from_event(&event);

        assert_eq!(parsed_relays.len(), 2);
        assert!(parsed_relays.contains(&RelayUrl::parse("wss://keypackage1.example.com").unwrap()));
        assert!(parsed_relays.contains(&RelayUrl::parse("wss://keypackage2.example.com").unwrap()));
        assert!(!parsed_relays.contains(&RelayUrl::parse("wss://should-be-ignored.com").unwrap()));
    }

    #[tokio::test]
    async fn test_relay_urls_from_event_unknown_kind_backward_compatibility() {
        use nostr_sdk::prelude::*;

        // Test unknown kind with both "r" and "relay" tags (backward compatibility)
        let keys = Keys::generate();

        let mixed_tags = vec![
            Tag::reference("wss://r-tag-relay.example.com"),
            Tag::custom(TagKind::Relay, ["wss://relay-tag-relay.example.com"]),
        ];

        let event = EventBuilder::new(Kind::Custom(9999), "")
            .tags(mixed_tags)
            .sign(&keys)
            .await
            .unwrap();

        let parsed_relays = NostrManager::relay_urls_from_event(&event);

        assert_eq!(parsed_relays.len(), 2);
        assert!(parsed_relays.contains(&RelayUrl::parse("wss://r-tag-relay.example.com").unwrap()));
        assert!(
            parsed_relays.contains(&RelayUrl::parse("wss://relay-tag-relay.example.com").unwrap())
        );
    }

    #[tokio::test]
    async fn test_relay_urls_from_event_invalid_urls_filtered() {
        use nostr_sdk::prelude::*;

        // Test that invalid URLs are filtered out
        let keys = Keys::generate();

        let tags = vec![
            Tag::reference("wss://valid-relay.example.com"),
            Tag::reference("not a valid url"),
            Tag::reference("wss://another-valid.example.com"),
        ];

        let event = EventBuilder::new(Kind::RelayList, "")
            .tags(tags)
            .sign(&keys)
            .await
            .unwrap();

        let parsed_relays = NostrManager::relay_urls_from_event(&event);

        assert_eq!(parsed_relays.len(), 2);
        assert!(parsed_relays.contains(&RelayUrl::parse("wss://valid-relay.example.com").unwrap()));
        assert!(
            parsed_relays.contains(&RelayUrl::parse("wss://another-valid.example.com").unwrap())
        );
    }

    #[tokio::test]
    async fn test_relay_urls_from_event_empty_tags() {
        use nostr_sdk::prelude::*;

        // Test event with no relay tags
        let keys = Keys::generate();

        let tags = vec![
            Tag::custom(TagKind::Custom("alt".into()), ["Some description"]),
            Tag::custom(TagKind::Custom("d".into()), ["identifier"]),
        ];

        let event = EventBuilder::new(Kind::RelayList, "")
            .tags(tags)
            .sign(&keys)
            .await
            .unwrap();

        let parsed_relays = NostrManager::relay_urls_from_event(&event);
        assert!(parsed_relays.is_empty());
    }

    // Existing tests below

    #[tokio::test]
    async fn test_pubkeys_from_event_with_valid_p_tags() {
        // Create test public keys
        let signer_keys = Keys::generate();
        let keys1 = Keys::generate();
        let keys2 = Keys::generate();
        let pubkey1 = keys1.public_key();
        let pubkey2 = keys2.public_key();

        // Create an event with p tags containing valid public keys
        let event = EventBuilder::text_note("test content")
            .tags([Tag::public_key(pubkey1), Tag::public_key(pubkey2)])
            .sign(&signer_keys)
            .await
            .unwrap();

        let result = NostrManager::pubkeys_from_event(&event);

        assert_eq!(result.len(), 2);
        assert!(result.contains(&pubkey1));
        assert!(result.contains(&pubkey2));
    }

    #[tokio::test]
    async fn test_pubkeys_from_event_with_empty_event() {
        // Create an event with no tags
        let keys = Keys::generate();
        let event = EventBuilder::text_note("test content")
            .sign(&keys)
            .await
            .unwrap();

        let result = NostrManager::pubkeys_from_event(&event);

        assert_eq!(result.len(), 0);
    }

    #[tokio::test]
    async fn test_pubkeys_from_event_with_non_p_tags() {
        let keys = Keys::generate();

        // Create an event with various non-p tags
        let event = EventBuilder::text_note("test content")
            .tags([
                Tag::hashtag("bitcoin"),
                Tag::identifier("test-id"),
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E)),
                    vec!["event-id"],
                ),
            ])
            .sign(&keys)
            .await
            .unwrap();

        let result = NostrManager::pubkeys_from_event(&event);

        assert_eq!(result.len(), 0);
    }

    #[tokio::test]
    async fn test_pubkeys_from_event_with_invalid_pubkey_content() {
        let keys = Keys::generate();

        // Create an event with p tags containing invalid public key content
        let event = EventBuilder::text_note("test content")
            .tags([
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::P)),
                    vec!["invalid-pubkey"],
                ),
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::P)),
                    vec!["also-invalid"],
                ),
            ])
            .sign(&keys)
            .await
            .unwrap();

        let result = NostrManager::pubkeys_from_event(&event);

        assert_eq!(result.len(), 0);
    }

    #[tokio::test]
    async fn test_pubkeys_from_event_with_mixed_valid_and_invalid() {
        let keys1 = Keys::generate();
        let keys2 = Keys::generate();
        let valid_pubkey = keys2.public_key();

        // Create an event with both valid and invalid p tags
        let event = EventBuilder::text_note("test content")
            .tags([
                Tag::public_key(valid_pubkey),
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::P)),
                    vec!["invalid-pubkey"],
                ),
                Tag::hashtag("bitcoin"), // Non-p tag
            ])
            .sign(&keys1)
            .await
            .unwrap();

        let result = NostrManager::pubkeys_from_event(&event);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], valid_pubkey);
    }

    #[tokio::test]
    async fn test_pubkeys_from_event_with_duplicate_pubkeys() {
        let keys1 = Keys::generate();
        let keys2 = Keys::generate();
        let pubkey = keys2.public_key();

        // Create an event with duplicate p tags
        let event = EventBuilder::text_note("test content")
            .tags([Tag::public_key(pubkey), Tag::public_key(pubkey)])
            .sign(&keys1)
            .await
            .unwrap();

        let result = NostrManager::pubkeys_from_event(&event);

        // Should contain duplicates as the method doesn't deduplicate
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], pubkey);
        assert_eq!(result[1], pubkey);
    }

    #[tokio::test]
    async fn test_pubkeys_from_event_with_empty_p_tag_content() {
        let keys = Keys::generate();

        // Create an event with p tag but no content
        let event = EventBuilder::text_note("test content")
            .tags([Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::P)),
                Vec::<String>::new(),
            )])
            .sign(&keys)
            .await
            .unwrap();

        let result = NostrManager::pubkeys_from_event(&event);

        assert_eq!(result.len(), 0);
    }

    // Tests for timestamp utility functions

    #[test]
    fn test_is_event_timestamp_valid_with_current_time() {
        use nostr_sdk::prelude::*;

        let keys = Keys::generate();
        let event = EventBuilder::text_note("test")
            .custom_created_at(Timestamp::now())
            .sign_with_keys(&keys)
            .unwrap();

        assert!(is_event_timestamp_valid(&event));
    }

    #[test]
    fn test_is_event_timestamp_valid_with_past_time() {
        use nostr_sdk::prelude::*;

        let keys = Keys::generate();
        let past_timestamp = Timestamp::now() - Duration::from_secs(3600); // 1 hour ago
        let event = EventBuilder::text_note("test")
            .custom_created_at(past_timestamp)
            .sign_with_keys(&keys)
            .unwrap();

        assert!(is_event_timestamp_valid(&event));
    }

    #[test]
    fn test_is_event_timestamp_valid_with_near_future() {
        use nostr_sdk::prelude::*;

        let keys = Keys::generate();
        let near_future = Timestamp::now() + Duration::from_secs(1800); // 30 minutes in future
        let event = EventBuilder::text_note("test")
            .custom_created_at(near_future)
            .sign_with_keys(&keys)
            .unwrap();

        assert!(is_event_timestamp_valid(&event));
    }

    #[test]
    fn test_is_event_timestamp_valid_with_far_future() {
        use nostr_sdk::prelude::*;

        let keys = Keys::generate();
        let far_future = Timestamp::now() + Duration::from_secs(7200); // 2 hours in future (exceeds 1 hour limit)
        let event = EventBuilder::text_note("test")
            .custom_created_at(far_future)
            .sign_with_keys(&keys)
            .unwrap();

        assert!(!is_event_timestamp_valid(&event));
    }

    #[test]
    fn test_is_event_timestamp_valid_at_boundary() {
        use nostr_sdk::prelude::*;

        let keys = Keys::generate();
        let boundary_timestamp = Timestamp::now() + MAX_FUTURE_SKEW; // Exactly at the boundary
        let event = EventBuilder::text_note("test")
            .custom_created_at(boundary_timestamp)
            .sign_with_keys(&keys)
            .unwrap();

        assert!(is_event_timestamp_valid(&event));
    }

    #[test]
    fn test_adjust_since_for_giftwrap_with_none() {
        let result = adjust_since_for_giftwrap(None);
        assert!(result.is_none());
    }

    #[test]
    fn test_adjust_since_for_giftwrap_with_some() {
        let original_timestamp = Timestamp::now();
        let result = adjust_since_for_giftwrap(Some(original_timestamp));

        assert!(result.is_some());
        let adjusted = result.unwrap();

        // Should be exactly GIFTWRAP_LOOKBACK_BUFFER earlier
        assert_eq!(adjusted, original_timestamp - GIFTWRAP_LOOKBACK_BUFFER);
    }

    #[test]
    fn test_adjust_since_for_giftwrap_with_old_timestamp() {
        // Test with a timestamp that's already old
        let old_timestamp = Timestamp::now() - Duration::from_secs(30 * 24 * 60 * 60); // 30 days ago
        let result = adjust_since_for_giftwrap(Some(old_timestamp));

        assert!(result.is_some());
        let adjusted = result.unwrap();

        // Should be GIFTWRAP_LOOKBACK_BUFFER earlier than the old timestamp
        assert_eq!(adjusted, old_timestamp - GIFTWRAP_LOOKBACK_BUFFER);

        // The adjusted timestamp should be even older
        assert!(adjusted < old_timestamp);
    }

    #[test]
    fn test_cap_timestamp_to_now_with_past() {
        let past_timestamp = Timestamp::now() - Duration::from_secs(3600); // 1 hour ago
        let result = cap_timestamp_to_now(past_timestamp);

        // Past timestamp should be returned unchanged
        assert_eq!(result, past_timestamp);
    }

    #[test]
    fn test_cap_timestamp_to_now_with_current() {
        let now = Timestamp::now();
        let result = cap_timestamp_to_now(now);

        // Current timestamp should be returned (may be slightly different due to timing)
        // Allow for small timing differences
        let diff = if result >= now {
            result.as_secs() - now.as_secs()
        } else {
            now.as_secs() - result.as_secs()
        };
        assert!(diff <= 1); // Allow 1 second difference
    }

    #[test]
    fn test_cap_timestamp_to_now_with_future() {
        let future_timestamp = Timestamp::now() + Duration::from_secs(3600); // 1 hour in future
        let before_call = Timestamp::now();
        let result = cap_timestamp_to_now(future_timestamp);
        let after_call = Timestamp::now();

        // Future timestamp should be capped to now (somewhere between before_call and after_call)
        assert!(result >= before_call);
        assert!(result <= after_call);
        assert!(result < future_timestamp);
    }

    #[test]
    fn test_constants_are_reasonable() {
        // Test that the constants have reasonable values
        assert_eq!(MAX_FUTURE_SKEW, Duration::from_secs(60 * 60)); // 1 hour
        assert_eq!(
            GIFTWRAP_LOOKBACK_BUFFER,
            Duration::from_secs(7 * 24 * 60 * 60)
        ); // 7 days
    }
}
