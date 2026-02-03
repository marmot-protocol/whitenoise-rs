use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::relays::Relay;
use nostr_sdk::prelude::*;
use std::sync::Arc;
use std::time::Duration;

/// Checks if a key package event has the required encoding tag.
///
/// Per MIP-00/MIP-02, key packages must have an explicit `["encoding", "base64"]` tag.
/// Key packages without this tag are considered outdated and should be rotated.
///
/// # Arguments
///
/// * `event` - The key package event to check
///
/// # Returns
///
/// Returns `true` if the event has the required encoding tag, `false` otherwise.
pub(crate) fn has_encoding_tag(event: &Event) -> bool {
    event.tags.iter().any(|tag| {
        tag.kind() == TagKind::Custom("encoding".into()) && tag.content() == Some("base64")
    })
}

/// Returns key packages that are missing the required encoding tag.
///
/// These outdated packages were published before the MIP-00/MIP-02 encoding tag
/// requirement was enforced. They should be deleted and replaced with new
/// key packages that include the proper `["encoding", "base64"]` tag.
///
/// # Arguments
///
/// * `packages` - The key package events to check
///
/// # Returns
///
/// A vector of key package events that are missing the encoding tag.
pub(crate) fn find_outdated_packages(packages: &[Event]) -> Vec<Event> {
    packages
        .iter()
        .filter(|p| !has_encoding_tag(p))
        .cloned()
        .collect()
}

