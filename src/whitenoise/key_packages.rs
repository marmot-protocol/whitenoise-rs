use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::relays::Relay;
use nostr_sdk::prelude::*;
use std::sync::Arc;
use std::time::Duration;

impl Whitenoise {
    /// Helper method to create and encode a key package for the given account.
    pub(crate) async fn encoded_key_package(
        &self,
        account: &Account,
        key_package_relays: &[Relay],
    ) -> Result<(String, [Tag; 4])> {
        let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;

        let key_package_relay_urls = Relay::urls(key_package_relays);
        let result = mdk
            .create_key_package_for_event(&account.pubkey, key_package_relay_urls)
            .map_err(|e| WhitenoiseError::Configuration(format!("NostrMls error: {}", e)))?;

        Ok(result)
    }

    /// Publishes the MLS key package for the given account to its key package relays.
    pub async fn publish_key_package_for_account(&self, account: &Account) -> Result<()> {
        let relays = account.key_package_relays(self).await?;

        if relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }
        self.publish_key_package_to_relays(account, &relays).await?;

        Ok(())
    }

    pub(crate) async fn publish_key_package_to_relays(
        &self,
        account: &Account,
        relays: &[Relay],
    ) -> Result<()> {
        let (encoded_key_package, tags) = self.encoded_key_package(account, relays).await?;
        let relays_urls = Relay::urls(relays);
        let signer = self.get_signer_for_account(account)?;
        let result = self
            .nostr
            .publish_key_package_with_signer(&encoded_key_package, &relays_urls, &tags, signer)
            .await?;

        tracing::debug!(target: "whitenoise::publish_key_package_to_relays", "Published key package to relays: {:?}", result);

        Ok(())
    }

    /// Deletes the key package from the relays for the given account.
    ///
    /// Returns `true` if a key package was found and deleted, `false` if no key package was found.
    pub async fn delete_key_package_for_account(
        &self,
        account: &Account,
        event_id: &EventId,
        delete_mls_stored_keys: bool,
    ) -> Result<bool> {
        let key_package_filter = Filter::new()
            .id(*event_id)
            .kind(Kind::MlsKeyPackage)
            .author(account.pubkey);

        let mut key_package_stream = self
            .nostr
            .client
            .stream_events(key_package_filter, Duration::from_secs(5))
            .await?;

        let mut key_package_events = Vec::new();
        while let Some(event) = key_package_stream.next().await {
            key_package_events.push(event);
        }
        let signer = self.get_signer_for_account(account)?;

        if let Some(event) = key_package_events.first() {
            if delete_mls_stored_keys {
                let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
                let key_package = mdk.parse_key_package(event)?;
                mdk.delete_key_package_from_storage(&key_package)?;
            }

            let key_package_relays = account.key_package_relays(self).await?;
            if key_package_relays.is_empty() {
                return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
            }

            let key_package_relays_urls = Relay::urls(&key_package_relays);

            let result = self
                .nostr
                .publish_event_deletion_with_signer(&event.id, &key_package_relays_urls, signer)
                .await?;
            return Ok(!result.success.is_empty());
        }
        Ok(false)
    }

    /// Finds and returns all key package events for the given account from its key package relays.
    ///
    /// This method fetches all key package events (not just the latest) authored by the account
    /// from the account's key package relays. This is useful for getting a complete view of
    /// all published key packages.
    ///
    /// # Arguments
    ///
    /// * `account` - The account to find key packages for
    ///
    /// # Returns
    ///
    /// Returns a vector of all key package events found for the account.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Account has no key package relays configured
    /// - Failed to retrieve account's key package relays
    /// - Network error while fetching events from relays
    /// - NostrSDK error during event streaming
    pub async fn fetch_all_key_packages_for_account(
        &self,
        account: &Account,
    ) -> Result<Vec<Event>> {
        let key_package_relays = account.key_package_relays(self).await?;
        let relay_urls: Vec<RelayUrl> = Relay::urls(&key_package_relays);

        if relay_urls.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        let key_package_filter = Filter::new()
            .kind(Kind::MlsKeyPackage)
            .author(account.pubkey);

        let mut key_package_stream = self
            .nostr
            .client
            .stream_events_from(relay_urls, key_package_filter, Duration::from_secs(10))
            .await?;

        let mut key_package_events = Vec::new();
        while let Some(event) = key_package_stream.next().await {
            key_package_events.push(event);
        }

        tracing::debug!(
            target: "whitenoise::fetch_all_key_packages_for_account",
            "Found {} key package events for account {}",
            key_package_events.len(),
            account.pubkey.to_hex()
        );

        Ok(key_package_events)
    }

    /// Deletes all key package events from relays for the given account.
    ///
    /// This method finds all key package events authored by the account and publishes
    /// a batch deletion event to efficiently remove them from the relays. It then verifies
    /// the deletions by refetching and returns the actual count of deleted key packages.
    /// Optionally, it can also delete the MLS stored keys from local storage.
    ///
    /// # Arguments
    ///
    /// * `account` - The account to delete key packages for
    /// * `delete_mls_stored_keys` - Whether to also delete MLS keys from local storage
    ///
    /// # Returns
    ///
    /// Returns the number of key packages that were successfully deleted.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Account has no key package relays configured
    /// - Failed to retrieve account's key package relays
    /// - Failed to get signing keys for the account
    /// - Network error while fetching or publishing events
    /// - Batch deletion event publishing failed
    pub async fn delete_all_key_packages_for_account(
        &self,
        account: &Account,
        delete_mls_stored_keys: bool,
    ) -> Result<usize> {
        let key_package_events = self.fetch_all_key_packages_for_account(account).await?;
        self.delete_key_packages_for_account(account, key_package_events, delete_mls_stored_keys, 1)
            .await
    }

    /// Deletes the specified key package events from relays for the given account.
    ///
    /// This method publishes batch deletion events and retries up to `max_retries` times
    /// if some packages fail to delete. Storage deletion happens only on the initial attempt.
    ///
    /// # Arguments
    ///
    /// * `account` - The account the key packages belong to
    /// * `key_package_events` - The key package events to delete
    /// * `delete_mls_stored_keys` - Whether to also delete MLS keys from local storage
    /// * `max_retries` - Maximum number of retries after the initial attempt (0 = no retries)
    ///
    /// # Returns
    ///
    /// Returns the number of key packages that were successfully deleted.
    pub(crate) async fn delete_key_packages_for_account(
        &self,
        account: &Account,
        key_package_events: Vec<Event>,
        delete_mls_stored_keys: bool,
        max_retries: u32,
    ) -> Result<usize> {
        if key_package_events.is_empty() {
            tracing::debug!(
                target: "whitenoise::key_packages",
                "No key package events to delete for account {}",
                account.pubkey.to_hex()
            );
            return Ok(0);
        }

        let original_count = key_package_events.len();
        let original_ids: std::collections::HashSet<EventId> =
            key_package_events.iter().map(|e| e.id).collect();

        tracing::debug!(
            target: "whitenoise::key_packages",
            "Deleting {} key package events for account {}",
            original_count,
            account.pubkey.to_hex()
        );

        let (signer, relay_urls) = self.prepare_key_package_deletion_context(account).await?;

        // Delete from local storage on initial attempt only
        if delete_mls_stored_keys {
            self.delete_key_packages_from_storage(account, &key_package_events, original_count)?;
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

            self.publish_key_package_deletion_with_context(
                &pending_ids,
                &relay_urls,
                signer.clone(),
                "",
            )
            .await?;

            // Wait for relays to process
            tokio::time::sleep(Duration::from_millis(500)).await;

            // Check which of our original packages are still present
            let remaining_events = self.fetch_all_key_packages_for_account(account).await?;
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
                account.pubkey.to_hex()
            );
        } else {
            tracing::warn!(
                target: "whitenoise::key_packages",
                "After {} retries, {} of {} key package(s) still not deleted for account {}",
                max_retries,
                pending_ids.len(),
                original_count,
                account.pubkey.to_hex()
            );
        }

        Ok(deleted_count)
    }

    async fn prepare_key_package_deletion_context(
        &self,
        account: &Account,
    ) -> Result<(Arc<dyn NostrSigner>, Vec<RelayUrl>)> {
        let signer = self.get_signer_for_account(account)?;
        let key_package_relays = account.key_package_relays(self).await?;

        if key_package_relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        Ok((signer, Relay::urls(&key_package_relays)))
    }

    fn delete_key_packages_from_storage(
        &self,
        account: &Account,
        key_package_events: &[Event],
        initial_count: usize,
    ) -> Result<()> {
        let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
        let mut storage_delete_count = 0;

        for event in key_package_events {
            match mdk.parse_key_package(event) {
                Ok(key_package) => match mdk.delete_key_package_from_storage(&key_package) {
                    Ok(_) => storage_delete_count += 1,
                    Err(e) => {
                        tracing::warn!(
                            target: "whitenoise::key_packages",
                            "Failed to delete key package from storage for event {}: {}",
                            event.id,
                            e
                        );
                    }
                },
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

    async fn publish_key_package_deletion_with_context(
        &self,
        event_ids: &[EventId],
        relay_urls: &[RelayUrl],
        signer: Arc<dyn NostrSigner>,
        context: &str,
    ) -> Result<()> {
        match self
            .nostr
            .publish_batch_event_deletion_with_signer(event_ids, relay_urls, signer)
            .await
        {
            Ok(result) => {
                if result.success.is_empty() {
                    tracing::error!(
                        target: "whitenoise::key_packages",
                        "{}Batch deletion event was not accepted by any relay",
                        context
                    );
                } else {
                    tracing::info!(
                        target: "whitenoise::key_packages",
                        "{}Published batch deletion event to {} relay(s) for {} key packages",
                        context,
                        result.success.len(),
                        event_ids.len()
                    );
                }
                Ok(())
            }
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::key_packages",
                    "{}Failed to publish batch deletion event: {}",
                    context,
                    e
                );
                Err(e.into())
            }
        }
    }
}
