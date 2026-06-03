use std::collections::HashSet;

use base64ct::{Base64, Encoding};
use cgka_traits::engine::KeyPackage;
use cgka_traits::{MessageId, TransportEndpoint};
use nostr_sdk::prelude::*;
use transport_nostr_adapter::NostrKeyPackagePublication;
use transport_nostr_peeler::NostrTransportEvent;

use super::app_components::whitenoise_supported_app_component_tags;
use super::session::MarmotSession;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::key_packages::REQUIRED_MLS_CIPHERSUITE_TAG;

/// Darkmatter account-identity proof extension (`marmot.account-identity-proof.v1`).
pub(crate) const ACCOUNT_IDENTITY_PROOF_EXTENSION_TAG: &str = "0xf2f1";

pub(crate) const MLS_PROPOSALS_TAG_KEY: &str = "mls_proposals";
pub(crate) const REQUIRED_MLS_PROPOSAL_TAGS: [&str; 1] = ["0x000a"];

const KEY_PACKAGE_REF_TAG_KEY: &str = "i";
const MLS_PROTOCOL_VERSION_TAG_KEY: &str = "mls_protocol_version";
const APP_COMPONENTS_TAG_KEY: &str = "app_components";

const LAST_RESORT_EXTENSION_TAG: &str = "0x0006";
const SELF_REMOVE_EXTENSION_TAG: &str = "0x000a";
const APP_DATA_UPDATE_PROPOSAL_TAG: &str = "0x0008";
const SELF_REMOVE_PROPOSAL_TAG: &str = "0x000a";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarmotKeyPackageEventData {
    pub content: String,
    pub tags: Vec<Tag>,
    pub d_tag: String,
    pub key_package_ref: Vec<u8>,
    pub key_package_ref_hex: String,
    pub app_components: Vec<String>,
}

impl MarmotSession {
    pub(crate) async fn fresh_key_package_event(
        &mut self,
        d_tag: String,
        relay_urls: &[RelayUrl],
    ) -> Result<MarmotKeyPackageEventData> {
        let key_package = self.fresh_key_package().await?;
        self.key_package_event(key_package, d_tag, relay_urls)
    }

    pub(crate) fn key_package_event(
        &self,
        key_package: KeyPackage,
        d_tag: String,
        relay_urls: &[RelayUrl],
    ) -> Result<MarmotKeyPackageEventData> {
        let metadata = cgka_engine::key_package_metadata(&key_package)?;
        let key_package_ref = hex::decode(&metadata.key_package_ref_hex).map_err(|err| {
            WhitenoiseError::InvalidInput(format!(
                "Darkmatter key package ref is not valid hex: {err}"
            ))
        })?;
        let app_components = whitenoise_supported_app_component_tags();
        let publication = NostrKeyPackagePublication {
            account_id: self.self_id(),
            key_package,
            key_package_slot_id: d_tag.clone(),
            key_package_ref: metadata.key_package_ref_hex.clone(),
            mls_ciphersuite: REQUIRED_MLS_CIPHERSUITE_TAG.to_string(),
            mls_extensions: vec![
                LAST_RESORT_EXTENSION_TAG.to_string(),
                ACCOUNT_IDENTITY_PROOF_EXTENSION_TAG.to_string(),
                SELF_REMOVE_EXTENSION_TAG.to_string(),
            ],
            mls_proposals: vec![
                APP_DATA_UPDATE_PROPOSAL_TAG.to_string(),
                SELF_REMOVE_PROPOSAL_TAG.to_string(),
            ],
            app_components: app_components.clone(),
            publish_endpoints: relay_urls
                .iter()
                .map(|url| TransportEndpoint(url.as_str().to_string()))
                .collect(),
        };
        let transport_event = publication.to_event().map_err(|err| {
            WhitenoiseError::InvalidInput(format!(
                "Failed to build Darkmatter key package event: {err}"
            ))
        })?;
        let tags = nostr_tags(&transport_event)?;

        Ok(MarmotKeyPackageEventData {
            content: transport_event.content,
            tags,
            d_tag,
            key_package_ref,
            key_package_ref_hex: metadata.key_package_ref_hex,
            app_components,
        })
    }
}

pub(crate) fn key_package_from_base64_content(content: &str) -> Result<KeyPackage> {
    Ok(KeyPackage::new(Base64::decode_vec(content)?))
}

