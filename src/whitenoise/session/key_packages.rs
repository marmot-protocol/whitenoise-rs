//! Key package lifecycle operations scoped to an [`AccountSession`].

use std::collections::HashSet;
use std::time::Duration;

use mdk_core::key_packages::KeyPackageEventData;
use nostr_sdk::prelude::*;

use super::AccountSession;
use crate::RelayType;
use crate::perf_instrument;
use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::key_packages::filter_key_package_events_for_account;
use crate::whitenoise::relays::Relay;
use crate::whitenoise::users::User;

/// Maximum number of relay publish attempts before giving up.
const MAX_PUBLISH_ATTEMPTS: u32 = 3;

/// Maximum number of fetch-delete rounds before giving up.
const MAX_DELETE_ROUNDS: u32 = 10;

/// View over [`AccountSession`] for key package lifecycle operations.
///
/// Obtain via [`AccountSession::key_packages`].
pub struct KeyPackageOps<'a> {
    session: &'a AccountSession,
}

impl<'a> KeyPackageOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    // ── Public API ─────────────────────────────────────────────────────

    /// Publish a new key package for this account with retry (up to 3 attempts).
    ///
    /// Creates a single MLS key package, then retries relay publishing with
    /// exponential backoff if publishing fails. The key package is created
    /// only once to avoid orphaning unused key material in local MLS storage.
    #[perf_instrument("key_packages")]
    pub async fn publish(&self) -> Result<()> {
        let relays = self.prepare_relays().await?;

        if relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        let key_package_data = self.encoded_key_package(&relays).await?;
        let relay_urls = Relay::urls(&relays);

        let mut last_error = None;

        for attempt in 0..MAX_PUBLISH_ATTEMPTS {
            if attempt > 0 {
                let delay = Duration::from_secs(1 << attempt);
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "Retrying key package publish for account {} (attempt {}/{})",
                    self.session.account_pubkey.to_hex(),
                    attempt + 1,
                    MAX_PUBLISH_ATTEMPTS,
                );
                tokio::time::sleep(delay).await;
            }

            match self
                .publish_to_relays(
                    &key_package_data.content,
                    &relay_urls,
                    &key_package_data.tags_443,
                )
                .await
            {
                Ok(event_id) => {
                    self.track_published(&key_package_data.hash_ref, &event_id)
                        .await;
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::key_packages",
                        "Key package publish attempt {}/{} failed for account {}: {}",
                        attempt + 1,
                        MAX_PUBLISH_ATTEMPTS,
                        self.session.account_pubkey.to_hex(),
                        e,
                    );
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.expect("loop ran at least once"))
    }

    /// Create a new key package and publish it (single attempt, no retry).
    #[perf_instrument("key_packages")]
    pub(crate) async fn create_and_publish(&self, relays: &[Relay]) -> Result<()> {
        let key_package_data = self.encoded_key_package(relays).await?;
        let relay_urls = Relay::urls(relays);
        let event_id = self
            .publish_to_relays(
                &key_package_data.content,
                &relay_urls,
                &key_package_data.tags_443,
            )
            .await?;
        self.track_published(&key_package_data.hash_ref, &event_id)
            .await;
        Ok(())
    }

    /// Fetch all key package events for this account from its key package relays.
    #[perf_instrument("key_packages")]
    pub async fn fetch_all(&self) -> Result<Vec<Event>> {
        let relays = self.prepare_relays().await?;

        if relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        let relay_urls = Relay::urls(&relays);
        let key_package_events = self
            .session
            .ephemeral
            .fetch_key_packages_from_relays(&relay_urls)
            .await?;

        let (key_package_events, dropped_wrong_kind, dropped_wrong_author) =
            filter_key_package_events_for_account(self.session.account_pubkey, key_package_events);

        let total_dropped = dropped_wrong_kind + dropped_wrong_author;
        if total_dropped > 0 {
            tracing::warn!(
                target: "whitenoise::key_packages",
                "Dropped {} off-filter event(s) while fetching key packages for account {} \
                 (wrong kind: {}, wrong author: {})",
                total_dropped,
                self.session.account_pubkey.to_hex(),
                dropped_wrong_kind,
                dropped_wrong_author,
            );
        }

        tracing::debug!(
            target: "whitenoise::key_packages",
            "Found {} valid key package event(s) for account {}",
            key_package_events.len(),
            self.session.account_pubkey.to_hex()
        );

        Ok(key_package_events)
    }

    /// Delete a single key package from relays, optionally deleting local key material.
    ///
    /// Returns `true` if the deletion event was accepted by at least one relay.
    #[perf_instrument("key_packages")]
    pub async fn delete(&self, event_id: &EventId, delete_mls_stored_keys: bool) -> Result<bool> {
        if delete_mls_stored_keys {
            self.delete_local_key_material(event_id).await;
        }

        let relays = self.prepare_relays().await?;
        if relays.is_empty() {
            return Ok(false);
        }

        let relay_urls = Relay::urls(&relays);
        let result = self
            .session
            .ephemeral
            .publish_event_deletion(&[*event_id], &relay_urls)
            .await?;
        Ok(!result.success.is_empty())
    }

    /// Delete all key packages for this account with convergence loop.
    ///
    /// Fetches and deletes in rounds until no key packages remain on relays,
    /// up to 10 rounds.
    #[perf_instrument("key_packages")]
    pub async fn delete_all(&self, delete_mls_stored_keys: bool) -> Result<usize> {
        let mut total_deleted = 0;

        for round in 0..MAX_DELETE_ROUNDS {
            let key_package_events = self.fetch_all().await?;

            if key_package_events.is_empty() {
                tracing::info!(
                    target: "whitenoise::key_packages",
                    "All key packages deleted for account {} \
                     ({} total across {} round(s))",
                    self.session.account_pubkey.to_hex(),
                    total_deleted,
                    round + 1,
                );
                return Ok(total_deleted);
            }

            tracing::debug!(
                target: "whitenoise::key_packages",
                "Round {}: found {} remaining key package(s) for account {}",
                round + 1,
                key_package_events.len(),
                self.session.account_pubkey.to_hex(),
            );

            let batch_size = key_package_events.len();

            let deleted = self
                .delete_batch_internal(key_package_events, delete_mls_stored_keys, 1)
                .await?;

            total_deleted += deleted;

            if deleted == 0 {
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "Round {} deleted 0 key packages despite {} found \
                     — relays may not support deletion",
                    round + 1,
                    batch_size,
                );
                break;
            }
        }

        Ok(total_deleted)
    }

    /// Delete specific key package events from relays with retry.
    ///
    /// Storage deletion happens only on the initial attempt.
    #[perf_instrument("key_packages")]
    pub(crate) async fn delete_batch(
        &self,
        key_package_events: Vec<Event>,
        delete_mls_stored_keys: bool,
        max_retries: u32,
    ) -> Result<usize> {
        self.delete_batch_internal(key_package_events, delete_mls_stored_keys, max_retries)
            .await
    }

    // ── Private helpers ────────────────────────────────────────────────

    /// Create and encode a key package for this account.
    #[perf_instrument("key_packages")]
    async fn encoded_key_package(&self, relays: &[Relay]) -> Result<KeyPackageEventData> {
        let key_package_relay_urls = Relay::urls(relays);
        let data = self
            .session
            .mdk
            .create_key_package_for_event(&self.session.account_pubkey, key_package_relay_urls)
            .map_err(|e| WhitenoiseError::Configuration(format!("NostrMls error: {}", e)))?;

        Ok(data)
    }

    /// Publish an encoded key package to relays. Returns the event ID on success.
    #[perf_instrument("key_packages")]
    async fn publish_to_relays(
        &self,
        encoded_key_package: &str,
        relay_urls: &[RelayUrl],
        tags: &[Tag],
    ) -> Result<EventId> {
        let result = self
            .session
            .ephemeral
            .publish_key_package(encoded_key_package, relay_urls, tags)
            .await?;

        if result.success.is_empty() {
            return Err(WhitenoiseError::KeyPackagePublishFailed(
                "no relay accepted the key package event".to_string(),
            ));
        }

        tracing::debug!(
            target: "whitenoise::key_packages",
            "Published key package to {} relay(s)",
            result.success.len(),
        );

        Ok(*result.id())
    }

    /// Record a published key package in the lifecycle tracking table.
    #[perf_instrument("key_packages")]
    async fn track_published(&self, hash_ref: &[u8], event_id: &EventId) {
        if let Err(e) = self
            .session
            .repos
            .published_key_packages
            .create(hash_ref, &event_id.to_hex())
            .await
        {
            tracing::warn!(
                target: "whitenoise::key_packages",
                "Published key package but failed to track it: {}",
                e
            );
        }
    }

    /// Resolve key package relay URLs for this account.
    async fn prepare_relays(&self) -> Result<Vec<Relay>> {
        let user = User::find_by_pubkey(&self.session.account_pubkey, &self.session.database)
            .await
            .map_err(|_| WhitenoiseError::AccountNotFound)?;
        user.relays(RelayType::KeyPackage, &self.session.database)
            .await
    }

    /// Delete local MLS key material for a single key package event.
    async fn delete_local_key_material(&self, event_id: &EventId) {
        match self
            .session
            .repos
            .published_key_packages
            .find_by_event_id(&event_id.to_hex())
            .await
        {
            Ok(Some(pkg)) if !pkg.key_material_deleted => {
                if let Err(e) = self
                    .session
                    .mdk
                    .delete_key_package_from_storage_by_hash_ref(&pkg.key_package_hash_ref)
                {
                    tracing::warn!(
                        target: "whitenoise::key_packages",
                        "Failed to delete key material for event {}: {}",
                        event_id,
                        e
                    );
                    return;
                }
                if let Err(e) = self
                    .session
                    .repos
                    .published_key_packages
                    .mark_key_material_deleted(pkg.id)
                    .await
                {
                    tracing::warn!(
                        target: "whitenoise::key_packages",
                        "Deleted key material but failed to mark record {}: {}",
                        pkg.id,
                        e
                    );
                }
            }
            Ok(Some(_)) => {
                tracing::debug!(
                    target: "whitenoise::key_packages",
                    "Key material already deleted for event {}, skipping local deletion",
                    event_id
                );
            }
            Ok(None) => {
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "No published key package record found for event {}, cannot delete local key material",
                    event_id
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "Failed to look up published key package for event {}: {}",
                    event_id,
                    e
                );
            }
        }
    }

    /// Delete key packages from local MLS storage by parsing them from events.
    fn delete_from_storage(
        &self,
        key_package_events: &[Event],
        initial_count: usize,
    ) -> Result<()> {
        let mut storage_delete_count = 0;

        for event in key_package_events {
            match self.session.mdk.parse_key_package(event) {
                Ok(key_package) => {
                    match self
                        .session
                        .mdk
                        .delete_key_package_from_storage(&key_package)
                    {
                        Ok(_) => storage_delete_count += 1,
                        Err(e) => {
                            tracing::warn!(
                                target: "whitenoise::key_packages",
                                "Failed to delete key package from storage for event {}: {}",
                                event.id,
                                e
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::key_packages",
                        "Failed to parse key package for event {}: {}",
                        event.id,
                        e
                    );
                }
            }
        }

        tracing::debug!(
            target: "whitenoise::key_packages",
            "Deleted {} out of {} key packages from MLS storage",
            storage_delete_count,
            initial_count
        );

        Ok(())
    }

    /// Publish a batch deletion event for the given event IDs.
    #[perf_instrument("key_packages")]
    async fn publish_deletion(&self, event_ids: &[EventId], relay_urls: &[RelayUrl]) -> Result<()> {
        match self
            .session
            .ephemeral
            .publish_event_deletion(event_ids, relay_urls)
            .await
        {
            Ok(result) => {
                if result.success.is_empty() {
                    tracing::error!(
                        target: "whitenoise::key_packages",
                        "Batch deletion event was not accepted by any relay",
                    );
                } else {
                    tracing::info!(
                        target: "whitenoise::key_packages",
                        "Published batch deletion event to {} relay(s) for {} key packages",
                        result.success.len(),
                        event_ids.len()
                    );
                }
                Ok(())
            }
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::key_packages",
                    "Failed to publish batch deletion event: {}",
                    e
                );
                Err(e)
            }
        }
    }

    /// Internal batch delete with retry logic.
    async fn delete_batch_internal(
        &self,
        key_package_events: Vec<Event>,
        delete_mls_stored_keys: bool,
        max_retries: u32,
    ) -> Result<usize> {
        if key_package_events.is_empty() {
            tracing::debug!(
                target: "whitenoise::key_packages",
                "No key package events to delete for account {}",
                self.session.account_pubkey.to_hex()
            );
            return Ok(0);
        }

        let original_count = key_package_events.len();
        let original_ids: HashSet<EventId> = key_package_events.iter().map(|e| e.id).collect();

        tracing::debug!(
            target: "whitenoise::key_packages",
            "Deleting {} key package events for account {}",
            original_count,
            self.session.account_pubkey.to_hex()
        );

        let relays = self.prepare_relays().await?;
        if relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }
        let relay_urls = Relay::urls(&relays);

        // Delete from local storage on initial attempt only
        if delete_mls_stored_keys {
            self.delete_from_storage(&key_package_events, original_count)?;
        }

        let mut pending_ids: Vec<EventId> = key_package_events.iter().map(|e| e.id).collect();

        for attempt in 0..=max_retries {
            if attempt > 0 {
                tracing::debug!(
                    target: "whitenoise::key_packages",
                    "Retry {}/{} for {} remaining key package(s)",
                    attempt,
                    max_retries,
                    pending_ids.len()
                );
            }

            self.publish_deletion(&pending_ids, &relay_urls).await?;

            // Wait for relays to process
            tokio::time::sleep(Duration::from_millis(500)).await;

            // Check which of our original packages are still present
            let remaining_events = self.fetch_all().await?;
            pending_ids = remaining_events
                .iter()
                .filter(|e| original_ids.contains(&e.id))
                .map(|e| e.id)
                .collect();

            if pending_ids.is_empty() {
                break;
            }
        }

        let deleted_count = original_count - pending_ids.len();

        if pending_ids.is_empty() {
            tracing::info!(
                target: "whitenoise::key_packages",
                "Successfully deleted {} key package(s) for account {}",
                deleted_count,
                self.session.account_pubkey.to_hex()
            );
        } else {
            tracing::warn!(
                target: "whitenoise::key_packages",
                "After {} retries, {} of {} key package(s) still not deleted for account {}",
                max_retries,
                pending_ids.len(),
                original_count,
                self.session.account_pubkey.to_hex()
            );
        }

        Ok(deleted_count)
    }

    // ── Testing helpers ────────────────────────────────────────────────

    /// Look up a published key package record by event ID (test helper).
    #[cfg(feature = "integration-tests")]
    pub async fn find_published_for_testing(
        &self,
        event_id: &str,
    ) -> Result<Option<PublishedKeyPackage>> {
        self.session
            .repos
            .published_key_packages
            .find_by_event_id(event_id)
            .await
    }

    /// Record a published key package for lifecycle tracking (test helper).
    #[cfg(feature = "integration-tests")]
    pub async fn track_published_for_testing(&self, hash_ref: &[u8], event_id: &str) -> Result<()> {
        self.session
            .repos
            .published_key_packages
            .create(hash_ref, event_id)
            .await
    }

    /// Backdate `consumed_at` for a published key package (test helper).
    #[cfg(feature = "integration-tests")]
    pub async fn backdate_consumed_at_for_testing(
        &self,
        event_id: &str,
        age_secs: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE published_key_packages SET consumed_at = unixepoch() - ?
             WHERE account_pubkey = ? AND event_id = ?",
        )
        .bind(age_secs)
        .bind(self.session.account_pubkey.to_hex())
        .bind(event_id)
        .execute(&self.session.database.pool)
        .await
        .map_err(crate::whitenoise::database::DatabaseError::Sqlx)?;

        tracing::debug!(
            target: "whitenoise::key_packages",
            "Backdated consumed_at by {}s for KP event {} (TEST ONLY)",
            age_secs,
            event_id
        );

        Ok(())
    }
}
