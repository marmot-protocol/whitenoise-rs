use std::collections::HashSet;
use std::time::Duration;

use base64ct::{Base64, Encoding};
use nostr_sdk::prelude::*;

pub(crate) use crate::marmot::key_packages::{MLS_PROPOSALS_TAG_KEY, REQUIRED_MLS_PROPOSAL_TAGS};
use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::relays::Relay;
use crate::whitenoise::session::key_packages::LocalKeyMaterialDeleteOutcome;

/// The ciphersuite currently required by Marmot key package tags.
pub(crate) const REQUIRED_MLS_CIPHERSUITE_TAG: &str = "0x0001";

/// Current Nostr event kind for MLS KeyPackage events.
pub(crate) const MLS_KEY_PACKAGE_KIND: Kind = Kind::Custom(30443);

/// The legacy MLS Marmot-identity extension advertised on the `mls_extensions`
/// tag by older Marmot key packages.
///
/// This is consumer-facing: legacy peers' KPs must still carry this tag for us
/// to recognise them as Marmot KPs at all. Other capability codepoints
/// (notably SelfRemove `0x000a`) are projected as soft signals, not required.
pub(crate) const REQUIRED_MARMOT_IDENTITY_EXTENSION_TAG: &str = "0xf2ee";

/// Required extension IDs for the strict (self-publish) validator.
///
/// Strict enforcement only: we keep this tighter than the consumer baseline so
/// our own published key packages always carry the SelfRemove extension
/// codepoint. The consumer-side baseline validator
/// ([`validate_marmot_key_package_baseline`]) checks only
/// [`REQUIRED_MARMOT_IDENTITY_EXTENSION_TAG`] (`0xf2ee`).
const STRICT_REQUIRED_MLS_EXTENSION_TAGS: [&str; 2] = ["0x000a", "0xf2ee"];

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
/// This performs a lightweight pre-check before protocol operations
/// so callers can fail early with actionable errors.
pub(crate) fn validate_marmot_key_package_strict(
    event: &Event,
    expected_ciphersuite: &str,
) -> Result<()> {
    if !is_key_package_kind(event.kind) {
        return Err(WhitenoiseError::InvalidEventKind {
            expected: MLS_KEY_PACKAGE_KIND.to_string(),
            got: event.kind.to_string(),
        });
    }

    if event.kind == MLS_KEY_PACKAGE_KIND && event.tags.identifier().is_none() {
        return Err(WhitenoiseError::MissingKeyPackageDTag);
    }

    if crate::marmot::key_packages::is_v2_event(event) {
        return crate::marmot::key_packages::validate_v2_event(event, expected_ciphersuite);
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
    let missing_extensions: Vec<String> = STRICT_REQUIRED_MLS_EXTENSION_TAGS
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
    kind == MLS_KEY_PACKAGE_KIND
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

/// Validates that a key package advertises the minimum Marmot-identity extension tag.
///
/// This is the *consumer-facing* baseline: it checks only that the Marmot identity
/// extension (`0xf2ee`) is present — it does NOT require the SelfRemove proposal
/// tag. Use [`validate_marmot_key_package_strict`] for self-publish validation.
pub(crate) fn validate_marmot_key_package_baseline(
    event: &Event,
    expected_ciphersuite: &str,
) -> Result<()> {
    if !is_key_package_kind(event.kind) {
        return Err(WhitenoiseError::InvalidEventKind {
            expected: MLS_KEY_PACKAGE_KIND.to_string(),
            got: event.kind.to_string(),
        });
    }

    if crate::marmot::key_packages::is_v2_event(event) {
        return crate::marmot::key_packages::validate_v2_event(event, expected_ciphersuite);
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

    // Baseline only requires the Marmot identity extension (0xf2ee).
    let extensions: HashSet<String> = normalized_tag_values(event, TagKind::MlsExtensions)
        .into_iter()
        .collect();
    let required = [REQUIRED_MARMOT_IDENTITY_EXTENSION_TAG];
    let missing_extensions: Vec<String> = required
        .into_iter()
        .filter(|r| !extensions.contains(*r))
        .map(|r| r.to_string())
        .collect();

    if !missing_extensions.is_empty() {
        return Err(WhitenoiseError::MissingMlsExtensions {
            missing: missing_extensions,
        });
    }

    Ok(())
}

/// Returns a strictly-monotonic `created_at` for the next kind:30443 publish.
///
/// NIP-01 specifies that when two replaceable events share a `created_at`,
/// relays keep the one with the lowest event id. Reusing the d-tag alone
/// does not guarantee that a fresh publish replaces the previous canonical
/// event when both land in the same second — the new event might lose the
/// id-comparison tiebreaker.
///
/// `prev_max` is the maximum `created_at` recorded in the
/// `published_key_packages` table for the canonical kind. That value is the
/// SQLite `unixepoch()` at INSERT time, which happens *after* the relay
/// round-trip, so it is always ≥ the previous Nostr event's `created_at`.
/// Setting the new event's `created_at` to `max(now, prev_max + 1)`
/// therefore lands strictly above any prior canonical event and sidesteps
/// the NIP-01 tie entirely.
pub(crate) fn monotonic_canonical_created_at(prev_max: Option<i64>) -> Timestamp {
    let now = Timestamp::now();
    match prev_max {
        Some(prev) if prev >= 0 => {
            // `prev >= 0` guarantees the cast is value-preserving.
            let floor = (prev as u64).saturating_add(1);
            Timestamp::from_secs(floor.max(now.as_secs()))
        }
        _ => now,
    }
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
        let relays = account.key_package_relays(&self.shared).await?;

        if relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        self.validate_signer_pubkey(&account.pubkey, &signer)
            .await?;
        let signer = std::sync::Arc::new(signer);

        // Serialize concurrent rotations for this account so the
        // prepare/publish/track sequence stays atomic. See
        // `AccountSession::key_package_publish_lock`.
        let session = self.require_session(&account.pubkey)?;
        if session.has_marmot_session() {
            session
                .key_packages()
                .create_and_publish_with_signer(&relays, signer)
                .await?;
            return Ok(());
        }

        Err(WhitenoiseError::MarmotSessionUnavailable(account.pubkey))
    }

    /// Creates and publishes a key package for a freshly-set-up account.
    ///
    /// In production this spawns the publish so account-creation flows
    /// don't block on relays; in tests it runs inline so assertions can
    /// observe the published event deterministically. Either way, failures
    /// only log a warning — the [`KeyPackageMaintenance`] scheduler retries
    /// any publish that didn't land on its next tick (10 min).
    ///
    /// The full `lock → prepare → publish → track` sequence runs inside
    /// the spawn/inline-await block: by the time this function returns,
    /// `setup_subscriptions` has already wired up the giftwrap handler
    /// (see `accounts/setup.rs`), so a welcome can arrive concurrently
    /// with the spawn. Taking the per-account
    /// [`key_package_publish_lock`] *inside* the spawned task is what
    /// prevents the giftwrap-triggered `KeyPackageOps::publish` from
    /// reading the same empty `published_key_packages` state and landing
    /// in a *different* fresh NIP-33 slot — i.e. the slot-drift bug this
    /// PR fixes.
    ///
    /// [`KeyPackageMaintenance`]: crate::whitenoise::scheduled_tasks::tasks::KeyPackageMaintenance
    /// [`key_package_publish_lock`]: crate::whitenoise::session::AccountSession#structfield.key_package_publish_lock
    pub(crate) async fn create_key_package_and_background_publish(
        &self,
        account: &Account,
        relays: &[Relay],
    ) -> Result<()> {
        let relays = relays.to_vec();

        #[cfg(not(test))]
        if let Ok(wn) = self.arc() {
            let account = account.clone();
            tokio::spawn(async move {
                wn.locked_create_and_publish_canonical_pair(&account, &relays)
                    .await;
            });
            return Ok(());
        }

        self.locked_create_and_publish_canonical_pair(account, &relays)
            .await;
        Ok(())
    }

    /// Acquires the per-account publish lock, then runs the full
    /// `prepare → publish → track` sequence and swallows any failure as a
    /// warning so callers always observe success.
    ///
    /// Used by the spawned and inline branches of
    /// [`Self::create_key_package_and_background_publish`]. Holding the
    /// lock across the entire sequence (rather than just publish) is what
    /// prevents the slot-drift race documented on that function.
    async fn locked_create_and_publish_canonical_pair(&self, account: &Account, relays: &[Relay]) {
        let session = match self.require_session(&account.pubkey) {
            Ok(session) => session,
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "Key package publish skipped, session resolution failed: {}",
                    e
                );
                return;
            }
        };

        if session.has_marmot_session() {
            if let Err(e) = session.key_packages().create_and_publish(relays).await {
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "Darkmatter key package publish failed, scheduler will retry: {}",
                    e
                );
            }
            return;
        }

        tracing::warn!(
            target: "whitenoise::key_packages",
            account = %account.pubkey,
            "Key package publish skipped because the account has no Darkmatter session"
        );
    }

    /// Deletes the key package from the relays for the given account.
    ///
    /// Automatically uses the appropriate signer for the account:
    /// - For external accounts (Amber/NIP-55): uses the registered external signer
    /// - For local accounts: uses keys from the secrets store
    ///
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
        let session = self.require_session(&account.pubkey)?;
        let published_package = match session
            .repos
            .published_key_packages
            .find_by_event_id(&event_id.to_hex())
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

        // Delete local MLS key material from the tracked package record.
        // Darkmatter v2 rows delete through Marmot storage; unsupported legacy
        // rows are left unmarked because no local key material was removed.
        if delete_mls_stored_keys {
            match &published_package {
                Some(pkg) if !pkg.key_material_deleted => {
                    let outcome = session
                        .key_packages()
                        .delete_tracked_key_material(pkg)
                        .await?;
                    if outcome != LocalKeyMaterialDeleteOutcome::Deleted {
                        tracing::warn!(
                            target: "whitenoise::key_packages",
                            "Local key material deletion skipped for unsupported legacy key package event {}",
                            event_id
                        );
                    } else if let Err(e) = session
                        .repos
                        .published_key_packages
                        .mark_key_material_deleted_by_hash_ref(&pkg.key_package_hash_ref)
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

        let key_package_relays = account.key_package_relays(&self.shared).await?;
        if key_package_relays.is_empty() {
            return Ok(false);
        }

        let key_package_relays_urls = Relay::urls(&key_package_relays);
        let event_ids = self
            .key_package_event_ids_for_deletion(account, event_id, published_package.as_ref())
            .await;

        let result = self
            .shared
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

        let Some(session) = self.session(&account.pubkey) else {
            tracing::warn!(
                target: "whitenoise::key_packages",
                "No session found for account, cannot find key package hash_ref sibling rows"
            );
            return event_ids;
        };

        match session
            .repos
            .published_key_packages
            .find_by_hash_ref(&package.key_package_hash_ref)
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
                    "Failed to find key package hash_ref sibling rows for event {}: {}",
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
    ///
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
            let remaining_events = self
                .require_session(&account.pubkey)?
                .key_packages()
                .fetch_all()
                .await?;
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
        let key_package_relays = account.key_package_relays(&self.shared).await?;

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
        let session = self.require_session(&account.pubkey)?;
        let mut deleted_hash_refs = HashSet::new();
        let mut deleted_untracked_contents = HashSet::new();
        let mut storage_delete_count = 0;

        for event in key_package_events {
            match session
                .repos
                .published_key_packages
                .find_by_event_id(&event.id.to_hex())
                .await
            {
                Ok(Some(pkg)) => {
                    if !deleted_hash_refs.insert(pkg.key_package_hash_ref.clone()) {
                        continue;
                    }

                    let outcome = match session
                        .key_packages()
                        .delete_tracked_key_material(&pkg)
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(e) => {
                            tracing::warn!(
                                target: "whitenoise::key_packages",
                                "Failed to delete key package from storage for event {}: {}",
                                event.id,
                                e
                            );
                            continue;
                        }
                    };

                    match outcome {
                        LocalKeyMaterialDeleteOutcome::Deleted => {
                            storage_delete_count += 1;
                            if let Err(e) = session
                                .repos
                                .published_key_packages
                                .mark_key_material_deleted_by_hash_ref(&pkg.key_package_hash_ref)
                                .await
                            {
                                tracing::warn!(
                                    target: "whitenoise::key_packages",
                                    "Deleted key material but failed to mark hash_ref group: {}",
                                    e
                                );
                            }
                        }
                        LocalKeyMaterialDeleteOutcome::SkippedUnsupportedLegacy => {}
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

            match session
                .key_packages()
                .delete_event_key_material_if_available(event)
                .await
            {
                Ok(true) => storage_delete_count += 1,
                Ok(false) => {}
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
            .shared
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
}

#[cfg(test)]
mod tests {
    use base64ct::{Base64, Encoding};
    use chrono::Utc;
    use nostr_sdk::{EventBuilder, Keys, Kind, Tag, TagKind};

    use super::*;
    use crate::marmot::session::MarmotSession;
    use crate::marmot::storage::WhitenoiseMarmotStorage;
    use crate::whitenoise::accounts::AccountType;
    use crate::whitenoise::session::key_packages::MAX_DELETE_ROUNDS;
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

    /// Persists the account, stores its keys, and registers an `AccountSession`
    /// so the deprecated key-package wrappers (which require a session) can
    /// find one. Returns the persisted account.
    async fn register_session_with_keys(
        whitenoise: &std::sync::Arc<Whitenoise>,
        account: Account,
        keys: &Keys,
    ) -> Account {
        whitenoise
            .shared
            .secrets_store
            .store_private_key(keys)
            .expect("Should store keys");
        let account = account
            .save(&whitenoise.shared.database)
            .await
            .expect("Should save account");
        let session = std::sync::Arc::new(
            crate::whitenoise::session::AccountSession::from_account(&account, whitenoise)
                .await
                .unwrap(),
        );
        whitenoise.account_manager.insert_session(session);
        account
    }

    /// Persists an `External` account and registers a session with no signer attached.
    /// Used to exercise operations that should fail because the signer is missing.
    async fn register_session_without_signer(whitenoise: &std::sync::Arc<Whitenoise>) -> Account {
        let mut account = create_local_account_struct();
        account.id = None;
        account.account_type = AccountType::External;
        let account = account
            .save(&whitenoise.shared.database)
            .await
            .expect("Should save account");
        let session = std::sync::Arc::new(
            crate::whitenoise::session::AccountSession::from_account(&account, whitenoise)
                .await
                .unwrap(),
        );
        whitenoise.account_manager.insert_session(session);
        account
    }

    /// Creates a persisted account with a key package relay, stored keys, and
    /// a registered session.
    async fn create_account_with_relay(whitenoise: &std::sync::Arc<Whitenoise>) -> Account {
        let (account, keys) = create_test_account(whitenoise).await;
        let user = account.user(&whitenoise.shared.database).await.unwrap();
        // Use a loopback IP so connection refusal is instant (no DNS lookup).
        let relay = crate::whitenoise::relays::Relay::find_or_create_by_url(
            &RelayUrl::parse("ws://127.0.0.1:1").unwrap(),
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        user.add_relay(
            &relay,
            crate::RelayType::KeyPackage,
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        register_session_with_keys(whitenoise, account, &keys).await
    }

    /// Like [`create_account_with_relay`] but also returns the generated keys
    /// for tests that need to sign events as the account.
    async fn create_account_with_relay_and_keys(
        whitenoise: &std::sync::Arc<Whitenoise>,
    ) -> (Account, Keys) {
        let (account, keys) = create_test_account(whitenoise).await;
        let user = account.user(&whitenoise.shared.database).await.unwrap();
        let relay = crate::whitenoise::relays::Relay::find_or_create_by_url(
            &RelayUrl::parse("ws://127.0.0.1:1").unwrap(),
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        user.add_relay(
            &relay,
            crate::RelayType::KeyPackage,
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        let account = register_session_with_keys(whitenoise, account, &keys).await;
        (account, keys)
    }

    /// Creates a persisted external-signer account with a key-package relay and
    /// a registered session. The session intentionally has no Marmot session
    /// because external signers do not expose the account-proof signing hook
    /// Darkmatter currently requires.
    async fn create_external_account_with_relay_and_signer(
        whitenoise: &std::sync::Arc<Whitenoise>,
    ) -> (Account, Keys) {
        let keys = Keys::generate();
        let mut account = Account::new_external(whitenoise, keys.public_key())
            .await
            .unwrap();
        account = account.save(&whitenoise.shared.database).await.unwrap();
        let user = account.user(&whitenoise.shared.database).await.unwrap();
        let relay = crate::whitenoise::relays::Relay::find_or_create_by_url(
            &RelayUrl::parse("ws://127.0.0.1:1").unwrap(),
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        user.add_relay(
            &relay,
            crate::RelayType::KeyPackage,
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        whitenoise
            .insert_external_signer(account.pubkey, keys.clone())
            .await
            .unwrap();

        let session = std::sync::Arc::new(
            crate::whitenoise::session::AccountSession::from_account(&account, whitenoise)
                .await
                .unwrap(),
        );
        whitenoise.account_manager.insert_session(session);

        (account, keys)
    }

    #[tokio::test]
    async fn test_get_signer_for_local_account_with_keys() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an account and store keys in secrets store
        let keys = Keys::generate();
        let pubkey = keys.public_key();
        whitenoise
            .shared
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
            .shared
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
        let user = account.user(&whitenoise.shared.database).await.unwrap();
        let relay = crate::whitenoise::relays::Relay::find_or_create_by_url(
            &RelayUrl::parse("wss://test.relay.com").unwrap(),
            &whitenoise.shared.database,
        )
        .await
        .unwrap();
        user.add_relay(
            &relay,
            crate::RelayType::KeyPackage,
            &whitenoise.shared.database,
        )
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
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = register_session_with_keys(&whitenoise, account, &keys).await;

        // Attempt to publish key package without relays
        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .publish()
            .await;

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
        EventBuilder::new(MLS_KEY_PACKAGE_KIND, "test_content")
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .sign_with_keys(keys)
            .unwrap()
    }

    /// Creates a mock key package event without the encoding tag.
    fn create_key_package_event_without_encoding_tag(keys: &Keys) -> Event {
        EventBuilder::new(MLS_KEY_PACKAGE_KIND, "test_content")
            .sign_with_keys(keys)
            .unwrap()
    }

    fn create_key_package_event_with_compatibility_tags(
        keys: &Keys,
        ciphersuite: &str,
        extensions: &[&str],
    ) -> Event {
        EventBuilder::new(MLS_KEY_PACKAGE_KIND, "dGVzdF9jb250ZW50")
            .tag(Tag::identifier("compatibility-test"))
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
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "test_content")
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["hex"]))
            .sign_with_keys(&keys)
            .unwrap();

        assert!(
            !has_encoding_tag(&event),
            "Should return false when encoding tag has wrong value"
        );
    }

    #[test]
    fn test_validate_marmot_key_package_strict_accepts_required_tags() {
        let keys = Keys::generate();
        let event = create_key_package_event_with_compatibility_tags(
            &keys,
            REQUIRED_MLS_CIPHERSUITE_TAG,
            &["0x000a", "0xF2EE"],
        );

        let result = validate_marmot_key_package_strict(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_ok(), "Expected valid compatibility tags");
    }

    #[test]
    fn test_validate_marmot_key_package_strict_accepts_current_kind() {
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

        let result = validate_marmot_key_package_strict(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(
            result.is_ok(),
            "Expected current kind key package to validate"
        );
    }

    #[test]
    fn test_validate_marmot_key_package_strict_rejects_current_kind_without_d_tag() {
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

        let result = validate_marmot_key_package_strict(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(matches!(
            result,
            Err(WhitenoiseError::MissingKeyPackageDTag)
        ));
    }

    #[test]
    fn test_validate_marmot_key_package_strict_rejects_wrong_ciphersuite() {
        let keys = Keys::generate();
        let event = create_key_package_event_with_compatibility_tags(
            &keys,
            "0x0002",
            &["0x000a", "0xF2EE"],
        );

        let result = validate_marmot_key_package_strict(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_err(), "Expected ciphersuite mismatch to fail");
        assert!(matches!(
            result,
            Err(WhitenoiseError::IncompatibleMlsCiphersuite { .. })
        ));
    }

    #[test]
    fn test_validate_marmot_key_package_strict_rejects_missing_extensions() {
        let keys = Keys::generate();
        let event = create_key_package_event_with_compatibility_tags(
            &keys,
            REQUIRED_MLS_CIPHERSUITE_TAG,
            &["0x000a"],
        );

        let result = validate_marmot_key_package_strict(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_err(), "Expected missing extension to fail");
        assert!(matches!(
            result,
            Err(WhitenoiseError::MissingMlsExtensions { .. })
        ));
    }

    #[test]
    fn test_validate_marmot_key_package_strict_rejects_invalid_base64_content() {
        let keys = Keys::generate();
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "not-base64$$$")
            .tag(Tag::identifier("invalid-base64-test"))
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

        let result = validate_marmot_key_package_strict(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_err(), "Expected invalid base64 content to fail");
        assert!(matches!(result, Err(WhitenoiseError::InvalidBase64(_))));
    }

    #[test]
    fn test_validate_marmot_key_package_strict_rejects_wrong_kind() {
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

        let result = validate_marmot_key_package_strict(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_err(), "Expected wrong kind to fail");
        assert!(matches!(
            result,
            Err(WhitenoiseError::InvalidEventKind { .. })
        ));
    }

    #[test]
    fn test_validate_marmot_key_package_strict_rejects_missing_encoding_tag() {
        let keys = Keys::generate();
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "dGVzdF9jb250ZW50")
            .tag(Tag::identifier("missing-encoding-test"))
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

        let result = validate_marmot_key_package_strict(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_err(), "Expected missing encoding tag to fail");
        assert!(matches!(result, Err(WhitenoiseError::MissingEncodingTag)));
    }

    #[test]
    fn test_validate_marmot_key_package_strict_rejects_missing_self_remove_proposal() {
        // The strict validator still rejects KPs that omit the SelfRemove
        // proposal advertisement. This is intentional: the strict path is the
        // self-publish lifecycle, where we want to fail closed against our own
        // half-built outputs.
        let keys = Keys::generate();
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "dGVzdF9jb250ZW50")
            .tag(Tag::identifier("missing-self-remove-test"))
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

        let result = validate_marmot_key_package_strict(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(result.is_err(), "Expected missing SelfRemove to fail");
        assert!(matches!(
            result,
            Err(WhitenoiseError::MissingMlsProposals { .. })
        ));
    }

    #[test]
    fn test_validate_marmot_key_package_baseline_rejects_missing_marmot_identity_extension() {
        let keys = Keys::generate();
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                [REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a"],
            ))
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .sign_with_keys(&keys)
            .unwrap();

        let result = validate_marmot_key_package_baseline(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(matches!(
            result,
            Err(WhitenoiseError::MissingMlsExtensions { ref missing })
                if missing.iter().any(|m| m == REQUIRED_MARMOT_IDENTITY_EXTENSION_TAG)
        ));
    }

    #[test]
    fn test_validate_marmot_key_package_baseline_accepts_kp_missing_self_remove_proposal() {
        let keys = Keys::generate();
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                [REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0xf2ee"],
            ))
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .sign_with_keys(&keys)
            .unwrap();

        let result = validate_marmot_key_package_baseline(&event, REQUIRED_MLS_CIPHERSUITE_TAG);
        assert!(
            result.is_ok(),
            "Expected legacy-compatible KP (missing SelfRemove tag) to pass baseline; got {result:?}",
        );
    }

    async fn create_darkmatter_v2_key_package_event(
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
        let metadata = cgka_engine::key_package_metadata(&key_package)
            .expect("derive Marmot key package metadata");
        let content = Base64::encode_string(key_package.bytes());
        let key_package_ref = key_package_ref.unwrap_or(metadata.key_package_ref_hex);
        let mut builder = EventBuilder::new(MLS_KEY_PACKAGE_KIND, content)
            .tag(Tag::identifier("darkmatter-slot"))
            .tag(Tag::custom(
                TagKind::Custom("mls_protocol_version".into()),
                ["1.0"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("i".into()),
                [key_package_ref.as_str()],
            ))
            .tag(Tag::custom(
                TagKind::MlsCiphersuite,
                [REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .tag(Tag::custom(
                TagKind::MlsExtensions,
                ["0x0006", "0xf2f1", "0x000a"],
            ))
            .tag(Tag::custom(
                TagKind::Custom(MLS_PROPOSALS_TAG_KEY.into()),
                ["0x0008", "0x000a"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("app_components".into()),
                ["0x8001", "0x8003", "0x8004"],
            ));
        if include_encoding_tag {
            builder = builder.tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]));
        }
        builder
            .sign_with_keys(keys)
            .expect("sign Darkmatter key package event")
    }

    #[tokio::test]
    async fn validate_marmot_key_package_strict_accepts_darkmatter_v2_event() {
        let keys = Keys::generate();
        let event = create_darkmatter_v2_key_package_event(&keys, None, false).await;

        let result = validate_marmot_key_package_strict(&event, REQUIRED_MLS_CIPHERSUITE_TAG);

        assert!(
            result.is_ok(),
            "expected strict validation to accept Darkmatter v2 key package: {result:?}"
        );
    }

    #[tokio::test]
    async fn validate_darkmatter_v2_key_package_rejects_wrong_key_package_ref() {
        let keys = Keys::generate();
        let event =
            create_darkmatter_v2_key_package_event(&keys, Some("00".repeat(32)), false).await;

        let result = validate_marmot_key_package_baseline(&event, REQUIRED_MLS_CIPHERSUITE_TAG);

        assert!(matches!(
            result,
            Err(WhitenoiseError::InvalidKeyPackageRef { .. })
        ));
    }

    #[tokio::test]
    async fn validate_darkmatter_v2_key_package_rejects_encoding_tag() {
        let keys = Keys::generate();
        let event = create_darkmatter_v2_key_package_event(&keys, None, true).await;

        let result = validate_marmot_key_package_baseline(&event, REQUIRED_MLS_CIPHERSUITE_TAG);

        assert!(matches!(
            result,
            Err(WhitenoiseError::UnexpectedEncodingTag)
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
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "test_content")
            .tag(Tag::custom(
                TagKind::Custom("mls_protocol_version".into()),
                ["1.0"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                ["0x0001"],
            ))
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .tag(Tag::custom(
                TagKind::Custom("client".into()),
                ["legacy-marmot/0.5.3"],
            ))
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
        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .publish()
            .await;
        tokio::time::resume();
        assert!(result.is_err(), "Should fail when relay is unreachable");
    }

    #[tokio::test]
    async fn test_create_and_publish_key_package_fails_with_unreachable_relay() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = create_account_with_relay(&whitenoise).await;

        let relays = account
            .key_package_relays(&whitenoise.shared)
            .await
            .unwrap();
        // Pause time so exponential backoff sleeps complete instantly.
        tokio::time::pause();
        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .create_and_publish(&relays)
            .await;
        tokio::time::resume();
        assert!(result.is_err(), "Should fail when relay is unreachable");
    }

    #[test]
    fn test_monotonic_canonical_created_at_uses_now_when_no_prior() {
        let now_secs = Timestamp::now().as_secs();
        let ts = monotonic_canonical_created_at(None);
        assert!(
            ts.as_secs() >= now_secs,
            "with no prior publish the new timestamp must be at least the wall clock"
        );
    }

    #[test]
    fn test_monotonic_canonical_created_at_steps_past_future_prior() {
        // Pin prior strictly in the future so we can prove the bump.
        let now_secs = Timestamp::now().as_secs();
        let prior = i64::try_from(now_secs + 3600).unwrap();
        let ts = monotonic_canonical_created_at(Some(prior));
        assert_eq!(
            ts.as_secs(),
            u64::try_from(prior).unwrap() + 1,
            "must produce prev+1 when the wall clock is below the prior publish"
        );
    }

    #[test]
    fn test_monotonic_canonical_created_at_uses_now_when_prior_is_past() {
        let now_secs = Timestamp::now().as_secs();
        let prior = i64::try_from(now_secs.saturating_sub(3600)).unwrap();
        let ts = monotonic_canonical_created_at(Some(prior));
        assert!(
            ts.as_secs() >= now_secs,
            "wall clock wins when the prior publish is in the past"
        );
    }

    #[test]
    fn test_monotonic_canonical_created_at_ignores_negative_prior() {
        // Defensive: a corrupt DB row shouldn't crash the publish path.
        let now_secs = Timestamp::now().as_secs();
        let ts = monotonic_canonical_created_at(Some(-5));
        assert!(ts.as_secs() >= now_secs);
    }

    #[tokio::test]
    async fn test_publish_key_package_pair_reuses_d_tag_across_publishes() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // First publish lands during account creation.
        let account = whitenoise.create_identity().await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let initial_d_tag = session
            .repos
            .published_key_packages
            .find_latest_by_kind(MLS_KEY_PACKAGE_KIND)
            .await
            .unwrap()
            .and_then(|row| row.d_tag)
            .expect("initial publish must record a d_tag for kind:30443");

        let initial_fetch = session.key_packages().fetch_all().await.unwrap();
        let initial_canonical = initial_fetch
            .iter()
            .find(|e| e.kind == MLS_KEY_PACKAGE_KIND)
            .expect("relay must hold a canonical event after the first publish")
            .clone();

        // Trigger a second publish through the same code path *in the same
        // wall-clock second* — the monotonic-created_at logic must keep the
        // new event id from losing a NIP-01 tiebreaker.
        let keys = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
            .unwrap();
        whitenoise
            .publish_key_package_for_account_with_signer(&account, keys)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        // DB invariant: every canonical row reuses the original slot.
        let canonical_rows: Vec<(String,)> = sqlx::query_as(
            "SELECT d_tag FROM published_key_packages
             WHERE kind = ? AND d_tag IS NOT NULL",
        )
        .bind(i64::from(MLS_KEY_PACKAGE_KIND.as_u16()))
        .fetch_all(&session.account_db.inner.pool)
        .await
        .unwrap();

        assert!(
            canonical_rows.len() >= 2,
            "expected at least two kind:30443 publish rows, got {}",
            canonical_rows.len()
        );
        for (d_tag,) in &canonical_rows {
            assert_eq!(
                d_tag, &initial_d_tag,
                "every kind:30443 publish must reuse the original d_tag slot"
            );
        }

        // Relay invariant: the relay surfaces exactly one canonical event for
        // the (kind, pubkey, d) slot, and it is the NEW one — not the original.
        let after_fetch = session.key_packages().fetch_all().await.unwrap();
        let canonical_events: Vec<_> = after_fetch
            .iter()
            .filter(|e| e.kind == MLS_KEY_PACKAGE_KIND)
            .collect();
        assert_eq!(
            canonical_events.len(),
            1,
            "NIP-33 must collapse kind:30443 events sharing the same d-tag to a single row \
             after replacement; got {} canonical event(s)",
            canonical_events.len()
        );
        assert_ne!(
            canonical_events[0].id, initial_canonical.id,
            "the new canonical event must replace the original on the relay"
        );
        assert!(
            canonical_events[0].created_at >= initial_canonical.created_at,
            "the replacement event must not be older than the one it replaces"
        );
    }

    #[tokio::test]
    async fn test_publish_key_package_with_signer_fails_with_unreachable_relay() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_account_with_relay_and_keys(&whitenoise).await;

        // Pause time so exponential backoff sleeps complete instantly.
        tokio::time::pause();
        let result = whitenoise
            .publish_key_package_for_account_with_signer(&account, keys)
            .await;
        tokio::time::resume();
        assert!(result.is_err(), "Should fail when relay is unreachable");
    }

    #[tokio::test]
    async fn public_publish_key_package_with_signer_uses_darkmatter_without_obsolete_mls_storage() {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_account_with_relay_and_keys(&whitenoise).await;
        let artifacts = remove_obsolete_mls_artifacts(&account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        tokio::time::pause();
        let result = whitenoise
            .publish_key_package_for_account_with_signer(&account, keys)
            .await;
        tokio::time::resume();

        assert!(result.is_err(), "Should fail when relay is unreachable");
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn external_signer_key_package_publish_without_marmot_does_not_create_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_external_account_with_relay_and_signer(&whitenoise).await;
        let artifacts = remove_obsolete_mls_artifacts(&account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let result = whitenoise
            .publish_key_package_for_account_with_signer(&account, keys)
            .await;

        assert!(matches!(
            result,
            Err(WhitenoiseError::MarmotSessionUnavailable(pubkey)) if pubkey == account.pubkey
        ));
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn test_fetch_all_key_packages_without_relays_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = register_session_with_keys(&whitenoise, account, &keys).await;

        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .fetch_all()
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
    async fn test_delete_all_key_packages_without_relays_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an account with keys stored but no key package relays.
        // Keys are needed because the convergence loop resolves the signer
        // before fetching key packages from relays.
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = register_session_with_keys(&whitenoise, account, &keys).await;

        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .delete_all(true)
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
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .delete_batch(vec![], false, 1)
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_delete_key_packages_from_storage_deduplicates_hash_ref_siblings() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_account_with_relay_and_keys(&whitenoise).await;
        let session = whitenoise.session(&account.pubkey).unwrap();
        let key_package_data = {
            let mut marmot = session.marmot.as_ref().unwrap().lock().await;
            marmot
                .fresh_key_package_event(
                    "darkmatter-sibling-delete-slot".to_string(),
                    &[RelayUrl::parse("wss://kp.example").unwrap()],
                )
                .await
                .unwrap()
        };

        let canonical = EventBuilder::new(MLS_KEY_PACKAGE_KIND, &key_package_data.content)
            .tags(key_package_data.tags.clone())
            .sign_with_keys(&keys)
            .unwrap();
        let rotated = EventBuilder::new(MLS_KEY_PACKAGE_KIND, &key_package_data.content)
            .tags(key_package_data.tags.clone())
            .tag(Tag::identifier("rotated-d-tag"))
            .sign_with_keys(&keys)
            .unwrap();

        session
            .repos
            .published_key_packages
            .create_with_protocol_data(
                &key_package_data.key_package_ref,
                &canonical.id.to_hex(),
                MLS_KEY_PACKAGE_KIND,
                Some(&key_package_data.d_tag),
                crate::whitenoise::database::published_key_packages::PublishedKeyPackageProtocolData::darkmatter_v2_last_resort(
                    key_package_data.key_package_ref.clone(),
                    key_package_data.content.clone(),
                    key_package_data.app_components.clone(),
                ),
            )
            .await
            .unwrap();
        session
            .repos
            .published_key_packages
            .create_with_protocol_data(
                &key_package_data.key_package_ref,
                &rotated.id.to_hex(),
                MLS_KEY_PACKAGE_KIND,
                Some("rotated-d-tag"),
                crate::whitenoise::database::published_key_packages::PublishedKeyPackageProtocolData::darkmatter_v2_last_resort(
                    key_package_data.key_package_ref.clone(),
                    key_package_data.content.clone(),
                    key_package_data.app_components.clone(),
                ),
            )
            .await
            .unwrap();

        let invalid_untracked = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "not-a-key-package")
            .sign_with_keys(&keys)
            .unwrap();
        let duplicate_untracked = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "not-a-key-package")
            .sign_with_keys(&keys)
            .unwrap();

        whitenoise
            .delete_key_packages_from_storage(
                &account,
                &[
                    canonical.clone(),
                    rotated.clone(),
                    invalid_untracked,
                    duplicate_untracked,
                ],
                4,
            )
            .await
            .unwrap();

        let canonical_record = session
            .repos
            .published_key_packages
            .find_by_event_id(&canonical.id.to_hex())
            .await
            .unwrap()
            .unwrap();
        let rotated_record = session
            .repos
            .published_key_packages
            .find_by_event_id(&rotated.id.to_hex())
            .await
            .unwrap()
            .unwrap();

        assert!(canonical_record.key_material_deleted);
        assert!(rotated_record.key_material_deleted);
    }

    #[tokio::test]
    async fn delete_key_packages_from_storage_deletes_darkmatter_v2_without_obsolete_mls_storage() {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = register_session_with_keys(&whitenoise, account, &keys).await;
        let artifacts = remove_obsolete_mls_artifacts(&account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let canonical = {
            let mut marmot = session.marmot.as_ref().unwrap().lock().await;
            marmot
                .fresh_key_package_event(
                    "darkmatter-batch-delete-slot".to_string(),
                    &[RelayUrl::parse("wss://kp.example").unwrap()],
                )
                .await
                .unwrap()
        };
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, &canonical.content)
            .tags(canonical.tags.clone())
            .sign_with_keys(&keys)
            .unwrap();
        let key_package =
            crate::marmot::key_packages::key_package_from_base64_content(&canonical.content)
                .unwrap();

        session
            .repos
            .published_key_packages
            .create_with_protocol_data(
                &canonical.key_package_ref,
                &event.id.to_hex(),
                MLS_KEY_PACKAGE_KIND,
                Some(&canonical.d_tag),
                crate::whitenoise::database::published_key_packages::PublishedKeyPackageProtocolData::darkmatter_v2_last_resort(
                    canonical.key_package_ref.clone(),
                    canonical.content.clone(),
                    canonical.app_components.clone(),
                ),
            )
            .await
            .unwrap();

        {
            let marmot = session.marmot.as_ref().unwrap().lock().await;
            assert!(marmot.has_key_package_material(&key_package).unwrap());
        }

        let untracked_non_v2 = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "not-a-key-package")
            .sign_with_keys(&keys)
            .unwrap();

        whitenoise
            .delete_key_packages_from_storage(&account, &[event.clone(), untracked_non_v2], 2)
            .await
            .unwrap();

        {
            let marmot = session.marmot.as_ref().unwrap().lock().await;
            assert!(!marmot.has_key_package_material(&key_package).unwrap());
        }
        let record = session
            .repos
            .published_key_packages
            .find_by_event_id(&event.id.to_hex())
            .await
            .unwrap()
            .unwrap();
        assert!(record.key_material_deleted);
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn public_delete_key_package_deletes_darkmatter_v2_material_without_obsolete_mls_storage()
    {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = register_session_with_keys(&whitenoise, account, &keys).await;
        let artifacts = remove_obsolete_mls_artifacts(&account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let session = whitenoise.require_session(&account.pubkey).unwrap();
        let canonical = {
            let mut marmot = session.marmot.as_ref().unwrap().lock().await;
            marmot
                .fresh_key_package_event(
                    "darkmatter-delete-slot".to_string(),
                    &[RelayUrl::parse("wss://kp.example").unwrap()],
                )
                .await
                .unwrap()
        };
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND, &canonical.content)
            .tags(canonical.tags.clone())
            .sign_with_keys(&keys)
            .unwrap();
        let key_package =
            crate::marmot::key_packages::key_package_from_base64_content(&canonical.content)
                .unwrap();

        session
            .repos
            .published_key_packages
            .create_with_protocol_data(
                &canonical.key_package_ref,
                &event.id.to_hex(),
                MLS_KEY_PACKAGE_KIND,
                Some(&canonical.d_tag),
                crate::whitenoise::database::published_key_packages::PublishedKeyPackageProtocolData::darkmatter_v2_last_resort(
                    canonical.key_package_ref.clone(),
                    canonical.content.clone(),
                    canonical.app_components.clone(),
                ),
            )
            .await
            .unwrap();

        {
            let marmot = session.marmot.as_ref().unwrap().lock().await;
            assert!(marmot.has_key_package_material(&key_package).unwrap());
        }

        let deleted = whitenoise
            .delete_key_package_for_account_with_signer(&account, &event.id, true, keys.clone())
            .await
            .unwrap();

        assert!(
            !deleted,
            "no relay delete is published when the account has no key-package relays"
        );
        {
            let marmot = session.marmot.as_ref().unwrap().lock().await;
            assert!(!marmot.has_key_package_material(&key_package).unwrap());
        }
        let record = session
            .repos
            .published_key_packages
            .find_by_event_id(&event.id.to_hex())
            .await
            .unwrap()
            .unwrap();
        assert!(record.key_material_deleted);
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn test_key_package_event_ids_for_deletion_includes_hash_ref_siblings() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_account_with_relay_and_keys(&whitenoise).await;
        let hash_ref = vec![1, 2, 3];

        let canonical = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "canonical")
            .tag(Tag::identifier("delete-siblings"))
            .sign_with_keys(&keys)
            .unwrap();
        let rotated = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "rotated")
            .tag(Tag::identifier("delete-siblings-rotated"))
            .sign_with_keys(&keys)
            .unwrap();

        let session = whitenoise.session(&account.pubkey).unwrap();
        session
            .repos
            .published_key_packages
            .create(
                &hash_ref,
                &canonical.id.to_hex(),
                MLS_KEY_PACKAGE_KIND,
                Some("delete-siblings"),
            )
            .await
            .unwrap();
        session
            .repos
            .published_key_packages
            .create(
                &hash_ref,
                &rotated.id.to_hex(),
                MLS_KEY_PACKAGE_KIND,
                Some("delete-siblings-rotated"),
            )
            .await
            .unwrap();

        let canonical_record = session
            .repos
            .published_key_packages
            .find_by_event_id(&canonical.id.to_hex())
            .await
            .unwrap()
            .unwrap();
        let event_ids = whitenoise
            .key_package_event_ids_for_deletion(&account, &canonical.id, Some(&canonical_record))
            .await;

        assert_eq!(event_ids.len(), 2);
        assert!(event_ids.contains(&canonical.id));
        assert!(event_ids.contains(&rotated.id));
    }

    #[tokio::test]
    async fn test_delete_single_key_package_without_signer_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // External account with no external signer registered: session exists,
        // but the inner signer slot is None so signer-dependent ops must fail.
        let account = register_session_without_signer(&whitenoise).await;
        let event_id = EventId::all_zeros();

        let result = whitenoise
            .require_session(&account.pubkey)
            .unwrap()
            .key_packages()
            .delete(&event_id, false)
            .await;
        assert!(result.is_err(), "Should fail when no signer is available");
    }

    #[tokio::test]
    async fn test_delete_single_key_package_with_signer_without_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Account exists but has no key package relays. The internal method
        // streams events first (which returns empty without a real relay),
        // so it returns Ok(false) — no event found, nothing to delete.
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = register_session_with_keys(&whitenoise, account, &keys).await;
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
