use std::collections::HashSet;
use std::time::Duration;

use base64ct::{Base64, Encoding};
use mdk_core::key_packages::KeyPackageEventData;
use nostr_sdk::prelude::*;

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::relays::Relay;

/// Maximum number of relay publish attempts before giving up.
const MAX_PUBLISH_ATTEMPTS: u32 = 3;

/// Maximum number of fetch-delete rounds before giving up. This prevents
/// infinite loops when relays keep returning the same key packages after
/// deletion (e.g., because they don't support NIP-09 deletion).
const MAX_DELETE_ROUNDS: u32 = 10;

/// The ciphersuite currently required by Marmot key package tags.
pub(crate) const REQUIRED_MLS_CIPHERSUITE_TAG: &str = "0x0001";

/// Current Nostr event kind for MLS KeyPackage events.
pub(crate) const MLS_KEY_PACKAGE_KIND: Kind = Kind::Custom(30443);

/// Legacy Nostr event kind for MLS KeyPackage events.
pub(crate) const MLS_KEY_PACKAGE_KIND_LEGACY: Kind = Kind::Custom(443);

/// Required extension IDs that must appear in `mls_extensions` tags.
const REQUIRED_MLS_EXTENSION_TAGS: [&str; 2] = ["0x000a", "0xf2ee"];

/// Required proposal IDs that must appear in `mls_proposals` tags.
pub(crate) const REQUIRED_MLS_PROPOSAL_TAGS: [&str; 1] = ["0x000a"];

/// Tag name containing supported MLS proposal IDs.
pub(crate) const MLS_PROPOSALS_TAG_KEY: &str = "mls_proposals";

/// Checks if a key package event has the required encoding tag.
///
/// Per MIP-00/MIP-02, key packages must have an explicit `["encoding", "base64"]` tag.
/// Key packages without this tag are incompatible with current clients.
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

/// Validates that a key package advertises Marmot-required compatibility tags.
///
/// This performs a lightweight pre-check (before MDK add/create operations)
/// so callers can fail early with actionable errors.
pub(crate) fn validate_marmot_key_package_tags(
    event: &Event,
    expected_ciphersuite: &str,
) -> Result<()> {
    if !is_key_package_kind(event.kind) {
        return Err(WhitenoiseError::InvalidEventKind {
            expected: format!("{MLS_KEY_PACKAGE_KIND} or {MLS_KEY_PACKAGE_KIND_LEGACY}"),
            got: event.kind.to_string(),
        });
    }

    if event.kind == MLS_KEY_PACKAGE_KIND && event.tags.identifier().is_none() {
        return Err(WhitenoiseError::MissingKeyPackageDTag);
    }

    if !has_encoding_tag(event) {
        return Err(WhitenoiseError::MissingEncodingTag);
    }

    Base64::decode_vec(&event.content)?;

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
    let missing_extensions: Vec<String> = REQUIRED_MLS_EXTENSION_TAGS
        .into_iter()
        .filter(|required| !extensions.contains(*required))
        .map(|required| required.to_string())
        .collect();

    if !missing_extensions.is_empty() {
        return Err(WhitenoiseError::MissingMlsExtensions {
            missing: missing_extensions,
        });
    }

    let missing_proposals = missing_required_mls_proposals(event);

    if !missing_proposals.is_empty() {
        return Err(WhitenoiseError::MissingMlsProposals {
            missing: missing_proposals,
        });
    }

    Ok(())
}

