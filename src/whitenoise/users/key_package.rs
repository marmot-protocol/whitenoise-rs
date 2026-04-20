use nostr_sdk::prelude::*;

use crate::perf_instrument;
use crate::relay_control::ephemeral::KeyPackageLookup;
use crate::whitenoise::{
    Whitenoise,
    error::Result,
    relays::{Relay, RelayType},
    users::User,
};

#[cfg(test)]
use crate::whitenoise::key_packages::{
    REQUIRED_MLS_CIPHERSUITE_TAG, validate_marmot_key_package_tags,
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
    /// 4. Default bootstrap relays from `Relay::defaults()`
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
            "User {} has no NIP-65 relays either, trying configured discovery relays",
            self.pubkey
        );

        let fallback_relays = whitenoise.fallback_relay_urls().await;
        if !fallback_relays.is_empty() {
            return Ok(fallback_relays);
        }

        tracing::warn!(
            target: "whitenoise::users::key_package",
            "User {} has no configured discovery relays available either, trying default relays",
            self.pubkey
        );

        Ok(Relay::urls(&Relay::defaults()))
    }

    /// Fetches the user's MLS key package event from relays.
    ///
    /// Relay resolution follows [`key_package_relay_urls`](Self::key_package_relay_urls):
    /// the user's KeyPackage relays, then NIP-65, then configured discovery
    /// relays, then default bootstrap relays.
    ///
    /// # Arguments
    ///
    /// * `whitenoise` - The Whitenoise instance used to access the Nostr client and database
    #[perf_instrument("users")]
    pub async fn key_package_event(&self, whitenoise: &Whitenoise) -> Result<Option<Event>> {
        match self.key_package_lookup(whitenoise).await? {
            KeyPackageLookup::Found(event) => Ok(Some(event)),
            KeyPackageLookup::Incompatible { .. } | KeyPackageLookup::NotFound => Ok(None),
        }
    }

    #[perf_instrument("users")]
    pub(crate) async fn key_package_lookup(
        &self,
        whitenoise: &Whitenoise,
    ) -> Result<KeyPackageLookup> {
        let relay_urls = self.key_package_relay_urls(whitenoise).await?;
        if relay_urls.is_empty() {
            tracing::warn!(
                target: "whitenoise::users::key_package",
                "No relays available for user {}; returning None",
                self.pubkey
            );
            return Ok(KeyPackageLookup::NotFound);
        }

        let lookup = whitenoise
            .relay_control
            .fetch_user_key_package_lookup(self.pubkey, &relay_urls)
            .await?;
        Ok(lookup)
    }

    /// Checks the status of a user's key package on relays.
    ///
    /// Similar to [`key_package_event`](Self::key_package_event), but returns a
    /// [`KeyPackageStatus`] that distinguishes between valid, missing, and incompatible.
    ///
    /// If the initial lookup returns [`KeyPackageStatus::NotFound`] and the user has no
    /// relay data yet (e.g. created by a background sync that hasn't finished), this
    /// method will perform a blocking relay sync and retry once.
    #[perf_instrument("users")]
    pub async fn key_package_status(&self, whitenoise: &Whitenoise) -> Result<KeyPackageStatus> {
        let lookup = self.key_package_lookup(whitenoise).await?;
        let status = classify_key_package_lookup(lookup);

        match status {
            KeyPackageStatus::NotFound
                if self
                    .relays(RelayType::KeyPackage, &whitenoise.database)
                    .await?
                    .is_empty() =>
            {
                tracing::debug!(
                    target: "whitenoise::users::key_package",
                    "Key package not found for user {} with empty relay list, syncing relays and retrying",
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
                let lookup = self.key_package_lookup(whitenoise).await?;
                Ok(classify_key_package_lookup(lookup))
            }
            other => Ok(other),
        }
    }
}

/// Determines [`KeyPackageStatus`] from an optional key package event.
#[cfg(test)]
pub(super) fn classify_key_package(event: Option<Event>) -> KeyPackageStatus {
    match event {
        None => KeyPackageStatus::NotFound,
        Some(event) => {
            if validate_marmot_key_package_tags(&event, REQUIRED_MLS_CIPHERSUITE_TAG).is_ok() {
                KeyPackageStatus::Valid(Box::new(event))
            } else {
                KeyPackageStatus::Incompatible
            }
        }
    }
}

fn classify_key_package_lookup(lookup: KeyPackageLookup) -> KeyPackageStatus {
    match lookup {
        KeyPackageLookup::Found(event) => KeyPackageStatus::Valid(Box::new(event)),
        KeyPackageLookup::Incompatible { .. } => KeyPackageStatus::Incompatible,
        KeyPackageLookup::NotFound => KeyPackageStatus::NotFound,
    }
}
