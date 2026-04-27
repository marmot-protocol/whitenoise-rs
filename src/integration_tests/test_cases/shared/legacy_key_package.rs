//! Hand-crafted "legacy peer" key-package fixture for mixed-version testing.
//!
//! Bypasses `Whitenoise::publish_key_package_for_account` so the test can
//! control which capability tags (`mls_proposals`, `mls_extensions`) the
//! resulting Nostr event advertises. The underlying MLS bytes still come from
//! MDK's `encoded_key_package` — only the Nostr-event tag values are
//! overridden — so this fixture exercises the WhiteNoise-side preflight
//! softening without claiming to forge legacy MLS LeafNode bytes.

use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::groups::{KeyPackageCapabilities, MlsExtensionId, RequiredProposal};
use crate::whitenoise::key_packages::{MLS_KEY_PACKAGE_KIND_LEGACY, REQUIRED_MLS_CIPHERSUITE_TAG};
use crate::whitenoise::relays::Relay;
use nostr_sdk::prelude::*;

/// Codepoint emitted in the `mls_proposals` tag for [`RequiredProposal::SelfRemove`].
/// Mirrors `crate::whitenoise::groups::required_proposals::SELF_REMOVE_CODEPOINT`,
/// duplicated here so this fixture stays self-contained behind the
/// integration-tests feature gate.
const SELF_REMOVE_TAG: &str = "0x000a";

/// Codepoint for [`MlsExtensionId::NostrGroupData`] (`0xf2ee`).
const NOSTR_GROUP_DATA_TAG: &str = "0xf2ee";

/// Builds and publishes a "legacy peer" key-package event whose Nostr tags
/// reflect the supplied [`KeyPackageCapabilities`] verbatim.
///
/// Only the `mls_proposals` and `mls_extensions` tag values are overridden;
/// the encoded MLS bytes (event `content`) and the `mls_ciphersuite` /
/// `encoding=base64` tags are taken from MDK's normal key-package output. The
/// event is signed with the account's secret key and published directly to
/// the supplied relays via a fresh `nostr_sdk` client (not through the
/// scheduler), so it lands as a single canonical KP for `account`.
///
/// Returns the published event id.
///
/// # Limitations
///
/// MDK's `encoded_key_package` always builds the full SelfRemove capability
/// set into the underlying MLS LeafNode. This fixture cannot strip
/// capabilities at the MLS level — only at the Nostr-event level. As a
/// result, MDK's LCD logic (which reads from MLS bytes, not Nostr tags) will
/// not actually downgrade the resulting group's `RequiredCapabilities` for
/// this fixture. The fixture is sufficient to exercise WhiteNoise's
/// consumer-side preflight softening (Phase 1), but end-to-end LCD
/// verification needs real legacy LeafNode bytes recorded from an older MDK.
pub(crate) async fn publish_legacy_capability_key_package(
    context: &ScenarioContext,
    account: &crate::Account,
    relays: &[Relay],
    capabilities: KeyPackageCapabilities,
) -> Result<EventId, WhitenoiseError> {
    // Reuse MDK's normal encoded KP (content, hash_ref) and override the
    // Nostr-event capability tags below.
    let key_package_data = context
        .whitenoise
        .encoded_key_package(account, relays)
        .await?;

    // Pull the account's signer key via the public nsec export so we can
    // sign a custom event without involving the scheduler.
    let nsec = context.whitenoise.export_account_nsec(account).await?;
    let secret_key =
        SecretKey::from_bech32(&nsec).map_err(|e| WhitenoiseError::Internal(e.to_string()))?;
    let keys = Keys::new(secret_key);

    let proposal_values: Vec<&'static str> = capabilities
        .proposals
        .iter()
        .filter_map(|p| match p {
            RequiredProposal::SelfRemove => Some(SELF_REMOVE_TAG),
            // The fixture only models the codepoints currently mirrored on
            // the proposal side; `Unknown` would not have a stable wire
            // representation here, so it's silently dropped.
            RequiredProposal::Unknown => None,
        })
        .collect();

    let extension_values: Vec<&'static str> = capabilities
        .extensions
        .iter()
        .filter_map(|e| match e {
            MlsExtensionId::SelfRemove => Some(SELF_REMOVE_TAG),
            MlsExtensionId::NostrGroupData => Some(NOSTR_GROUP_DATA_TAG),
            MlsExtensionId::Unknown => None,
        })
        .collect();

    let mut tags: Vec<Tag> = vec![
        Tag::custom(
            TagKind::Custom("mls_ciphersuite".into()),
            [REQUIRED_MLS_CIPHERSUITE_TAG],
        ),
        Tag::custom(TagKind::Custom("encoding".into()), ["base64"]),
    ];
    if !extension_values.is_empty() {
        tags.push(Tag::custom(
            TagKind::Custom("mls_extensions".into()),
            extension_values.iter().copied(),
        ));
    }
    if !proposal_values.is_empty() {
        tags.push(Tag::custom(
            TagKind::Custom("mls_proposals".into()),
            proposal_values.iter().copied(),
        ));
    }
    // Mirror the relay tags MDK normally emits (one `relays` tag per URL)
    // so the published KP carries a parseable relay hint.
    for relay in relays {
        tags.push(Tag::custom(
            TagKind::Custom("relays".into()),
            [relay.url.as_str()],
        ));
    }

    let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, &key_package_data.content)
        .tags(tags)
        .sign_with_keys(&keys)
        .map_err(|e| WhitenoiseError::Internal(e.to_string()))?;
    let event_id = event.id;

    let relay_urls: Vec<&str> = relays.iter().map(|r| r.url.as_str()).collect();
    let client = create_test_client(&relay_urls, keys).await?;
    client.send_event(&event).await?;
    client.disconnect().await;

    context
        .whitenoise
        .track_published_key_package_for_testing(
            &account.pubkey,
            &key_package_data.hash_ref,
            &event_id.to_hex(),
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
        )
        .await?;

    tracing::debug!(
        "Published legacy-capability key package {} for account {} (proposals={:?}, extensions={:?})",
        event_id.to_hex(),
        account.pubkey.to_hex(),
        capabilities.proposals,
        capabilities.extensions,
    );

    Ok(event_id)
}