pub(crate) fn key_package_from_v2_event(event: &Event) -> Result<KeyPackage> {
    validate_v2_event(event, REQUIRED_MLS_CIPHERSUITE_TAG)?;
    Ok(KeyPackage::with_source_event_id(
        Base64::decode_vec(&event.content)?,
        MessageId::new(event.id.to_bytes().to_vec()),
    ))
}

pub(crate) fn is_v2_event(event: &Event) -> bool {
    normalized_tag_values(event, TagKind::MlsExtensions)
        .iter()
        .any(|extension| extension == ACCOUNT_IDENTITY_PROOF_EXTENSION_TAG)
}

pub(crate) fn validate_v2_event(event: &Event, expected_ciphersuite: &str) -> Result<()> {
    if event.kind != Kind::Custom(30_443) {
        return Err(WhitenoiseError::InvalidEventKind {
            expected: Kind::Custom(30_443).to_string(),
            got: event.kind.to_string(),
        });
    }

    if event.tags.identifier().is_none() {
        return Err(WhitenoiseError::MissingKeyPackageDTag);
    }

    if has_encoding_tag(event) {
        return Err(WhitenoiseError::UnexpectedEncodingTag);
    }

    match tag_value(
        event,
        TagKind::Custom(MLS_PROTOCOL_VERSION_TAG_KEY.to_owned().into()),
    ) {
        Some("1.0") => {}
        Some(version) => {
            return Err(WhitenoiseError::InvalidInput(format!(
                "Unsupported Darkmatter key package MLS protocol version: {version}"
            )));
        }
        None => {
            return Err(WhitenoiseError::InvalidInput(
                "Missing mls_protocol_version tag on Darkmatter key package".to_string(),
            ));
        }
    }

    let expected_ciphersuite = expected_ciphersuite.to_ascii_lowercase();
    let advertised_ciphersuites = normalized_tag_values(event, TagKind::MlsCiphersuite);
    if !advertised_ciphersuites.contains(&expected_ciphersuite) {
        return Err(WhitenoiseError::IncompatibleMlsCiphersuite {
            expected: expected_ciphersuite,
            advertised: advertised_ciphersuites,
        });
    }

    let extensions: HashSet<String> = normalized_tag_values(event, TagKind::MlsExtensions)
        .into_iter()
        .collect();
    if !extensions.contains(ACCOUNT_IDENTITY_PROOF_EXTENSION_TAG) {
        return Err(WhitenoiseError::MissingMlsExtensions {
            missing: vec![ACCOUNT_IDENTITY_PROOF_EXTENSION_TAG.to_string()],
        });
    }

    let missing_proposals = missing_required_mls_proposals(event);
    if !missing_proposals.is_empty() {
        return Err(WhitenoiseError::MissingMlsProposals {
            missing: missing_proposals,
        });
    }

    if normalized_tag_values(
        event,
        TagKind::Custom(APP_COMPONENTS_TAG_KEY.to_owned().into()),
    )
    .is_empty()
    {
        return Err(WhitenoiseError::InvalidInput(
            "Missing app_components tag on Darkmatter key package".to_string(),
        ));
    }

    let advertised_key_package_ref = tag_value(
        event,
        TagKind::Custom(KEY_PACKAGE_REF_TAG_KEY.to_owned().into()),
    )
    .ok_or(WhitenoiseError::MissingKeyPackageRefTag)?
    .to_ascii_lowercase();
    let key_package_bytes = Base64::decode_vec(&event.content)?;
    let key_package = KeyPackage::new(key_package_bytes);
    let metadata = cgka_engine::key_package_metadata(&key_package)?;

    if advertised_key_package_ref != metadata.key_package_ref_hex {
        return Err(WhitenoiseError::InvalidKeyPackageRef {
            expected: metadata.key_package_ref_hex,
            advertised: advertised_key_package_ref,
        });
    }

    let expected_identity = event.pubkey.to_hex();
    if metadata.credential_identity_hex != expected_identity {
        return Err(WhitenoiseError::KeyPackageIdentityMismatch {
            expected: expected_identity,
            got: metadata.credential_identity_hex,
        });
    }

    Ok(())
}

fn has_encoding_tag(event: &Event) -> bool {
    event.tags.iter().any(|tag| {
        tag.kind() == TagKind::Custom("encoding".into()) && tag.content() == Some("base64")
    })
}

