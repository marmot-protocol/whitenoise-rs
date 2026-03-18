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
    /// Returns the relay URLs to query for this user's key package.
    ///
    /// Fallback order:
    /// 1. User's KeyPackage relays (kind 10051)
    /// 2. User's NIP-65 relays (kind 10002)
    /// 3. Whitenoise discovery/fallback relays
    #[perf_instrument("users")]
    pub async fn key_package_relay_urls(&self, whitenoise: &Whitenoise) -> Result<Vec<RelayUrl>> {
        let kp_relays = self
            .relays(RelayType::KeyPackage, &whitenoise.database)
            .await?;
        if !kp_relays.is_empty() {
            return Ok(Relay::urls(&kp_relays));
        }

        tracing::warn!(
            target: "whitenoise::users::key_package",
            "User {} has no key package relays, trying NIP-65 relays",
            self.pubkey
        );

        let nip65_relays = self.relays(RelayType::Nip65, &whitenoise.database).await?;
        if !nip65_relays.is_empty() {
            return Ok(Relay::urls(&nip65_relays));
        }

        tracing::warn!(
            target: "whitenoise::users::key_package",
            "User {} has no NIP-65 relays either, using fallback relays",
            self.pubkey
        );

        Ok(whitenoise.fallback_relay_urls().await)
    }

    /// Fetches the user's MLS key package event from relays.
    ///
    /// Relay resolution follows [`key_package_relay_urls`](Self::key_package_relay_urls):
    /// the user's KeyPackage relays, then NIP-65, then discovery fallbacks.
    ///
    /// # Arguments
    ///
    /// * `whitenoise` - The Whitenoise instance used to access the Nostr client and database
    #[perf_instrument("users")]
    pub async fn key_package_event(&self, whitenoise: &Whitenoise) -> Result<Option<Event>> {
        let relay_urls = self.key_package_relay_urls(whitenoise).await?;
        if relay_urls.is_empty() {
            tracing::warn!(
                target: "whitenoise::users::key_package",
                "No relays available for user {}; returning None",
                self.pubkey
            );
            return Ok(None);
        }

        let event = whitenoise
            .relay_control
            .fetch_user_key_package(self.pubkey, &relay_urls)
            .await?;
        Ok(event)
    }

    /// Checks the status of a user's key package on relays.
    ///
    /// Similar to [`key_package_event`](Self::key_package_event), but returns a
    /// [`KeyPackageStatus`] that distinguishes between valid, missing, and incompatible.
    ///
    /// If the initial lookup returns [`KeyPackageStatus::NotFound`] and the user's
    /// relay data is empty or stale, this method will perform a blocking relay sync
    /// and retry once.
    #[perf_instrument("users")]
    pub async fn key_package_status(&self, whitenoise: &Whitenoise) -> Result<KeyPackageStatus> {
        let event = self.key_package_event(whitenoise).await?;
        let status = classify_key_package(event);

        match status {
            KeyPackageStatus::NotFound
                if self.should_retry_key_package_lookup(whitenoise).await? =>
            {
                tracing::debug!(
                    target: "whitenoise::users::key_package",
                    "Key package not found for user {} with empty or stale relay data, syncing relays and retrying",
                    self.pubkey
                );
                if let Err(e) = self.update_relay_lists(whitenoise).await {
                    tracing::warn!(
                        target: "whitenoise::users::key_package",
                        "Failed to sync relay lists for user {}: {}",
                        self.pubkey,
                        e
                    );
                    return Ok(KeyPackageStatus::NotFound);
                }
                let event = self.key_package_event(whitenoise).await?;
                Ok(classify_key_package(event))
            }
            other => Ok(other),
        }
    }

    /// Determines whether a failed key-package lookup should trigger a relay
    /// sync and retry.
    ///
    /// Returns `true` when the stored relay data is empty or stale, meaning a
    /// fresh sync might discover relays that actually host the key package.
    async fn should_retry_key_package_lookup(&self, whitenoise: &Whitenoise) -> Result<bool> {
        let kp_relays = self
            .relays(RelayType::KeyPackage, &whitenoise.database)
            .await?;
        if kp_relays.is_empty() {
            return Ok(true);
        }
        Ok(self.needs_metadata_refresh())
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