impl Whitenoise {
    /// Gets the appropriate signer for an account.
    ///
    /// For external accounts (Amber/NIP-55), returns the stored external signer.
    /// For local accounts, returns the keys from the secrets store.
    ///
    /// Returns an error if no signer is available for the account.
    fn get_signer_for_account(&self, account: &Account) -> Result<Arc<dyn NostrSigner>> {
        // First check for a registered external signer
        if let Some(external_signer) = self.get_external_signer(&account.pubkey) {
            tracing::debug!(
                target: "whitenoise::key_packages",
                "Using external signer for account {}",
                account.pubkey.to_hex()
            );
            return Ok(external_signer);
        }

        // Fall back to local keys from secrets store
        let keys = self
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)?;
        tracing::debug!(
            target: "whitenoise::key_packages",
            "Using local keys for account {}",
            account.pubkey.to_hex()
        );
        Ok(Arc::new(keys))
    }

    /// Helper method to create and encode a key package for the given account.
    pub(crate) async fn encoded_key_package(
        &self,
        account: &Account,
        key_package_relays: &[Relay],
    ) -> Result<(String, [Tag; 7])> {
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

    /// Publishes the MLS key package using an external signer.
    ///
    /// This is used for external signer accounts (like Amber/NIP-55) where the
    /// private key is not available locally. The signer is used to sign the
    /// key package event before publishing.
    pub async fn publish_key_package_for_account_with_signer(
        &self,
        account: &Account,
        signer: impl NostrSigner + 'static,
    ) -> Result<()> {
        let relays = account.key_package_relays(self).await?;

        if relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        let (encoded_key_package, tags) = self.encoded_key_package(account, &relays).await?;
        let relays_urls = Relay::urls(&relays);

        let result = self
            .nostr
            .publish_key_package_with_signer(&encoded_key_package, &relays_urls, &tags, signer)
            .await?;

        tracing::debug!(
            target: "whitenoise::publish_key_package_with_signer",
            "Published key package with external signer: {:?}",
            result
        );

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
    /// Automatically uses the appropriate signer for the account:
    /// - For external accounts (Amber/NIP-55): uses the registered external signer
    /// - For local accounts: uses keys from the secrets store
    ///
    /// Returns `true` if a key package was found and deleted, `false` if no key package was found.
    pub async fn delete_key_package_for_account(
        &self,
        account: &Account,
        event_id: &EventId,
        delete_mls_stored_keys: bool,
    ) -> Result<bool> {
        let signer = self.get_signer_for_account(account)?;
        self.delete_key_package_for_account_internal(
            account,
            event_id,
            delete_mls_stored_keys,
            signer,
        )
        .await
    }

    /// Deletes the key package from the relays using an external signer.
    ///
    /// This is used for external signer accounts (like Amber/NIP-55) where the
    /// private key is not available locally. The signer is used to sign the
    /// deletion event before publishing.
    ///
    /// Returns `true` if a key package was found and deleted, `false` if no key package was found.
    pub async fn delete_key_package_for_account_with_signer(
        &self,
        account: &Account,
        event_id: &EventId,
        delete_mls_stored_keys: bool,
        signer: impl NostrSigner + 'static,
    ) -> Result<bool> {
        self.delete_key_package_for_account_internal(
            account,
            event_id,
            delete_mls_stored_keys,
            signer,
        )
        .await
    }

    async fn delete_key_package_for_account_internal(
        &self,
        account: &Account,
        event_id: &EventId,
        delete_mls_stored_keys: bool,
        signer: impl NostrSigner + 'static,
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
    /// Automatically uses the appropriate signer for the account:
    /// - For external accounts (Amber/NIP-55): uses the registered external signer
    /// - For local accounts: uses keys from the secrets store
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
        let signer = self.get_signer_for_account(account)?;
        self.delete_key_packages_for_account_internal(
            account,
            key_package_events,
            delete_mls_stored_keys,
            1,
            signer,
        )
        .await
    }

    /// Deletes all key package events from relays using an external signer.
    ///
    /// This is used for external signer accounts (like Amber/NIP-55) where the
    /// private key is not available locally. The signer is used to sign the
    /// deletion events before publishing.
    ///
    /// # Arguments
    ///
    /// * `account` - The account to delete key packages for
    /// * `delete_mls_stored_keys` - Whether to also delete MLS keys from local storage
    /// * `signer` - The external signer to use for signing deletion events
    ///
    /// # Returns
    ///
    /// Returns the number of key packages that were successfully deleted.
    pub async fn delete_all_key_packages_for_account_with_signer(
        &self,
        account: &Account,
        delete_mls_stored_keys: bool,
        signer: impl NostrSigner + Clone + 'static,
    ) -> Result<usize> {
        let key_package_events = self.fetch_all_key_packages_for_account(account).await?;
        self.delete_key_packages_for_account_internal(
            account,
            key_package_events,
            delete_mls_stored_keys,
            1,
            signer,
        )
        .await
    }

    /// Deletes the specified key package events from relays for the given account.
    ///
    /// This method publishes batch deletion events and retries up to `max_retries` times
    /// if some packages fail to delete. Storage deletion happens only on the initial attempt.
    ///
    /// Automatically uses the appropriate signer for the account:
    /// - For external accounts (Amber/NIP-55): uses the registered external signer
    /// - For local accounts: uses keys from the secrets store
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
        let signer = self.get_signer_for_account(account)?;
        self.delete_key_packages_for_account_internal(
            account,
            key_package_events,
            delete_mls_stored_keys,
            max_retries,
            signer,
        )
        .await
    }

    async fn delete_key_packages_for_account_internal(
        &self,
        account: &Account,
        key_package_events: Vec<Event>,
        delete_mls_stored_keys: bool,
        max_retries: u32,
        signer: impl NostrSigner + Clone + 'static,
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

        let relay_urls = self.prepare_key_package_relay_urls(account).await?;

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

            self.publish_key_package_deletion_with_signer(
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

    async fn prepare_key_package_relay_urls(&self, account: &Account) -> Result<Vec<RelayUrl>> {
        let key_package_relays = account.key_package_relays(self).await?;

        if key_package_relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        Ok(Relay::urls(&key_package_relays))
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

    async fn publish_key_package_deletion_with_signer(
        &self,
        event_ids: &[EventId],
        relay_urls: &[RelayUrl],
        signer: impl NostrSigner + 'static,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::accounts::AccountType;
    use crate::whitenoise::test_utils::*;
    use chrono::Utc;
    use nostr_sdk::{EventBuilder, Keys, Kind, Tag};

    fn create_local_account_struct() -> Account {
        Account {
            id: Some(1),
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn create_external_account_struct() -> Account {
        Account {
            id: Some(2),
            pubkey: Keys::generate().public_key(),
            user_id: 2,
            account_type: AccountType::External,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_get_signer_for_local_account_with_keys() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an account and store keys in secrets store
        let keys = Keys::generate();
        let pubkey = keys.public_key();
        whitenoise
            .secrets_store
            .store_private_key(&keys)
            .expect("Should store keys");

        let account = Account {
            id: Some(1),
            pubkey,
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // Should successfully get signer
        let signer = whitenoise.get_signer_for_account(&account);
        assert!(
            signer.is_ok(),
            "Should get signer for local account with keys"
        );

        // Verify signer returns correct pubkey
        let signer = signer.unwrap();
        let signer_pubkey = signer.get_public_key().await.unwrap();
        assert_eq!(signer_pubkey, pubkey);
    }

    #[tokio::test]
    async fn test_get_signer_for_local_account_without_keys_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a local account without storing keys
        let account = create_local_account_struct();

        // Should fail to get signer (no keys in secrets store)
        let result = whitenoise.get_signer_for_account(&account);
        assert!(
            result.is_err(),
            "Should fail for local account without keys"
        );
    }

    #[tokio::test]
    async fn test_get_signer_for_external_account_with_registered_signer() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create external account
        let keys = Keys::generate();
        let pubkey = keys.public_key();
        let account = Account {
            id: Some(1),
            pubkey,
            user_id: 1,
            account_type: AccountType::External,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // Register external signer
        whitenoise.register_external_signer(pubkey, keys.clone());

        // Should get the registered external signer
        let result = whitenoise.get_signer_for_account(&account);
        assert!(
            result.is_ok(),
            "Should get signer for external account with registered signer"
        );

        let signer = result.unwrap();
        let signer_pubkey = signer.get_public_key().await.unwrap();
        assert_eq!(signer_pubkey, pubkey);
    }

    #[tokio::test]
    async fn test_get_signer_for_external_account_without_signer_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create external account without registering signer
        let account = create_external_account_struct();

        // Should fail (no external signer registered, no local keys)
        let result = whitenoise.get_signer_for_account(&account);
        assert!(
            result.is_err(),
            "Should fail for external account without registered signer"
        );
    }

    #[tokio::test]
    async fn test_get_signer_prefers_external_signer_over_local_keys() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create account with both local keys and registered external signer
        let local_keys = Keys::generate();
        let external_keys = Keys::generate();
        let pubkey = local_keys.public_key();

        // Store local keys
        whitenoise
            .secrets_store
            .store_private_key(&local_keys)
            .expect("Should store keys");

        // Register external signer for same pubkey
        whitenoise.register_external_signer(pubkey, external_keys.clone());

        let account = Account {
            id: Some(1),
            pubkey,
            user_id: 1,
            account_type: AccountType::Local, // Even for local account type
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // Should prefer external signer
        let signer = whitenoise.get_signer_for_account(&account).unwrap();
        let signer_pubkey = signer.get_public_key().await.unwrap();

        // External signer has different keys, so pubkey will be from external_keys
        assert_eq!(
            signer_pubkey,
            external_keys.public_key(),
            "Should use external signer when available"
        );
    }

    #[tokio::test]
    async fn test_prepare_key_package_relay_urls_with_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an account with key package relays using test helper
        let (account, _keys) = create_test_account(&whitenoise).await;

        // Setup relays
        let user = account.user(&whitenoise.database).await.unwrap();
        let relay = crate::whitenoise::relays::Relay::find_or_create_by_url(
            &RelayUrl::parse("wss://test.relay.com").unwrap(),
            &whitenoise.database,
        )
        .await
        .unwrap();
        user.add_relay(&relay, crate::RelayType::KeyPackage, &whitenoise.database)
            .await
            .unwrap();

        // Should return relay URLs
        let urls = whitenoise.prepare_key_package_relay_urls(&account).await;
        assert!(urls.is_ok());
        let urls = urls.unwrap();
        assert!(!urls.is_empty());
        assert!(urls.iter().any(|u| u.as_str().contains("test.relay.com")));
    }

    #[tokio::test]
    async fn test_prepare_key_package_relay_urls_empty_relays_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an account without any key package relays using test helper
        let (account, _keys) = create_test_account(&whitenoise).await;

        // Don't add any key package relays - account from create_test_account has no relays

        // Should fail with AccountMissingKeyPackageRelays error
        let result = whitenoise.prepare_key_package_relay_urls(&account).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            WhitenoiseError::AccountMissingKeyPackageRelays => {}
            other => panic!(
                "Expected AccountMissingKeyPackageRelays error, got: {:?}",
                other
            ),
        }
    }

    #[tokio::test]
    async fn test_publish_key_package_without_relays_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an account without any key package relays using test helper
        let (account, _keys) = create_test_account(&whitenoise).await;

        // Attempt to publish key package without relays
        let result = whitenoise.publish_key_package_for_account(&account).await;

        // Should fail with AccountMissingKeyPackageRelays error
        assert!(result.is_err());
        match result.unwrap_err() {
            WhitenoiseError::AccountMissingKeyPackageRelays => {}
            other => panic!(
                "Expected AccountMissingKeyPackageRelays error, got: {:?}",
                other
            ),
        }
    }

    /// Creates a mock key package event with the encoding tag
    fn create_key_package_event_with_encoding_tag(keys: &Keys) -> Event {
        EventBuilder::new(Kind::MlsKeyPackage, "test_content")
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .sign_with_keys(keys)
            .unwrap()
    }

    /// Creates a mock key package event without the encoding tag (outdated)
    fn create_key_package_event_without_encoding_tag(keys: &Keys) -> Event {
        EventBuilder::new(Kind::MlsKeyPackage, "test_content")
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn test_has_encoding_tag_returns_true_when_present() {
        let keys = Keys::generate();
        let event = create_key_package_event_with_encoding_tag(&keys);

        assert!(
            has_encoding_tag(&event),
            "Should return true when encoding tag is present"
        );
    }

    #[test]
    fn test_has_encoding_tag_returns_false_when_missing() {
        let keys = Keys::generate();
        let event = create_key_package_event_without_encoding_tag(&keys);

        assert!(
            !has_encoding_tag(&event),
            "Should return false when encoding tag is missing"
        );
    }

    #[test]
    fn test_has_encoding_tag_returns_false_for_wrong_value() {
        let keys = Keys::generate();
        // Create event with encoding tag but wrong value
        let event = EventBuilder::new(Kind::MlsKeyPackage, "test_content")
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["hex"]))
            .sign_with_keys(&keys)
            .unwrap();

        assert!(
            !has_encoding_tag(&event),
            "Should return false when encoding tag has wrong value"
        );
    }

    #[test]
    fn test_find_outdated_packages_returns_only_packages_without_tag() {
        let keys = Keys::generate();
        let with_tag = create_key_package_event_with_encoding_tag(&keys);
        let without_tag = create_key_package_event_without_encoding_tag(&keys);

        let packages = vec![with_tag.clone(), without_tag.clone()];
        let outdated = find_outdated_packages(&packages);

        assert_eq!(
            outdated.len(),
            1,
            "Should find exactly one outdated package"
        );
        assert_eq!(
            outdated[0].id, without_tag.id,
            "Outdated package should be the one without encoding tag"
        );
    }

    #[test]
    fn test_find_outdated_packages_returns_empty_when_all_have_tag() {
        let keys = Keys::generate();
        let event1 = create_key_package_event_with_encoding_tag(&keys);
        let event2 = create_key_package_event_with_encoding_tag(&keys);

        let packages = vec![event1, event2];
        let outdated = find_outdated_packages(&packages);

        assert!(
            outdated.is_empty(),
            "Should return empty when all packages have encoding tag"
        );
    }

    #[test]
    fn test_find_outdated_packages_returns_all_when_none_have_tag() {
        let keys = Keys::generate();
        let event1 = create_key_package_event_without_encoding_tag(&keys);
        let event2 = create_key_package_event_without_encoding_tag(&keys);

        let packages = vec![event1, event2];
        let outdated = find_outdated_packages(&packages);

        assert_eq!(
            outdated.len(),
            2,
            "Should return all packages when none have encoding tag"
        );
    }

    #[test]
    fn test_find_outdated_packages_handles_empty_list() {
        let packages: Vec<Event> = vec![];
        let outdated = find_outdated_packages(&packages);

        assert!(
            outdated.is_empty(),
            "Should return empty when given empty list"
        );
    }

    #[test]
    fn test_has_encoding_tag_with_multiple_tags() {
        let keys = Keys::generate();
        // Create event with multiple tags including encoding tag
        let event = EventBuilder::new(Kind::MlsKeyPackage, "test_content")
            .tag(Tag::custom(
                TagKind::Custom("mls_protocol_version".into()),
                ["1.0"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                ["0x0001"],
            ))
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .tag(Tag::custom(TagKind::Custom("client".into()), ["MDK/0.5.3"]))
            .sign_with_keys(&keys)
            .unwrap();

        assert!(
            has_encoding_tag(&event),
            "Should find encoding tag among multiple tags"
        );
    }
}