fn missing_required_mls_proposals(event: &Event) -> Vec<String> {
    let proposals: HashSet<String> =
        normalized_tag_values(event, TagKind::Custom(MLS_PROPOSALS_TAG_KEY.into()))
            .into_iter()
            .collect();

    REQUIRED_MLS_PROPOSAL_TAGS
        .into_iter()
        .filter(|required| !proposals.contains(*required))
        .map(|required| required.to_string())
        .collect()
}

fn normalized_tag_values(event: &Event, tag_kind: TagKind<'_>) -> Vec<String> {
    event
        .tags
        .iter()
        .filter(|tag| tag.kind() == tag_kind)
        .flat_map(|tag| tag.as_slice().iter().skip(1))
        .flat_map(|value| value.split(|c: char| c == ',' || c.is_ascii_whitespace()))
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase())
        .collect()
}

fn tag_value<'a>(event: &'a Event, tag_kind: TagKind<'_>) -> Option<&'a str> {
    event
        .tags
        .iter()
        .find(|tag| tag.kind() == tag_kind)
        .and_then(|tag| tag.content())
}

fn nostr_tags(event: &NostrTransportEvent) -> Result<Vec<Tag>> {
    event
        .tags
        .iter()
        .map(|tag| Tag::parse(tag.iter().map(String::as_str)).map_err(WhitenoiseError::from))
        .collect()
}

#[cfg(test)]
pub(crate) mod testsupport {
    use nostr_sdk::Keys;

    use super::*;
    use crate::marmot::session::MarmotSession;
    use crate::marmot::storage::WhitenoiseMarmotStorage;

    pub(crate) async fn adapter_key_package_event(
        keys: &Keys,
        key_package_ref: Option<String>,
        include_encoding_tag: bool,
    ) -> Event {
        let storage = WhitenoiseMarmotStorage::in_memory().unwrap();
        let mut session = MarmotSession::open_local(keys.public_key(), storage, keys.clone())
            .expect("open Marmot session");
        let key_package = session
            .fresh_key_package()
            .await
            .expect("create Marmot key package");
        let publication = session
            .key_package_event(
                key_package,
                "darkmatter-slot".to_string(),
                &[RelayUrl::parse("wss://kp.example").unwrap()],
            )
            .expect("build Marmot key package event");
        let mut tags = publication.tags;
        if let Some(key_package_ref) = key_package_ref {
            tags.iter_mut().for_each(|tag| {
                if tag.kind() == TagKind::Custom(KEY_PACKAGE_REF_TAG_KEY.into()) {
                    *tag = Tag::parse([KEY_PACKAGE_REF_TAG_KEY, key_package_ref.as_str()])
                        .expect("replacement i tag parses");
                }
            });
        }
        if include_encoding_tag {
            tags.push(Tag::parse(["encoding", "base64"]).expect("encoding tag parses"));
        }
        EventBuilder::new(Kind::Custom(30_443), publication.content)
            .tags(tags)
            .sign_with_keys(keys)
            .expect("sign key package event")
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::Keys;

    use super::*;
    use crate::whitenoise::key_packages::REQUIRED_MLS_CIPHERSUITE_TAG;

    #[tokio::test]
    async fn adapter_key_package_publication_validates_as_darkmatter_v2_event() {
        let keys = Keys::generate();
        let event = testsupport::adapter_key_package_event(&keys, None, false).await;

        let result = validate_v2_event(&event, REQUIRED_MLS_CIPHERSUITE_TAG);

        assert!(
            result.is_ok(),
            "adapter-produced Darkmatter v2 key package should validate: {result:?}",
        );
    }

    #[tokio::test]
    async fn adapter_key_package_publication_rejects_wrong_ref() {
        let keys = Keys::generate();
        let event =
            testsupport::adapter_key_package_event(&keys, Some("00".repeat(32)), false).await;

        let result = validate_v2_event(&event, REQUIRED_MLS_CIPHERSUITE_TAG);

        assert!(matches!(
            result,
            Err(WhitenoiseError::InvalidKeyPackageRef { .. })
        ));
    }

    #[tokio::test]
    async fn adapter_key_package_publication_rejects_encoding_tag() {
        let keys = Keys::generate();
        let event = testsupport::adapter_key_package_event(&keys, None, true).await;

        let result = validate_v2_event(&event, REQUIRED_MLS_CIPHERSUITE_TAG);

        assert!(matches!(
            result,
            Err(WhitenoiseError::UnexpectedEncodingTag)
        ));
    }
}