pub(crate) fn missing_required_mls_proposals(event: &Event) -> Vec<String> {
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

pub(crate) fn is_key_package_kind(kind: Kind) -> bool {
    kind == MLS_KEY_PACKAGE_KIND || kind == MLS_KEY_PACKAGE_KIND_LEGACY
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

/// Filters relay responses to key package events that match the expected kind and author.
///
/// Returns `(valid_events, dropped_wrong_kind, dropped_wrong_author)`.
pub(crate) fn filter_key_package_events_for_account(
    account_pubkey: PublicKey,
    events: Vec<Event>,
) -> (Vec<Event>, usize, usize) {
    let mut valid_events = Vec::new();
    let mut dropped_wrong_kind = 0;
    let mut dropped_wrong_author = 0;

    for event in events {
        if !is_key_package_kind(event.kind) {
            dropped_wrong_kind += 1;
            continue;
        }

        if event.pubkey != account_pubkey {
            dropped_wrong_author += 1;
            continue;
        }

        valid_events.push(event);
    }

    (valid_events, dropped_wrong_kind, dropped_wrong_author)
}

impl Whitenoise {
    /// Helper method to create and encode a key package for the given account.
    ///
    /// Returns a [`KeyPackageEventData`] containing the encoded content, tags
    /// for both event kinds, and the hash_ref for lifecycle tracking.
    #[perf_instrument("key_packages")]
    pub(crate) async fn encoded_key_package(
        &self,
        account: &Account,
        key_package_relays: &[Relay],
    ) -> Result<KeyPackageEventData> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;

        let key_package_relay_urls = Relay::urls(key_package_relays);
        let data = mdk
            .create_key_package_for_event(&account.pubkey, key_package_relay_urls)
            .map_err(|e| WhitenoiseError::Configuration(format!("NostrMls error: {}", e)))?;

        Ok(data)
    }

    /// Publishes the MLS key package for the given account to its key package relays.
    ///
    /// Creates a single MLS key package, then retries relay publishing up to
    /// 3 times with exponential backoff (2s, 4s) if publishing fails. The key
    /// package is created only once to avoid orphaning unused key material in
    /// local MLS storage.
    #[perf_instrument("key_packages")]
    pub async fn publish_key_package_for_account(&self, account: &Account) -> Result<()> {
        let relays = account.key_package_relays(self).await?;

        if relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        // Create the key package once — retries below only re-publish the same payload
        let key_package_data = self.encoded_key_package(account, &relays).await?;
        let relay_urls = Relay::urls(&relays);
        let signer = self.get_signer_for_account(account)?;

        let mut last_error = None;

        for attempt in 0..MAX_PUBLISH_ATTEMPTS {
            if attempt > 0 {
                let delay = Duration::from_secs(1 << attempt);
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "Retrying key package publish for account {} (attempt {}/{})",
                    account.pubkey.to_hex(),
                    attempt + 1,
                    MAX_PUBLISH_ATTEMPTS,
                );
                tokio::time::sleep(delay).await;
            }

            match self
                .publish_key_package_pair_to_relays(
                    account,
                    &key_package_data,
                    &relay_urls,
                    signer.clone(),
                )
                .await
            {
                Ok(()) => {
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::key_packages",
                        "Key package publish attempt {}/{} failed for account {}: {}",
                        attempt + 1,
                        MAX_PUBLISH_ATTEMPTS,
                        account.pubkey.to_hex(),
                        e,
                    );
                    last_error = Some(e);
                }
            }
        }

        // `last_error` is always `Some` here because the loop runs at least once
        Err(last_error.expect("loop ran at least once"))
    }

    /// Publishes the MLS key package using an external signer.
    ///
    /// This is used for external signer accounts (like Amber/NIP-55) where the
    /// private key is not available locally. The signer is used to sign the
    /// key package event before publishing.
    #[perf_instrument("key_packages")]
    pub async fn publish_key_package_for_account_with_signer(
        &self,
        account: &Account,
        signer: impl NostrSigner + 'static,
    ) -> Result<()> {
        let relays = account.key_package_relays(self).await?;

        if relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        let key_package_data = self.encoded_key_package(account, &relays).await?;
        let relay_urls = Relay::urls(&relays);
        self.publish_key_package_pair_to_relays(
            account,
            &key_package_data,
            &relay_urls,
            std::sync::Arc::new(signer),
        )
        .await?;
        Ok(())
    }

    /// Creates a new MLS key package for the account and publishes it to the given relays.
    ///
    /// This is a convenience wrapper that calls [`Self::encoded_key_package`] followed by
    /// [`Self::publish_key_package_to_relays`]. If you need retry semantics, prefer calling
    /// those two methods separately so the key package is only created once.
    #[perf_instrument("key_packages")]
    pub(crate) async fn create_and_publish_key_package(
        &self,
        account: &Account,
        relays: &[Relay],
    ) -> Result<()> {
        let key_package_data = self.encoded_key_package(account, relays).await?;
        let relay_urls = Relay::urls(relays);
        let signer = self.get_signer_for_account(account)?;
        self.publish_key_package_pair_to_relays(account, &key_package_data, &relay_urls, signer)
            .await?;
        Ok(())
    }

    /// Like [`Self::create_and_publish_key_package`], but the relay publish
    /// runs in a background task so the caller isn't blocked by network latency.
    ///
    /// The MLS key material is always created synchronously (local, fast).
    /// Only the relay broadcast + DB tracking are deferred.  If the global
    /// [`Whitenoise`] singleton isn't available (unit tests), the publish
    /// runs inline as a fallback.
    ///
    /// Failures are non-fatal — the `KeyPackageMaintenance` scheduler
    /// (10-min interval) retries any that didn't land.
    pub(crate) async fn create_key_package_and_background_publish(
        &self,
        account: &Account,
        relays: &[Relay],
    ) -> Result<()> {
        let key_package_data = self.encoded_key_package(account, relays).await?;
        let relay_urls = Relay::urls(relays);
        let signer = self.get_signer_for_account(account)?;

        if Whitenoise::get_instance().is_ok() {
            let account = account.clone();
            tokio::spawn(async move {
                let wn = match Whitenoise::get_instance() {
                    Ok(wn) => wn,
                    Err(e) => {
                        tracing::error!(
                            target: "whitenoise::key_packages",
                            "Failed to get Whitenoise instance for background key package publish: {}",
                            e
                        );
                        return;
                    }
                };
                match wn
                    .publish_key_package_pair_to_relays(
                        &account,
                        &key_package_data,
                        &relay_urls,
                        signer,
                    )
                    .await
                {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::warn!(
                            target: "whitenoise::key_packages",
                            "Background key package publish failed, scheduler will retry: {}",
                            e
                        );
                    }
                }
            });
        } else {
            // Synchronous fallback (unit tests or pre-initialization).
            match self
                .publish_key_package_pair_to_relays(account, &key_package_data, &relay_urls, signer)
                .await
            {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::key_packages",
                        "Key package publish failed, scheduler will retry: {}",
                        e
                    );
                }
            }
        }

        Ok(())
    }

    #[perf_instrument("key_packages")]
    async fn publish_key_package_pair_to_relays(
        &self,
        account: &Account,
        key_package_data: &KeyPackageEventData,
        relay_urls: &[RelayUrl],
        signer: std::sync::Arc<dyn NostrSigner>,
    ) -> Result<()> {
        let canonical_event_id = self
            .publish_key_package_to_relays(
                MLS_KEY_PACKAGE_KIND,
                &key_package_data.content,
                relay_urls,
                &key_package_data.tags_30443,
                signer.clone(),
            )
            .await?;

        self.track_published_key_package(
            account,
            &key_package_data.hash_ref,
            &canonical_event_id,
            MLS_KEY_PACKAGE_KIND,
            Some(&key_package_data.d_tag),
        )
        .await;

        match self
            .publish_key_package_to_relays(
                MLS_KEY_PACKAGE_KIND_LEGACY,
                &key_package_data.content,
                relay_urls,
                &key_package_data.tags_443,
                signer,
            )
            .await
        {
            Ok(legacy_event_id) => {
                self.track_published_key_package(
                    account,
                    &key_package_data.hash_ref,
                    &legacy_event_id,
                    MLS_KEY_PACKAGE_KIND_LEGACY,
                    None,
                )
                .await;
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "Published canonical kind:30443 key package for account {} but failed \
                     to publish legacy kind:443 twin: {}",
                    account.pubkey.to_hex(),
                    e,
                );
            }
        }

        Ok(())
    }

    /// Publishes an already-encoded key package event to the given relays.
    ///
    /// Returns an error if no relay accepted the event. This method is
    /// intentionally separated from key package creation so callers can retry
    /// the relay publish without generating additional MLS key material.
    #[perf_instrument("key_packages")]
    async fn publish_key_package_to_relays(
        &self,
        kind: Kind,
        encoded_key_package: &str,
        relay_urls: &[RelayUrl],
        tags: &[Tag],
        signer: std::sync::Arc<dyn NostrSigner>,
    ) -> Result<EventId> {
        let result = self
            .relay_control
            .publish_key_package_with_signer(kind, encoded_key_package, relay_urls, tags, signer)
            .await?;

        if result.success.is_empty() {
            return Err(WhitenoiseError::KeyPackagePublishFailed(
                "no relay accepted the key package event".to_string(),
            ));
        }

        tracing::debug!(
            target: "whitenoise::key_packages",
            "Published kind:{} key package to {} relay(s)",
            kind.as_u16(),
            result.success.len(),
        );

        Ok(*result.id())
    }

    /// Records a successfully published key package in the lifecycle tracking table.
    #[perf_instrument("key_packages")]
    async fn track_published_key_package(
        &self,
        account: &Account,
        hash_ref: &[u8],
        event_id: &EventId,
        kind: Kind,
        d_tag: Option<&str>,
    ) {
        if let Err(e) = PublishedKeyPackage::create(
            &account.pubkey,
            hash_ref,
            &event_id.to_hex(),
            kind,
            d_tag,
            &self.database,
        )
        .await
        {
            tracing::warn!(
                target: "whitenoise::key_packages",
                "Published key package but failed to track it: {}",
                e
            );
        }
    }

    /// Deletes the key package from the relays for the given account.
    ///
    /// Automatically uses the appropriate signer for the account:
    /// - For external accounts (Amber/NIP-55): uses the registered external signer
    /// - For local accounts: uses keys from the secrets store
    ///
    /// Returns `true` if a key package was found and deleted, `false` if no key package was found.
    #[perf_instrument("key_packages")]
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
    #[perf_instrument("key_packages")]
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
            std::sync::Arc::new(signer),
        )
        .await
    }

    #[perf_instrument("key_packages")]
    async fn delete_key_package_for_account_internal(
        &self,
        account: &Account,
        event_id: &EventId,
        delete_mls_stored_keys: bool,
        signer: std::sync::Arc<dyn NostrSigner>,
    ) -> Result<bool> {
        let published_package = match PublishedKeyPackage::find_by_event_id(
            &account.pubkey,
            &event_id.to_hex(),
            &self.database,
        )
        .await
        {
            Ok(package) => package,
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "Failed to look up published key package for event {}: {}",
                    event_id,
                    e
                );
                None
            }
        };

        // Delete local MLS key material using the hash_ref stored at publish time.
        // This avoids a relay round-trip to fetch and parse the key package event.
        if delete_mls_stored_keys {
            match &published_package {
                Some(pkg) if !pkg.key_material_deleted => {
                    let mdk = self.create_mdk_for_account(account.pubkey)?;
                    mdk.delete_key_package_from_storage_by_hash_ref(&pkg.key_package_hash_ref)?;
                    if let Err(e) = PublishedKeyPackage::mark_key_material_deleted_by_hash_ref(
                        &account.pubkey,
                        &pkg.key_package_hash_ref,
                        &self.database,
                    )
                    .await
                    {
                        tracing::warn!(
                            target: "whitenoise::key_packages",
                            "Deleted key material but failed to mark hash_ref group: {}",
                            e
                        );
                    }
                }
                Some(_) => {
                    tracing::debug!(
                        target: "whitenoise::key_packages",
                        "Key material already deleted for event {}, skipping local deletion",
                        event_id
                    );
                }
                None => {
                    tracing::warn!(
                        target: "whitenoise::key_packages",
                        "No published key package record found for event {}, cannot delete local key material",
                        event_id
                    );
                }
            }
        }

        let key_package_relays = account.key_package_relays(self).await?;
        if key_package_relays.is_empty() {
            return Ok(false);
        }

        let key_package_relays_urls = Relay::urls(&key_package_relays);
        let event_ids = self
            .key_package_event_ids_for_deletion(account, event_id, published_package.as_ref())
            .await;

        let result = self
            .relay_control
            .publish_batch_event_deletion_with_signer(&event_ids, &key_package_relays_urls, signer)
            .await?;
        Ok(!result.success.is_empty())
    }

    async fn key_package_event_ids_for_deletion(
        &self,
        account: &Account,
        event_id: &EventId,
        published_package: Option<&PublishedKeyPackage>,
    ) -> Vec<EventId> {
        let mut seen = HashSet::new();
        let mut event_ids = Vec::new();
        if seen.insert(*event_id) {
            event_ids.push(*event_id);
        }

        let Some(package) = published_package else {
            return event_ids;
        };

        match PublishedKeyPackage::find_by_hash_ref(
            &account.pubkey,
            &package.key_package_hash_ref,
            &self.database,
        )
        .await
        {
            Ok(packages) => {
                for package in packages {
                    match EventId::from_hex(&package.event_id) {
                        Ok(package_event_id) if seen.insert(package_event_id) => {
                            event_ids.push(package_event_id);
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                target: "whitenoise::key_packages",
                                "Skipping invalid tracked key package event id {}: {}",
                                package.event_id,
                                e
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "Failed to find key package hash_ref twins for event {}: {}",
                    event_id,
                    e
                );
            }
        }

        event_ids
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
    #[perf_instrument("key_packages")]
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
            .kinds([MLS_KEY_PACKAGE_KIND, MLS_KEY_PACKAGE_KIND_LEGACY])
            .author(account.pubkey);

        let fetched = self
            .relay_control
            .ephemeral()
            .fetch_events_from(&relay_urls, key_package_filter)
            .await?;
        let key_package_events: Vec<Event> = fetched.into_iter().collect();

        let (key_package_events, dropped_wrong_kind, dropped_wrong_author) =
            filter_key_package_events_for_account(account.pubkey, key_package_events);

        let total_dropped = dropped_wrong_kind + dropped_wrong_author;
        if total_dropped > 0 {
            tracing::warn!(
                target: "whitenoise::key_packages",
                "Dropped {} off-filter event(s) while fetching key packages for account {} \
                 (wrong kind: {}, wrong author: {})",
                total_dropped,
                account.pubkey.to_hex(),
                dropped_wrong_kind,
                dropped_wrong_author,
            );
        }

        tracing::debug!(
            target: "whitenoise::key_packages",
            "Found {} valid key package event(s) for account {}",
            key_package_events.len(),
            account.pubkey.to_hex()
        );

        Ok(key_package_events)
    }

    /// Deletes all legacy key package events from relays for the given account.
    ///
    /// This developer-facing cleanup keeps canonical replaceable kind:30443 key packages
    /// intact and only removes legacy kind:443 copies. Shared local MLS key material
    /// is not deleted from this legacy-only path because kind:443 and kind:30443
    /// twins can reference the same `hash_ref`.
    ///
    /// Automatically uses the appropriate signer for the account:
    /// - For external accounts (Amber/NIP-55): uses the registered external signer
    /// - For local accounts: uses keys from the secrets store
    ///
    /// # Returns
    ///
    /// Returns the number of legacy key packages that were successfully deleted.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Account has no key package relays configured
    /// - Failed to retrieve account's key package relays
    /// - Failed to get signing keys for the account
    /// - Network error while fetching or publishing events
    /// - Batch deletion event publishing failed
    #[perf_instrument("key_packages")]
    pub async fn delete_legacy_key_packages_for_account(&self, account: &Account) -> Result<usize> {
        let signer = self.get_signer_for_account(account)?;
        self.delete_legacy_key_packages_loop(account, signer).await
    }

    /// Deletes all legacy key package events from relays using an external signer.
    ///
    /// This is used for external signer accounts (like Amber/NIP-55) where the
    /// private key is not available locally. The signer is used to sign the
    /// deletion events before publishing.
    ///
    /// # Returns
    ///
    /// Returns the number of legacy key packages that were successfully deleted.
    #[perf_instrument("key_packages")]
    pub async fn delete_legacy_key_packages_for_account_with_signer(
        &self,
        account: &Account,
        signer: impl NostrSigner + 'static,
    ) -> Result<usize> {
        self.delete_legacy_key_packages_loop(account, std::sync::Arc::new(signer))
            .await
    }

    /// Loops fetch-delete rounds until no legacy key packages remain on relays, up to
    /// [`MAX_DELETE_ROUNDS`]. This handles NIP-01 pagination: relays may return
    /// only a subset of key packages per query, so a single fetch-delete pass
    /// can leave packages behind.
    #[perf_instrument("key_packages")]
    async fn delete_legacy_key_packages_loop(
        &self,
        account: &Account,
        signer: std::sync::Arc<dyn NostrSigner>,
    ) -> Result<usize> {
        let mut total_deleted = 0;

        for round in 0..MAX_DELETE_ROUNDS {
            let key_package_events = Self::legacy_key_package_events(
                self.fetch_all_key_packages_for_account(account).await?,
            );

            if key_package_events.is_empty() {
                tracing::info!(
                    target: "whitenoise::key_packages",
                    "All legacy key packages deleted for account {} \
                     ({} total across {} round(s))",
                    account.pubkey.to_hex(),
                    total_deleted,
                    round + 1,
                );
                return Ok(total_deleted);
            }

            tracing::debug!(
                target: "whitenoise::key_packages",
                "Round {}: found {} remaining legacy key package(s) for account {}",
                round + 1,
                key_package_events.len(),
                account.pubkey.to_hex(),
            );

            let batch_size = key_package_events.len();

            let deleted = self
                .delete_key_packages_for_account_internal(
                    account,
                    key_package_events,
                    false,
                    1,
                    signer.clone(),
                )
                .await?;

            total_deleted += deleted;

            // If nothing was deleted this round, relays are not
            // cooperating (e.g. they don't support NIP-09 deletion).
            if deleted == 0 {
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "Round {} deleted 0 legacy key packages despite {} found \
                     — relays may not support deletion",
                    round + 1,
                    batch_size,
                );
                break;
            }
        }

        Ok(total_deleted)
    }

    fn legacy_key_package_events(events: Vec<Event>) -> Vec<Event> {
        events
            .into_iter()
            .filter(|event| event.kind == MLS_KEY_PACKAGE_KIND_LEGACY)
            .collect()
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
    #[perf_instrument("key_packages")]
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

    #[perf_instrument("key_packages")]
    async fn delete_key_packages_for_account_internal(
        &self,
        account: &Account,
        key_package_events: Vec<Event>,
        delete_mls_stored_keys: bool,
        max_retries: u32,
        signer: std::sync::Arc<dyn NostrSigner>,
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
            self.delete_key_packages_from_storage(account, &key_package_events, original_count)
                .await?;
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

    #[perf_instrument("key_packages")]
    async fn prepare_key_package_relay_urls(&self, account: &Account) -> Result<Vec<RelayUrl>> {
        let key_package_relays = account.key_package_relays(self).await?;

        if key_package_relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        Ok(Relay::urls(&key_package_relays))
    }

    async fn delete_key_packages_from_storage(
        &self,
        account: &Account,
        key_package_events: &[Event],
        initial_count: usize,
    ) -> Result<()> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let mut deleted_hash_refs = HashSet::new();
        let mut deleted_untracked_contents = HashSet::new();
        let mut storage_delete_count = 0;

        for event in key_package_events {
            match PublishedKeyPackage::find_by_event_id(
                &account.pubkey,
                &event.id.to_hex(),
                &self.database,
            )
            .await
            {
                Ok(Some(pkg)) => {
                    if !deleted_hash_refs.insert(pkg.key_package_hash_ref.clone()) {
                        continue;
                    }

                    match mdk.delete_key_package_from_storage_by_hash_ref(&pkg.key_package_hash_ref)
                    {
                        Ok(()) => {
                            storage_delete_count += 1;
                            if let Err(e) =
                                PublishedKeyPackage::mark_key_material_deleted_by_hash_ref(
                                    &account.pubkey,
                                    &pkg.key_package_hash_ref,
                                    &self.database,
                                )
                                .await
                            {
                                tracing::warn!(
                                    target: "whitenoise::key_packages",
                                    "Deleted key material but failed to mark hash_ref group: {}",
                                    e
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "whitenoise::key_packages",
                                "Failed to delete key package from storage for event {}: {}",
                                event.id,
                                e
                            );
                        }
                    }
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::key_packages",
                        "Failed to look up published key package for event {}: {}",
                        event.id,
                        e
                    );
                }
            }

            if !deleted_untracked_contents.insert(event.content.clone()) {
                continue;
            }

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

    #[perf_instrument("key_packages")]
    async fn publish_key_package_deletion_with_signer(
        &self,
        event_ids: &[EventId],
        relay_urls: &[RelayUrl],
        signer: std::sync::Arc<dyn NostrSigner>,
        context: &str,
    ) -> Result<()> {
        match self
            .relay_control
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

    /// Looks up a published key package record by account and event ID.
    ///
    /// Integration-test helper for verifying lifecycle states (consumed_at,
    /// key_material_deleted) without exposing the raw database handle.
    #[cfg(feature = "integration-tests")]
    pub async fn find_published_key_package_for_testing(
        &self,
        account_pubkey: &nostr_sdk::PublicKey,
        event_id: &str,
    ) -> Result<Option<PublishedKeyPackage>> {
        PublishedKeyPackage::find_by_event_id(account_pubkey, event_id, &self.database)
            .await
            .map_err(WhitenoiseError::from)
    }

    /// Records a published key package in the lifecycle tracking table.
    ///
    /// Integration-test helper for cases that manually publish custom key
    /// package events (for example with a backdated timestamp) and still need
    /// maintenance to treat them as locally-owned packages. Callers pass the
    /// event kind explicitly so canonical and legacy test fixtures match the
    /// event that was actually published.
    #[cfg(feature = "integration-tests")]
    pub async fn track_published_key_package_for_testing(
        &self,
        account_pubkey: &nostr_sdk::PublicKey,
        hash_ref: &[u8],
        event_id: &str,
        kind: Kind,
        d_tag: Option<&str>,
    ) -> Result<()> {
        PublishedKeyPackage::create(
            account_pubkey,
            hash_ref,
            event_id,
            kind,
            d_tag,
            &self.database,
        )
        .await
        .map_err(WhitenoiseError::from)
    }

    /// Backdates the `consumed_at` timestamp for a published key package.
    ///
    /// Integration-test helper that shifts `consumed_at` into the past so the
    /// maintenance task considers it eligible for cleanup without waiting the
    /// full quiet period.
    #[cfg(feature = "integration-tests")]
    pub async fn backdate_consumed_at_for_testing(
        &self,
        account_pubkey: &nostr_sdk::PublicKey,
        event_id: &str,
        age_secs: i64,
    ) -> Result<()> {
        let package =
            PublishedKeyPackage::find_by_event_id(account_pubkey, event_id, &self.database)
                .await?
                .ok_or_else(|| {
                    WhitenoiseError::Internal(format!(
                        "Published key package not found: {event_id}"
                    ))
                })?;

        sqlx::query(
            "UPDATE published_key_packages SET consumed_at = unixepoch() - ?
             WHERE account_pubkey = ? AND key_package_hash_ref = ?",
        )
        .bind(age_secs)
        .bind(account_pubkey.to_hex())
        .bind(&package.key_package_hash_ref)
        .execute(&self.database.pool)
        .await
        .map_err(crate::whitenoise::database::DatabaseError::Sqlx)?;

        tracing::debug!(
            target: "whitenoise::key_packages",
            "Backdated consumed_at by {}s for KP hash_ref group containing event {} (TEST ONLY)",
            age_secs,
            event_id
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use nostr_sdk::{EventBuilder, Keys, Kind, Tag, TagKind};

    use super::*;
    use crate::whitenoise::accounts::AccountType;
    use crate::whitenoise::test_utils::*;

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

    /// Creates a persisted account with a key package relay and stored keys.
    async fn create_account_with_relay(whitenoise: &Whitenoise) -> Account {
        let (account, _keys) = create_account_with_relay_and_keys(whitenoise).await;
        account
    }

    async fn create_account_with_relay_and_keys(whitenoise: &Whitenoise) -> (Account, Keys) {
        let (account, keys) = create_test_account(whitenoise).await;
        let account = account.save(&whitenoise.database).await.unwrap();
        whitenoise
            .secrets_store
            .store_private_key(&keys)
            .expect("Should store keys");
        let user = account.user(&whitenoise.database).await.unwrap();
        // Use a loopback IP so connection refusal is instant (no DNS lookup).
        let relay = crate::whitenoise::relays::Relay::find_or_create_by_url(
            &RelayUrl::parse("ws://127.0.0.1:1").unwrap(),
            &whitenoise.database,
        )
        .await
        .unwrap();
        user.add_relay(&relay, crate::RelayType::KeyPackage, &whitenoise.database)
            .await
            .unwrap();
        (account, keys)
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

        // Insert external signer directly (bypasses account-type validation)
        whitenoise
            .insert_external_signer(pubkey, keys.clone())
            .await
            .unwrap();

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

        // Create account with both local keys and a registered external signer.
        // Both use the same key material because insert_external_signer validates
        // that the signer pubkey matches. The point of this test is to verify
        // the external-signer map takes priority over the secrets store.
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Store keys in the secrets store (simulates a local account)
        whitenoise
            .secrets_store
            .store_private_key(&keys)
            .expect("Should store keys");

        // Also register the same keys as an external signer
        whitenoise
            .insert_external_signer(pubkey, keys.clone())
            .await
            .unwrap();

        let account = Account {
            id: Some(1),
            pubkey,
            user_id: 1,
            account_type: AccountType::Local, // Even for local account type
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // Should prefer the external signer entry over secrets-store lookup
        let signer = whitenoise.get_signer_for_account(&account).unwrap();
        let signer_pubkey = signer.get_public_key().await.unwrap();

        assert_eq!(
            signer_pubkey, pubkey,
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
        EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "test_content")
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .sign_with_keys(keys)
            .unwrap()
    }

    /// Creates a mock key package event without the encoding tag.
    fn create_key_package_event_without_encoding_tag(keys: &Keys) -> Event {
        EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "test_content")
            .sign_with_keys(keys)
            .unwrap()
    }

    fn create_key_package_event_with_compatibility_tags(
        keys: &Keys,
        ciphersuite: &str,
        extensions: &[&str],
    ) -> Event {
        EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                [ciphersuite],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                extensions.iter().copied(),
            ))
            .tag(Tag::custom(
                TagKind::Custom(MLS_PROPOSALS_TAG_KEY.into()),
                ["0x000a"],
            ))
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .sign_with_keys(keys)
            .unwrap()
    }

    /// Creates a non-key-package event for filtering tests
    fn create_non_key_package_event(keys: &Keys) -> Event {
        EventBuilder::new(Kind::TextNote, "test_content")
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn test_legacy_key_package_events_filters_to_kind_443() {
        let keys = Keys::generate();
        let canonical = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "canonical")
            .sign_with_keys(&keys)
            .unwrap();
        let legacy = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "legacy")
            .sign_with_keys(&keys)
            .unwrap();
        let text_note = create_non_key_package_event(&keys);

        let filtered =
            Whitenoise::legacy_key_package_events(vec![canonical, legacy.clone(), text_note]);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, legacy.id);
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
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "test_content")
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["hex"]))
            .sign_with_keys(&keys)
            .unwrap();

        assert!(
            !has_encoding_tag(&event),
            "Should return false when encoding tag has wrong value"
        );
    }

    #[test]
    fn test_validate_marmot_key_package_tags_accepts_required_tags() {
        let keys = Keys::generate();
        let event = create_key_package_event_with_compatibility_tags(
            &keys,
            REQUIRED_MLS_CIPHERSUITE_TAG,
            &["0x000a", "0xF2EE"],
        );

        let result = validate_marmot_key_package_tags(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_ok(), "Expected valid compatibility tags");
    }

    #[test]
    fn test_validate_marmot_key_package_tags_accepts_current_kind() {
        let keys = Keys::generate();
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "dGVzdF9jb250ZW50")
            .tag(Tag::identifier(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                [REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a", "0xf2ee"],
            ))
            .tag(Tag::custom(
                TagKind::Custom(MLS_PROPOSALS_TAG_KEY.into()),
                ["0x000a"],
            ))
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .sign_with_keys(&keys)
            .unwrap();

        let result = validate_marmot_key_package_tags(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(
            result.is_ok(),
            "Expected current kind key package to validate"
        );
    }

    #[test]
    fn test_validate_marmot_key_package_tags_rejects_current_kind_without_d_tag() {
        let keys = Keys::generate();
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                [REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a", "0xf2ee"],
            ))
            .tag(Tag::custom(
                TagKind::Custom(MLS_PROPOSALS_TAG_KEY.into()),
                ["0x000a"],
            ))
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .sign_with_keys(&keys)
            .unwrap();

        let result = validate_marmot_key_package_tags(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(matches!(
            result,
            Err(WhitenoiseError::MissingKeyPackageDTag)
        ));
    }

    #[test]
    fn test_validate_marmot_key_package_tags_rejects_wrong_ciphersuite() {
        let keys = Keys::generate();
        let event = create_key_package_event_with_compatibility_tags(
            &keys,
            "0x0002",
            &["0x000a", "0xF2EE"],
        );

        let result = validate_marmot_key_package_tags(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_err(), "Expected ciphersuite mismatch to fail");
        assert!(matches!(
            result,
            Err(WhitenoiseError::IncompatibleMlsCiphersuite { .. })
        ));
    }

    #[test]
    fn test_validate_marmot_key_package_tags_rejects_missing_extensions() {
        let keys = Keys::generate();
        let event = create_key_package_event_with_compatibility_tags(
            &keys,
            REQUIRED_MLS_CIPHERSUITE_TAG,
            &["0x000a"],
        );

        let result = validate_marmot_key_package_tags(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_err(), "Expected missing extension to fail");
        assert!(matches!(
            result,
            Err(WhitenoiseError::MissingMlsExtensions { .. })
        ));
    }

    #[test]
    fn test_validate_marmot_key_package_tags_rejects_invalid_base64_content() {
        let keys = Keys::generate();
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "not-base64$$$")
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                [REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a", "0xf2ee"],
            ))
            .tag(Tag::custom(
                TagKind::Custom(MLS_PROPOSALS_TAG_KEY.into()),
                ["0x000a"],
            ))
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .sign_with_keys(&keys)
            .unwrap();

        let result = validate_marmot_key_package_tags(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_err(), "Expected invalid base64 content to fail");
        assert!(matches!(result, Err(WhitenoiseError::InvalidBase64(_))));
    }

    #[test]
    fn test_validate_marmot_key_package_tags_rejects_wrong_kind() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::TextNote, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                [REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a", "0xf2ee"],
            ))
            .tag(Tag::custom(
                TagKind::Custom(MLS_PROPOSALS_TAG_KEY.into()),
                ["0x000a"],
            ))
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .sign_with_keys(&keys)
            .unwrap();

        let result = validate_marmot_key_package_tags(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_err(), "Expected wrong kind to fail");
        assert!(matches!(
            result,
            Err(WhitenoiseError::InvalidEventKind { .. })
        ));
    }

    #[test]
    fn test_validate_marmot_key_package_tags_rejects_missing_encoding_tag() {
        let keys = Keys::generate();
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                [REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a", "0xf2ee"],
            ))
            .sign_with_keys(&keys)
            .unwrap();

        let result = validate_marmot_key_package_tags(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_err(), "Expected missing encoding tag to fail");
        assert!(matches!(result, Err(WhitenoiseError::MissingEncodingTag)));
    }

    #[test]
    fn test_validate_marmot_key_package_tags_rejects_missing_self_remove_proposal() {
        let keys = Keys::generate();
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                [REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a", "0xf2ee"],
            ))
            .tag(Tag::custom(
                TagKind::Custom(MLS_PROPOSALS_TAG_KEY.into()),
                ["0x0001"],
            ))
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .sign_with_keys(&keys)
            .unwrap();

        let result = validate_marmot_key_package_tags(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_err(), "Expected missing SelfRemove to fail");
        assert!(matches!(
            result,
            Err(WhitenoiseError::MissingMlsProposals { .. })
        ));
    }

    #[test]
    fn test_filter_key_package_events_for_account_drops_off_filter_events() {
        let account_keys = Keys::generate();
        let other_keys = Keys::generate();

        let valid_with_encoding = create_key_package_event_with_encoding_tag(&account_keys);
        let valid_without_encoding = create_key_package_event_without_encoding_tag(&account_keys);
        let wrong_kind = create_non_key_package_event(&account_keys);
        let wrong_author = create_key_package_event_with_encoding_tag(&other_keys);

        let (filtered, dropped_wrong_kind, dropped_wrong_author) =
            filter_key_package_events_for_account(
                account_keys.public_key(),
                vec![
                    valid_with_encoding.clone(),
                    valid_without_encoding.clone(),
                    wrong_kind,
                    wrong_author,
                ],
            );

        assert_eq!(
            filtered.len(),
            2,
            "Should keep only key package events from the requested author"
        );
        assert!(
            filtered.iter().any(|e| e.id == valid_with_encoding.id),
            "Should keep matching key package with encoding tag"
        );
        assert!(
            filtered.iter().any(|e| e.id == valid_without_encoding.id),
            "Should keep matching key package without encoding tag"
        );
        assert_eq!(
            dropped_wrong_kind, 1,
            "Should count one dropped event with wrong kind"
        );
        assert_eq!(
            dropped_wrong_author, 1,
            "Should count one dropped event with wrong author"
        );
    }

    #[test]
    fn test_filter_key_package_events_for_account_keeps_all_matching_events() {
        let account_keys = Keys::generate();
        let event1 = create_key_package_event_with_encoding_tag(&account_keys);
        let event2 = create_key_package_event_without_encoding_tag(&account_keys);

        let (filtered, dropped_wrong_kind, dropped_wrong_author) =
            filter_key_package_events_for_account(
                account_keys.public_key(),
                vec![event1.clone(), event2.clone()],
            );

        assert_eq!(
            filtered.len(),
            2,
            "Should keep all matching key package events"
        );
        assert_eq!(
            dropped_wrong_kind, 0,
            "Should not drop events for wrong kind when all are key packages"
        );
        assert_eq!(
            dropped_wrong_author, 0,
            "Should not drop events for wrong author when all authors match"
        );
        assert!(
            filtered.iter().any(|e| e.id == event1.id),
            "Should retain first matching key package"
        );
        assert!(
            filtered.iter().any(|e| e.id == event2.id),
            "Should retain second matching key package"
        );
    }

    #[test]
    fn test_has_encoding_tag_with_multiple_tags() {
        let keys = Keys::generate();
        // Create event with multiple tags including encoding tag
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "test_content")
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

    #[tokio::test]
    async fn test_publish_key_package_with_signer_without_relays_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Use create_test_account to get a persisted account (no key package relays)
        let (account, _keys) = create_test_account(&whitenoise).await;
        let signer_keys = Keys::generate();

        let result = whitenoise
            .publish_key_package_for_account_with_signer(&account, signer_keys)
            .await;

        assert!(result.is_err());
        match result.unwrap_err() {
            WhitenoiseError::AccountMissingKeyPackageRelays => {}
            other => panic!(
                "Expected AccountMissingKeyPackageRelays error, got: {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_key_package_publish_failed_error_variant() {
        let err = WhitenoiseError::KeyPackagePublishFailed(
            "no relay accepted the key package event".to_string(),
        );
        assert!(err.to_string().contains("no relay accepted"));
        assert!(matches!(err, WhitenoiseError::KeyPackagePublishFailed(_)));
    }

    #[tokio::test]
    async fn test_publish_key_package_for_account_retries_and_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = create_account_with_relay(&whitenoise).await;

        // Pause time so exponential backoff sleeps complete instantly.
        tokio::time::pause();
        let result = whitenoise.publish_key_package_for_account(&account).await;
        tokio::time::resume();
        assert!(result.is_err(), "Should fail when relay is unreachable");
    }

    #[tokio::test]
    async fn test_create_and_publish_key_package_fails_with_unreachable_relay() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = create_account_with_relay(&whitenoise).await;

        let relays = account.key_package_relays(&whitenoise).await.unwrap();
        // Pause time so exponential backoff sleeps complete instantly.
        tokio::time::pause();
        let result = whitenoise
            .create_and_publish_key_package(&account, &relays)
            .await;
        tokio::time::resume();
        assert!(result.is_err(), "Should fail when relay is unreachable");
    }

    #[tokio::test]
    async fn test_publish_key_package_with_signer_fails_with_unreachable_relay() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = create_account_with_relay(&whitenoise).await;

        // Pause time so exponential backoff sleeps complete instantly.
        tokio::time::pause();
        let result = whitenoise
            .publish_key_package_for_account_with_signer(&account, Keys::generate())
            .await;
        tokio::time::resume();
        assert!(result.is_err(), "Should fail when relay is unreachable");
    }

    #[tokio::test]
    async fn test_fetch_all_key_packages_without_relays_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, _keys) = create_test_account(&whitenoise).await;

        let result = whitenoise
            .fetch_all_key_packages_for_account(&account)
            .await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WhitenoiseError::AccountMissingKeyPackageRelays
        ));
    }

    #[test]
    fn test_max_delete_rounds_is_reasonable() {
        // Safety cap must be high enough to handle pagination but low
        // enough to prevent infinite loops with uncooperative relays.
        assert_eq!(MAX_DELETE_ROUNDS, 10);
    }

    #[tokio::test]
    async fn test_delete_legacy_key_packages_without_signer_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Account without keys in secrets store — signer resolution fails
        let account = create_local_account_struct();

        let result = whitenoise
            .delete_legacy_key_packages_for_account(&account)
            .await;

        assert!(result.is_err(), "Should fail when no signer is available");
    }

    #[tokio::test]
    async fn test_delete_legacy_key_packages_without_relays_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an account with keys stored but no key package relays.
        // Keys are needed because the convergence loop resolves the signer
        // before fetching key packages from relays.
        let (account, keys) = create_test_account(&whitenoise).await;
        whitenoise
            .secrets_store
            .store_private_key(&keys)
            .expect("Should store keys");

        let result = whitenoise
            .delete_legacy_key_packages_for_account(&account)
            .await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WhitenoiseError::AccountMissingKeyPackageRelays
        ));
    }

    #[tokio::test]
    async fn test_delete_legacy_key_packages_with_signer_without_relays_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, _keys) = create_test_account(&whitenoise).await;
        let signer_keys = Keys::generate();

        let result = whitenoise
            .delete_legacy_key_packages_for_account_with_signer(&account, signer_keys)
            .await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WhitenoiseError::AccountMissingKeyPackageRelays
        ));
    }

    #[tokio::test]
    async fn test_delete_key_packages_with_empty_events_returns_zero() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = create_account_with_relay(&whitenoise).await;

        let result = whitenoise
            .delete_key_packages_for_account(&account, vec![], false, 1)
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_delete_key_packages_from_storage_deduplicates_hash_ref_twins() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_account_with_relay_and_keys(&whitenoise).await;
        let relays = account.key_package_relays(&whitenoise).await.unwrap();
        let key_package_data = whitenoise
            .encoded_key_package(&account, &relays)
            .await
            .unwrap();

        let canonical = EventBuilder::new(MLS_KEY_PACKAGE_KIND, &key_package_data.content)
            .tags(key_package_data.tags_30443.to_vec())
            .sign_with_keys(&keys)
            .unwrap();
        let legacy = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, &key_package_data.content)
            .tags(key_package_data.tags_443.to_vec())
            .sign_with_keys(&keys)
            .unwrap();

        PublishedKeyPackage::create(
            &account.pubkey,
            &key_package_data.hash_ref,
            &canonical.id.to_hex(),
            MLS_KEY_PACKAGE_KIND,
            Some(&key_package_data.d_tag),
            &whitenoise.database,
        )
        .await
        .unwrap();
        PublishedKeyPackage::create(
            &account.pubkey,
            &key_package_data.hash_ref,
            &legacy.id.to_hex(),
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &whitenoise.database,
        )
        .await
        .unwrap();

        let invalid_untracked = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "not-a-key-package")
            .sign_with_keys(&keys)
            .unwrap();
        let duplicate_untracked =
            EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "not-a-key-package")
                .sign_with_keys(&keys)
                .unwrap();

        whitenoise
            .delete_key_packages_from_storage(
                &account,
                &[
                    canonical.clone(),
                    legacy.clone(),
                    invalid_untracked,
                    duplicate_untracked,
                ],
                4,
            )
            .await
            .unwrap();

        let canonical_record = PublishedKeyPackage::find_by_event_id(
            &account.pubkey,
            &canonical.id.to_hex(),
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        let legacy_record = PublishedKeyPackage::find_by_event_id(
            &account.pubkey,
            &legacy.id.to_hex(),
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();

        assert!(canonical_record.key_material_deleted);
        assert!(legacy_record.key_material_deleted);
    }

    #[tokio::test]
    async fn test_key_package_event_ids_for_deletion_includes_hash_ref_twins() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_account_with_relay_and_keys(&whitenoise).await;
        let hash_ref = vec![1, 2, 3];

        let canonical = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "canonical")
            .tag(Tag::identifier("delete-twins"))
            .sign_with_keys(&keys)
            .unwrap();
        let legacy = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "legacy")
            .sign_with_keys(&keys)
            .unwrap();

        PublishedKeyPackage::create(
            &account.pubkey,
            &hash_ref,
            &canonical.id.to_hex(),
            MLS_KEY_PACKAGE_KIND,
            Some("delete-twins"),
            &whitenoise.database,
        )
        .await
        .unwrap();
        PublishedKeyPackage::create(
            &account.pubkey,
            &hash_ref,
            &legacy.id.to_hex(),
            MLS_KEY_PACKAGE_KIND_LEGACY,
            None,
            &whitenoise.database,
        )
        .await
        .unwrap();

        let canonical_record = PublishedKeyPackage::find_by_event_id(
            &account.pubkey,
            &canonical.id.to_hex(),
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        let event_ids = whitenoise
            .key_package_event_ids_for_deletion(&account, &canonical.id, Some(&canonical_record))
            .await;

        assert_eq!(event_ids.len(), 2);
        assert!(event_ids.contains(&canonical.id));
        assert!(event_ids.contains(&legacy.id));
    }

    #[tokio::test]
    async fn test_delete_single_key_package_without_signer_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Local account without stored keys — signer resolution fails
        let account = create_local_account_struct();
        let event_id = EventId::all_zeros();

        let result = whitenoise
            .delete_key_package_for_account(&account, &event_id, false)
            .await;
        assert!(result.is_err(), "Should fail when no signer is available");
    }

    #[tokio::test]
    async fn test_delete_single_key_package_with_signer_without_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Account exists but has no key package relays. The internal method
        // streams events first (which returns empty without a real relay),
        // so it returns Ok(false) — no event found, nothing to delete.
        let (account, _keys) = create_test_account(&whitenoise).await;
        let signer_keys = Keys::generate();
        let event_id = EventId::all_zeros();

        let result = whitenoise
            .delete_key_package_for_account_with_signer(&account, &event_id, false, signer_keys)
            .await;

        // stream_events returns empty → Ok(false)
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }
}
