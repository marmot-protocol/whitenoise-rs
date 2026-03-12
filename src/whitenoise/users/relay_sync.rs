use std::collections::HashSet;

use chrono::{DateTime, Utc};
use nostr_sdk::prelude::*;

use crate::perf_instrument;
use crate::relay_control::ephemeral::EphemeralScope;
use crate::whitenoise::{
    Whitenoise,
    database::processed_events::ProcessedEvent,
    error::Result,
    relays::{Relay, RelayType},
    users::User,
    utils::timestamp_to_datetime,
};

impl User {
    /// Fetches the latest relay lists for this user from Nostr and updates the local database
    #[perf_instrument("relay_sync")]
    pub(crate) async fn update_relay_lists(&self, whitenoise: &Whitenoise) -> Result<()> {
        let scope = whitenoise.relay_control.ephemeral().anonymous_scope();
        self.update_relay_lists_with_scope(whitenoise, &scope)
            .await?;
        Ok(())
    }

    #[perf_instrument("relay_sync")]
    async fn update_relay_lists_with_scope(
        &self,
        whitenoise: &Whitenoise,
        scope: &EphemeralScope,
    ) -> Result<Vec<Relay>> {
        let initial_query_relays = self.get_query_relays(whitenoise).await?;

        tracing::info!(
            target: "whitenoise::users::update_relay_lists",
            "Updating relay lists for user {} using {} query relays",
            self.pubkey,
            initial_query_relays.len()
        );

        let updated_query_relays = self
            .update_nip65_relays_with_scope(whitenoise, scope, &initial_query_relays)
            .await?;

        self.update_secondary_relay_types_with_scope(whitenoise, scope, &updated_query_relays)
            .await?;

        tracing::info!(
            target: "whitenoise::users::update_relay_lists",
            "Successfully completed relay list updates for user {}",
            self.pubkey
        );

        Ok(updated_query_relays)
    }

    pub(super) async fn get_query_relays(&self, whitenoise: &Whitenoise) -> Result<Vec<Relay>> {
        let stored_relays = self.relays(RelayType::Nip65, &whitenoise.database).await?;

        if stored_relays.is_empty() {
            tracing::debug!(
                target: "whitenoise::users::get_query_relays",
                "User {} has no stored NIP-65 relays, using fallback relays",
                self.pubkey,
            );
            let urls = whitenoise.fallback_relay_urls().await;
            Ok(urls.into_iter().map(|url| Relay::new(&url)).collect())
        } else {
            Ok(stored_relays)
        }
    }

    #[cfg(test)]
    pub(super) async fn update_nip65_relays(
        &self,
        whitenoise: &Whitenoise,
        query_relays: &[Relay],
    ) -> Result<Vec<Relay>> {
        let scope = whitenoise.relay_control.ephemeral().anonymous_scope();
        self.update_nip65_relays_with_scope(whitenoise, &scope, query_relays)
            .await
    }

