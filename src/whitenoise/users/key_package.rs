use std::collections::HashSet;

use nostr_sdk::prelude::*;

use crate::perf_instrument;
use crate::whitenoise::{
    Whitenoise,
    error::Result,
    relays::{Relay, RelayType},
    users::User,
};

/// Status of a user's key package on relays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPackageStatus {
    /// User has a valid, compatible key package.
    Valid(Box<Event>),
    /// No key package event found on relays.
    NotFound,
    /// Key package found but is in an old/incompatible format.
    Incompatible,
}

impl User {
    /// Fetches the user's MLS key package event from their configured key package relays.
    ///
    /// This method retrieves the user's published MLS (Message Layer Security) key package
    /// from the Nostr network. Key packages are cryptographic objects that contain the user's
    /// public keys and credentials needed to add them to MLS group conversations.
    ///
    /// The method first retrieves the user's key package relay list (NIP-65 kind 10051 events),
    /// then fetches the most recent MLS key package event (kind 443) from those relays.
    ///
    /// # Arguments
    ///
    /// * `whitenoise` - The Whitenoise instance used to access the Nostr client and database
    #[perf_instrument("users")]
    pub async fn key_package_event(&self, whitenoise: &Whitenoise) -> Result<Option<Event>> {
        let key_package_relays = self
            .relays(RelayType::KeyPackage, &whitenoise.database)
            .await?;
        let mut key_package_relays_urls_set: HashSet<RelayUrl> =
            Relay::urls(&key_package_relays).into_iter().collect();
        if key_package_relays.is_empty() {
            tracing::warn!(
                target: "whitenoise::users::key_package_event",
                "User {} has no key package relays, using nip65 relays",
                self.pubkey
            );
            key_package_relays_urls_set.extend(Relay::urls(
                &self.relays(RelayType::Nip65, &whitenoise.database).await?,
            ));
        }
        if key_package_relays_urls_set.is_empty() {
            tracing::warn!(
                target: "whitenoise::users::key_package_event",
                "User {} has neither key package nor NIP-65 relays; returning None",
                self.pubkey
            );
            return Ok(None);
        }

        let key_package_relays_urls: Vec<RelayUrl> =
            key_package_relays_urls_set.into_iter().collect();
        let key_package_event = whitenoise
            .relay_control
            .fetch_user_key_package(self.pubkey, &key_package_relays_urls)
            .await?;
        Ok(key_package_event)
    }

    /// Checks the status of a user's key package on relays.
    ///
    /// Similar to [`key_package_event`](Self::key_package_event), but returns a
    /// [`KeyPackageStatus`] that distinguishes between valid, missing, and incompatible.
    #[perf_instrument("users")]
    pub async fn key_package_status(&self, whitenoise: &Whitenoise) -> Result<KeyPackageStatus> {
        let event = self.key_package_event(whitenoise).await?;
        Ok(classify_key_package(event))
    }
}

/// Checks whether a key package event has the required `["encoding", "base64"]` tag.
///
/// Per MIP-00/MIP-02, key package events must include an explicit encoding tag.
/// Old key packages published before this requirement lack this tag and are
/// incompatible with current clients.
pub(super) fn has_valid_encoding_tag(event: &Event) -> bool {
    event.tags.iter().any(|tag| {
        let s = tag.as_slice();
        s.len() >= 2 && s[0] == "encoding" && s[1] == "base64"
    })
}

/// Determines [`KeyPackageStatus`] from an optional key package event.
pub(super) fn classify_key_package(event: Option<Event>) -> KeyPackageStatus {
    match event {
        None => KeyPackageStatus::NotFound,
        Some(event) => {
            if has_valid_encoding_tag(&event) {
                KeyPackageStatus::Valid(Box::new(event))
            } else {
                KeyPackageStatus::Incompatible
            }
        }
    }
}