    #[perf_instrument("relay_sync")]
    async fn update_nip65_relays_with_scope(
        &self,
        whitenoise: &Whitenoise,
        scope: &EphemeralScope,
        query_relays: &[Relay],
    ) -> Result<Vec<Relay>> {
        match self
            .sync_relays_for_type_with_scope(whitenoise, scope, RelayType::Nip65, query_relays)
            .await
        {
            Ok(true) => {
                let refreshed_relays = self.relays(RelayType::Nip65, &whitenoise.database).await?;
                tracing::info!(
                    target: "whitenoise::users::update_nip65_relays",
                    "Updated NIP-65 relays for user {}, now using {} relays for other types",
                    self.pubkey,
                    refreshed_relays.len()
                );
                Ok(refreshed_relays)
            }
            Ok(false) => {
                tracing::debug!(
                    target: "whitenoise::users::update_nip65_relays",
                    "NIP-65 relays unchanged for user {}",
                    self.pubkey
                );
                Ok(query_relays.to_vec())
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::users::update_nip65_relays",
                    "Failed to update NIP-65 relays for user {}: {}, continuing with original relays",
                    self.pubkey,
                    e
                );
                Ok(query_relays.to_vec())
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn update_secondary_relay_types(
        &self,
        whitenoise: &Whitenoise,
        query_relays: &[Relay],
    ) -> Result<()> {
        let scope = whitenoise.relay_control.ephemeral().anonymous_scope();
        self.update_secondary_relay_types_with_scope(whitenoise, &scope, query_relays)
            .await
    }

    #[perf_instrument("relay_sync")]
    async fn update_secondary_relay_types_with_scope(
        &self,
        whitenoise: &Whitenoise,
        scope: &EphemeralScope,
        query_relays: &[Relay],
    ) -> Result<()> {
        const SECONDARY_RELAY_TYPES: &[RelayType] = &[RelayType::Inbox, RelayType::KeyPackage];
        let relays_urls: Vec<_> = Relay::urls(query_relays);

        let (inbox_result, key_package_result) = tokio::join!(
            scope.fetch_user_relays(self.pubkey, RelayType::Inbox, &relays_urls),
            scope.fetch_user_relays(self.pubkey, RelayType::KeyPackage, &relays_urls),
        );

        for (relay_type, relay_event_result) in SECONDARY_RELAY_TYPES
            .iter()
            .copied()
            .zip([inbox_result, key_package_result])
        {
            match relay_event_result {
                Ok(relay_event) => {
                    if let Err(error) = self
                        .apply_relay_event(whitenoise, relay_type, relay_event)
                        .await
                    {
                        tracing::warn!(
                            target: "whitenoise::users::update_secondary_relay_types",
                            "Failed to update {:?} relays for user {}: {}",
                            relay_type,
                            self.pubkey,
                            error
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: "whitenoise::users::update_secondary_relay_types",
                        "Failed to fetch {:?} relays for user {}: {}",
                        relay_type,
                        self.pubkey,
                        error
                    );
                }
            }
        }

        Ok(())
    }

    /// Synchronizes stored relays with a new set of relay URLs.
    ///
    /// Returns `true` if changes were made, `false` if no changes needed.
    #[perf_instrument("relay_sync")]
    pub(crate) async fn sync_relay_urls(
        &self,
        whitenoise: &Whitenoise,
        relay_type: RelayType,
        new_relay_urls: &HashSet<RelayUrl>,
        event_created_at: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        // First, check if we should process this event based on timestamp
        if let Some(new_timestamp) = event_created_at {
            let newest_stored_timestamp = ProcessedEvent::newest_relay_event_timestamp(
                &self.pubkey,
                relay_type,
                &whitenoise.database,
            )
            .await?;

            match newest_stored_timestamp {
                Some(stored_timestamp)
                    if new_timestamp.timestamp_millis() <= stored_timestamp.timestamp_millis() =>
                {
                    tracing::debug!(
                        target: "whitenoise::users::sync_relay_urls",
                        "Ignoring stale {:?} relay event for user {} (event: {}, stored: {})",
                        relay_type,
                        self.pubkey,
                        new_timestamp.timestamp_millis(),
                        stored_timestamp.timestamp_millis()
                    );
                    return Ok(false);
                }
                None => {
                    tracing::debug!(
                        target: "whitenoise::users::sync_relay_urls",
                        "No stored {:?} relay timestamps for user {}, accepting new event",
                        relay_type,
                        self.pubkey
                    );
                }
                Some(_) => {
                    tracing::debug!(
                        target: "whitenoise::users::sync_relay_urls",
                        "New {:?} relay event is newer for user {}, proceeding with sync",
                        relay_type,
                        self.pubkey
                    );
                }
            }
        }

        let stored_relays = self.relays(relay_type, &whitenoise.database).await?;
        let stored_urls: HashSet<&RelayUrl> = stored_relays.iter().map(|r| &r.url).collect();
        let new_urls_set: HashSet<&RelayUrl> = new_relay_urls.iter().collect();

        if stored_urls == new_urls_set {
            tracing::debug!(
                target: "whitenoise::users::sync_relay_urls",
                "No changes needed for {:?} relays for user {}",
                relay_type,
                self.pubkey
            );
            return Ok(false);
        }

        tracing::info!(
            target: "whitenoise::users::sync_relay_urls",
            "Updating {:?} relays for user {}: {} existing -> {} new",
            relay_type,
            self.pubkey,
            stored_urls.len(),
            new_urls_set.len()
        );

        for existing_relay in &stored_relays {
            if !new_urls_set.contains(&existing_relay.url)
                && let Err(e) = self
                    .remove_relay(existing_relay, relay_type, &whitenoise.database)
                    .await
            {
                tracing::warn!(
                    target: "whitenoise::users::sync_relay_urls",
                    "Failed to remove {:?} relay {} for user {}: {}",
                    relay_type,
                    existing_relay.url,
                    self.pubkey,
                    e
                );
                return Err(e);
            }
        }

        for new_relay_url in new_relay_urls {
            if !stored_urls.contains(new_relay_url) {
                let new_relay = whitenoise
                    .find_or_create_relay_by_url(new_relay_url)
                    .await?;
                if let Err(e) = self
                    .add_relay(&new_relay, relay_type, &whitenoise.database)
                    .await
                {
                    tracing::warn!(
                        target: "whitenoise::users::sync_relay_urls",
                        "Failed to add {:?} relay {} for user {}: {}",
                        relay_type,
                        new_relay_url,
                        self.pubkey,
                        e
                    );
                    return Err(e);
                }
            }
        }

        Ok(true)
    }

    /// Synchronizes relays for a specific type with the network state.
    ///
    /// Returns `true` if changes were made, `false` if no changes needed.
    #[cfg(test)]
    pub(super) async fn sync_relays_for_type(
        &self,
        whitenoise: &Whitenoise,
        relay_type: RelayType,
        query_relays: &[Relay],
    ) -> Result<bool> {
        let scope = whitenoise.relay_control.ephemeral().anonymous_scope();
        self.sync_relays_for_type_with_scope(whitenoise, &scope, relay_type, query_relays)
            .await
    }

    #[perf_instrument("relay_sync")]
    async fn sync_relays_for_type_with_scope(
        &self,
        whitenoise: &Whitenoise,
        scope: &EphemeralScope,
        relay_type: RelayType,
        query_relays: &[Relay],
    ) -> Result<bool> {
        let relays_urls: Vec<_> = Relay::urls(query_relays);
        let relay_event = scope
            .fetch_user_relays(self.pubkey, relay_type, &relays_urls)
            .await
            .map_err(|e| {
                tracing::warn!(
                    target: "whitenoise::users::sync_relays_for_type",
                    "Failed to fetch {:?} relays for user {}: {}",
                    relay_type, self.pubkey, e
                );
                e
            })?;

        self.apply_relay_event(whitenoise, relay_type, relay_event)
            .await
    }

    #[perf_instrument("relay_sync")]
    async fn apply_relay_event(
        &self,
        whitenoise: &Whitenoise,
        relay_type: RelayType,
        relay_event: Option<Event>,
    ) -> Result<bool> {
        match relay_event {
            Some(event) => {
                let relay_hashset: HashSet<_> =
                    crate::nostr_manager::utils::relay_urls_from_event(&event);
                let changed = self
                    .sync_relay_urls(
                        whitenoise,
                        relay_type,
                        &relay_hashset,
                        Some(timestamp_to_datetime(event.created_at)?),
                    )
                    .await?;

                if changed {
                    whitenoise
                        .event_tracker
                        .track_processed_global_event(&event)
                        .await?;

                    tracing::debug!(
                        target: "whitenoise::users::sync_relays_for_type",
                        "Updated {:?} relays for user {} via background sync with event {}",
                        relay_type, self.pubkey, event.id.to_hex()
                    );
                }

                Ok(changed)
            }
            None => {
                tracing::debug!(
                    target: "whitenoise::users::sync_relays_for_type",
                    "No {:?} relay events found for user {}",
                    relay_type, self.pubkey
                );
                Ok(false)
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn all_users_with_relay_urls(
        whitenoise: &Whitenoise,
    ) -> Result<Vec<(PublicKey, Vec<RelayUrl>)>> {
        Self::all_with_nip65_relay_urls(&whitenoise.database).await
    }
}
