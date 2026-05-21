//! Key package lifecycle operations scoped to an [`AccountSession`].

use std::collections::HashSet;
use std::time::Duration;

use mdk_core::key_packages::KeyPackageEventData;
use nostr_sdk::prelude::*;

use super::AccountSession;
use crate::RelayType;
use crate::perf_instrument;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::key_packages::{
    MLS_KEY_PACKAGE_KIND, MLS_KEY_PACKAGE_KIND_LEGACY, filter_key_package_events_for_account,
    monotonic_canonical_created_at,
};
use crate::whitenoise::relays::Relay;
use crate::whitenoise::users::User;

#[cfg(feature = "integration-tests")]
use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;

/// Maximum number of relay publish attempts before giving up.
const MAX_PUBLISH_ATTEMPTS: u32 = 3;

/// Maximum number of fetch-delete rounds before giving up.
pub(crate) const MAX_DELETE_ROUNDS: u32 = 10;

/// One-time per-account marker for the release cleanup that purges legacy
/// relay-side key packages and republishes one fresh pair.
pub(crate) const KEY_PACKAGE_RELAY_CLEANUP_TASK: &str = "key_package_relay_cleanup_v1";

/// Quiet period used before the one-time relay cleanup runs after foreground catch-up.
pub(crate) const KEY_PACKAGE_RELAY_CLEANUP_INVITE_GRACE_SECS: u64 = 30;

/// Quiet period after a key package is consumed before relay cleanup may start.
pub(crate) const KEY_PACKAGE_RELAY_CLEANUP_QUIET_PERIOD_SECS: u64 = 30;

const KEY_PACKAGE_RELAY_CLEANUP_VERIFY_DELAY_MS: u64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyPackageRelayCleanupOutcome {
    AlreadyCompleted,
    DeferredRecentConsumedKeyPackage,
    DeletedAndPublished { deleted: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PublishedKeyPackagePair {
    canonical: EventId,
    legacy: Option<EventId>,
}

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

    /// Creates a single MLS key package, then publishes both the canonical
    /// (kind:30443) and legacy (kind:443) events with retry/backoff on the
    /// canonical leg. The key package is created only once to avoid
    /// orphaning unused key material in local MLS storage.
    ///
    /// The canonical publish reuses the previously-recorded NIP-33 `d` tag
    /// slot and uses a strictly-monotonic `created_at`, so it replaces any
    /// prior canonical event on relays instead of accumulating a new
    /// addressable-event row.
    #[perf_instrument("key_packages")]
    pub async fn publish(&self) -> Result<()> {
        let relays = self.prepare_relays().await?;
        if relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        // Serialize concurrent rotations for this account so the
        // prepare/publish/track sequence stays atomic — see the lock's
        // docs on [`AccountSession`]. Held until the function returns.
        let _publish_guard = self.session.key_package_publish_lock.lock().await;

        let (key_package_data, canonical_created_at) =
            self.prepare_canonical_publish_inputs(&relays).await?;
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
                .publish_pair(&key_package_data, canonical_created_at, &relay_urls)
                .await
            {
                Ok(_) => return Ok(()),
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
    ///
    /// Like [`Self::publish`] but without the retry loop. Used by the
    /// account-setup path where failures are caught and surfaced as
    /// warnings, leaving the scheduler to retry.
    #[perf_instrument("key_packages")]
    pub(crate) async fn create_and_publish(&self, relays: &[Relay]) -> Result<()> {
        // Mirror `publish()`: empty relays means we have nowhere to publish
        // to, so don't waste MLS key material generating a package we can't
        // send. Fail with the same error variant so callers handle both
        // paths uniformly.
        if relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        // Serialize concurrent rotations for this account so the
        // prepare/publish/track sequence stays atomic.
        let _publish_guard = self.session.key_package_publish_lock.lock().await;

        let (key_package_data, canonical_created_at) =
            self.prepare_canonical_publish_inputs(relays).await?;
        let relay_urls = Relay::urls(relays);
        self.publish_pair(&key_package_data, canonical_created_at, &relay_urls)
            .await?;
        Ok(())
    }

    /// Resolves persisted canonical-slot state and generates the key package
    /// in one step.
    ///
    /// Looks up the previously-recorded NIP-33 `d` tag and the maximum
    /// `created_at` for kind:30443 publishes, then hands the d-tag to MDK
    /// via `KeyPackageOptions::existing_d_tag` so the new event lands in
    /// the same addressable slot. Returns a strictly-monotonic canonical
    /// `created_at` so a same-second publish can't lose the NIP-01
    /// lowest-event-id tiebreaker.
    ///
    /// Fail-fast on DB read errors: a soft-fail `None` fallback would
    /// publish into a fresh slot (slot drift) and risk losing the timestamp
    /// tiebreaker — defeating the whole reuse path. Per-account SQLite
    /// reads on a local file are rare to fail; the scheduler retries on
    /// the next tick.
    async fn prepare_canonical_publish_inputs(
        &self,
        relays: &[Relay],
    ) -> Result<(KeyPackageEventData, Timestamp)> {
        // Single query: the d-tag and the insert timestamp both come from
        // the most recent canonical row. Folding the two lookups avoids the
        // implication that they could disagree.
        let latest_canonical = self
            .session
            .repos
            .published_key_packages
            .find_latest_by_kind(MLS_KEY_PACKAGE_KIND)
            .await?;
        let stored_d_tag = latest_canonical.as_ref().and_then(|r| r.d_tag.as_deref());
        let prev_canonical_max = latest_canonical.as_ref().map(|r| r.created_at);

        let key_package_data = self.encoded_key_package(relays, stored_d_tag).await?;
        let canonical_created_at = monotonic_canonical_created_at(prev_canonical_max);

        Ok((key_package_data, canonical_created_at))
    }

    /// Publish the canonical (kind:30443) and legacy (kind:443) key package
    /// events for an already-encoded `KeyPackageEventData`.
    ///
    /// Caller is responsible for resolving NIP-33 slot state via
    /// [`Self::prepare_canonical_publish_inputs`] so the d-tag baked into
    /// `key_package_data.tags_30443` by MDK is the persisted slot identifier
    /// (not a fresh random one), and `canonical_created_at` is strictly
    /// monotonic past the prior canonical insert. This function trusts
    /// those inputs and just publishes + tracks.
    ///
    /// Legacy publishing is best-effort: a failure logs a warning but does
    /// not fail the call.
    async fn publish_pair(
        &self,
        key_package_data: &KeyPackageEventData,
        canonical_created_at: Timestamp,
        relay_urls: &[RelayUrl],
    ) -> Result<PublishedKeyPackagePair> {
        let canonical_event_id = self
            .publish_to_relays(
                MLS_KEY_PACKAGE_KIND,
                &key_package_data.content,
                relay_urls,
                &key_package_data.tags_30443,
                Some(canonical_created_at),
            )
            .await?;
        // Propagate canonical tracking failures: the d-tag reuse and
        // monotonic-timestamp invariants both depend on this row being
        // persisted. If the publish landed but tracking silently fails, the
        // next publish would think no slot exists and start a fresh one —
        // the very bug this PR is fixing.
        self.track_published_canonical(
            &key_package_data.hash_ref,
            &canonical_event_id,
            &key_package_data.d_tag,
        )
        .await?;

        let mut legacy_event_id = None;
        match self
            .publish_to_relays(
                MLS_KEY_PACKAGE_KIND_LEGACY,
                &key_package_data.content,
                relay_urls,
                &key_package_data.tags_443,
                None,
            )
            .await
        {
            Ok(published_legacy_event_id) => {
                // Legacy tracking is best-effort: kind:443 is regular per
                // NIP-01 (not addressable), so a missed row only affects
                // audit-trail completeness, not future canonical
                // replacement.
                self.track_published_legacy(&key_package_data.hash_ref, &published_legacy_event_id)
                    .await;
                legacy_event_id = Some(published_legacy_event_id);
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "Published canonical kind:30443 key package for account {} but failed \
                     to publish legacy kind:443 twin: {}",
                    self.session.account_pubkey.to_hex(),
                    e,
                );
            }
        }

        Ok(PublishedKeyPackagePair {
            canonical: canonical_event_id,
            legacy: legacy_event_id,
        })
    }

    #[perf_instrument("key_packages")]
    pub async fn fetch_all(&self) -> Result<Vec<Event>> {
        let relays = self.prepare_relays().await?;
        if relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        let relay_urls = Relay::urls(&relays);
        self.fetch_all_from_relay_urls(&relay_urls).await
    }

    /// One-time release cleanup for accounts that may have accumulated extra
    /// relay-side key package events.
    ///
    /// The cleanup deliberately deletes only relay events, never local MLS key
    /// material. Pending Welcome processing still needs the local material for
    /// key packages that were consumed before this job runs. Local rows and key
    /// material for stale, unconsumed packages may remain orphaned; this bounded
    /// one-time leak is intentional because deleting them here could break late
    /// Welcome processing.
    #[perf_instrument("key_packages")]
    pub(crate) async fn cleanup_relay_key_packages_once(
        &self,
    ) -> Result<KeyPackageRelayCleanupOutcome> {
        if self
            .session
            .repos
            .maintenance_tasks
            .is_completed(KEY_PACKAGE_RELAY_CLEANUP_TASK)
            .await?
        {
            return Ok(KeyPackageRelayCleanupOutcome::AlreadyCompleted);
        }

        if self.has_recent_consumed_key_package().await? {
            return Ok(KeyPackageRelayCleanupOutcome::DeferredRecentConsumedKeyPackage);
        }

        let (publish_relays, cleanup_relay_urls) = self.cleanup_relay_scope().await?;
        if publish_relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        // Serialize against normal key package rotation. Welcomes may trigger
        // a rotation at the same time this maintenance job is preparing to
        // purge and republish.
        let _publish_guard = self.session.key_package_publish_lock.lock().await;

        if self
            .session
            .repos
            .maintenance_tasks
            .is_completed(KEY_PACKAGE_RELAY_CLEANUP_TASK)
            .await?
        {
            return Ok(KeyPackageRelayCleanupOutcome::AlreadyCompleted);
        }

        if self.has_recent_consumed_key_package().await? {
            return Ok(KeyPackageRelayCleanupOutcome::DeferredRecentConsumedKeyPackage);
        }

        let (key_package_data, canonical_created_at) = self
            .prepare_canonical_publish_inputs(&publish_relays)
            .await?;
        let publish_relay_urls = Relay::urls(&publish_relays);
        let published_pair = self
            .publish_pair(&key_package_data, canonical_created_at, &publish_relay_urls)
            .await?;
        let Some(legacy_event_id) = published_pair.legacy else {
            return Err(WhitenoiseError::KeyPackagePublishFailed(
                "one-time key package cleanup could not publish legacy replacement".to_string(),
            ));
        };
        let replacement_event_ids = [published_pair.canonical, legacy_event_id];

        tokio::time::sleep(Duration::from_millis(
            KEY_PACKAGE_RELAY_CLEANUP_VERIFY_DELAY_MS,
        ))
        .await;

        let mut total_deleted = 0;
        for round in 0..MAX_DELETE_ROUNDS {
            let stale_key_package_events = self
                .fetch_all_from_relay_urls(&cleanup_relay_urls)
                .await?
                .into_iter()
                .filter(|event| !replacement_event_ids.contains(&event.id))
                .collect::<Vec<_>>();
            if stale_key_package_events.is_empty() {
                break;
            }

            let deleted = self
                .delete_batch_from_relay_urls(
                    stale_key_package_events,
                    false,
                    1,
                    &cleanup_relay_urls,
                )
                .await?;
            total_deleted += deleted;

            if deleted == 0 {
                let remaining = self
                    .fetch_all_from_relay_urls(&cleanup_relay_urls)
                    .await?
                    .into_iter()
                    .filter(|event| !replacement_event_ids.contains(&event.id))
                    .count();
                return Err(WhitenoiseError::KeyPackageDeleteFailed(format!(
                    "one-time key package cleanup could not delete relay event(s): {} stale \
                     event(s) still present after round {}",
                    remaining,
                    round + 1
                )));
            }
        }

        let remaining = self
            .fetch_all_from_relay_urls(&cleanup_relay_urls)
            .await?
            .into_iter()
            .filter(|event| !replacement_event_ids.contains(&event.id))
            .count();
        if remaining > 0 {
            return Err(WhitenoiseError::KeyPackageDeleteFailed(format!(
                "one-time key package cleanup left {} stale relay event(s) after {} delete round(s)",
                remaining, MAX_DELETE_ROUNDS
            )));
        }

        let final_events = self.fetch_all_from_relay_urls(&cleanup_relay_urls).await?;
        if !published_key_package_pair_is_visible(&final_events, &replacement_event_ids) {
            return Err(WhitenoiseError::KeyPackagePublishFailed(format!(
                "one-time key package cleanup verification did not find replacement events {} \
                 and {}; {} key package event(s) visible",
                replacement_event_ids[0].to_hex(),
                replacement_event_ids[1].to_hex(),
                final_events.len()
            )));
        }

        self.session
            .repos
            .maintenance_tasks
            .mark_completed(KEY_PACKAGE_RELAY_CLEANUP_TASK)
            .await?;

        Ok(KeyPackageRelayCleanupOutcome::DeletedAndPublished {
            deleted: total_deleted,
        })
    }

    async fn fetch_all_from_relay_urls(&self, relay_urls: &[RelayUrl]) -> Result<Vec<Event>> {
        let key_package_events = self
            .session
            .ephemeral
            .fetch_key_packages_from_relays(relay_urls)
            .await?;
        Ok(self.filter_fetched_key_package_events(key_package_events))
    }

    fn filter_fetched_key_package_events(&self, key_package_events: Vec<Event>) -> Vec<Event> {
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

        key_package_events
    }

    /// Returns `true` if the deletion event was accepted by at least one relay.
    #[perf_instrument("key_packages")]
    pub async fn delete(&self, event_id: &EventId, delete_mls_stored_keys: bool) -> Result<bool> {
        // Resolve relays before wiping local MLS key material. If this step
        // fails, the relay-side key package event is still live, so wiping
        // local material would leave peers with a key package pointing at
        // nonexistent local keys. Matches `delete_batch` ordering.
        let relays = self.prepare_relays().await?;
        if relays.is_empty() {
            return Ok(false);
        }

        if delete_mls_stored_keys {
            self.delete_local_key_material(event_id).await;
        }

        let relay_urls = Relay::urls(&relays);
        let result = self
            .session
            .ephemeral
            .publish_event_deletion(&[*event_id], &relay_urls)
            .await?;
        Ok(!result.success.is_empty())
    }

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
                    "All key packages deleted for account {} ({} total across {} round(s))",
                    self.session.account_pubkey.to_hex(),
                    total_deleted,
                    round + 1,
                );
                return Ok(total_deleted);
            }

            let batch_size = key_package_events.len();
            let deleted = self
                .delete_batch(key_package_events, delete_mls_stored_keys, 1)
                .await?;

            total_deleted += deleted;

            if deleted == 0 {
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "Round {} deleted 0 key packages despite {} found — relays may not support deletion",
                    round + 1,
                    batch_size,
                );
                break;
            }
        }

        Ok(total_deleted)
    }

    /// Fetches and deletes legacy (kind 443) key packages in rounds until none
    /// remain on relays, up to 10 rounds.
    ///
    /// This is the session-scoped equivalent of both
    /// `Whitenoise::delete_legacy_key_packages_for_account` and
    /// `Whitenoise::delete_legacy_key_packages_for_account_with_signer` — the
    /// session already carries the correct signer, so no explicit signer
    /// argument is needed.
    #[perf_instrument("key_packages")]
    pub async fn delete_legacy(&self) -> Result<usize> {
        let mut total_deleted = 0;

        for round in 0..MAX_DELETE_ROUNDS {
            let all_events = self.fetch_all().await?;
            let legacy_events: Vec<Event> = all_events
                .into_iter()
                .filter(|e| e.kind == MLS_KEY_PACKAGE_KIND_LEGACY)
                .collect();

            if legacy_events.is_empty() {
                tracing::info!(
                    target: "whitenoise::key_packages",
                    "All legacy key packages deleted for account {} \
                     ({} total across {} round(s))",
                    self.session.account_pubkey.to_hex(),
                    total_deleted,
                    round + 1,
                );
                return Ok(total_deleted);
            }

            tracing::debug!(
                target: "whitenoise::key_packages",
                "Round {}: found {} remaining legacy key package(s) for account {}",
                round + 1,
                legacy_events.len(),
                self.session.account_pubkey.to_hex(),
            );

            let batch_size = legacy_events.len();
            let deleted = self.delete_batch(legacy_events, false, 1).await?;

            total_deleted += deleted;

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

    /// Delete specific key package events from relays with retry.
    /// Storage deletion happens only on the initial attempt.
    #[perf_instrument("key_packages")]
    pub(crate) async fn delete_batch(
        &self,
        key_package_events: Vec<Event>,
        delete_mls_stored_keys: bool,
        max_retries: u32,
    ) -> Result<usize> {
        if key_package_events.is_empty() {
            return Ok(0);
        }

        let relays = self.prepare_relays().await?;
        if relays.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }
        let relay_urls = Relay::urls(&relays);

        self.delete_batch_from_relay_urls(
            key_package_events,
            delete_mls_stored_keys,
            max_retries,
            &relay_urls,
        )
        .await
    }

    async fn delete_batch_from_relay_urls(
        &self,
        key_package_events: Vec<Event>,
        delete_mls_stored_keys: bool,
        max_retries: u32,
        relay_urls: &[RelayUrl],
    ) -> Result<usize> {
        if key_package_events.is_empty() {
            return Ok(0);
        }
        if relay_urls.is_empty() {
            return Err(WhitenoiseError::AccountMissingKeyPackageRelays);
        }

        let original_ids: HashSet<EventId> = key_package_events.iter().map(|e| e.id).collect();

        if delete_mls_stored_keys {
            self.delete_from_storage(&key_package_events)?;
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

            self.publish_deletion(&pending_ids, relay_urls).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;

            let remaining_events = self.fetch_all_from_relay_urls(relay_urls).await?;
            pending_ids = remaining_events
                .iter()
                .filter(|e| original_ids.contains(&e.id))
                .map(|e| e.id)
                .collect();

            if pending_ids.is_empty() {
                break;
            }
        }

        let deleted_count = original_ids.len() - pending_ids.len();

        if !pending_ids.is_empty() {
            tracing::warn!(
            target: "whitenoise::key_packages",
            "After {} retries, {} of {} key package(s) still not deleted for account {}",
                max_retries,
                pending_ids.len(),
                original_ids.len(),
                self.session.account_pubkey.to_hex()
            );
        }

        Ok(deleted_count)
    }

    // ── Private helpers ────────────────────────────────────────────────

    /// Builds a fresh MLS key package and serializes it for relay publish.
    ///
    /// When `existing_d_tag` is `Some(d)`, MDK reuses that NIP-33
    /// addressable-slot identifier (via `KeyPackageOptions::existing_d_tag`)
    /// so the resulting event replaces the prior canonical event on relays
    /// instead of accumulating a new addressable-event row. `None` lets MDK
    /// generate a fresh slot (first publish for the account).
    #[perf_instrument("key_packages")]
    async fn encoded_key_package(
        &self,
        relays: &[Relay],
        existing_d_tag: Option<&str>,
    ) -> Result<KeyPackageEventData> {
        let key_package_relay_urls = Relay::urls(relays);
        let options = mdk_core::key_packages::KeyPackageOptions {
            protected: false,
            existing_d_tag: existing_d_tag.map(str::to_owned),
        };
        let data = self
            .session
            .mdk
            .create_key_package_for_event_with_options(
                &self.session.account_pubkey,
                key_package_relay_urls,
                options,
            )
            .map_err(|e| WhitenoiseError::Configuration(format!("NostrMls error: {}", e)))?;
        Ok(data)
    }

    #[perf_instrument("key_packages")]
    async fn publish_to_relays(
        &self,
        kind: Kind,
        encoded_key_package: &str,
        relay_urls: &[RelayUrl],
        tags: &[Tag],
        custom_created_at: Option<Timestamp>,
    ) -> Result<EventId> {
        let result = self
            .session
            .ephemeral
            .publish_key_package(
                kind,
                encoded_key_package,
                relay_urls,
                tags,
                custom_created_at,
            )
            .await?;

        if result.success.is_empty() {
            return Err(WhitenoiseError::KeyPackagePublishFailed(
                "no relay accepted the key package event".to_string(),
            ));
        }

        Ok(*result.id())
    }

    /// Tracks a canonical (kind:30443) publish in the per-account DB.
    ///
    /// Errors propagate to the caller because the d-tag reuse and
    /// monotonic-timestamp logic both read from this row on the next
    /// publish; silent loss would re-introduce slot drift.
    #[perf_instrument("key_packages")]
    async fn track_published_canonical(
        &self,
        hash_ref: &[u8],
        event_id: &EventId,
        d_tag: &str,
    ) -> Result<()> {
        self.session
            .repos
            .published_key_packages
            .create(
                hash_ref,
                &event_id.to_hex(),
                MLS_KEY_PACKAGE_KIND,
                Some(d_tag),
            )
            .await
    }

    /// Tracks a legacy (kind:443) publish — best-effort.
    ///
    /// kind:443 is regular per NIP-01 (not addressable), so missing rows
    /// only affect the audit trail. We log and continue.
    #[perf_instrument("key_packages")]
    async fn track_published_legacy(&self, hash_ref: &[u8], event_id: &EventId) {
        if let Err(e) = self
            .session
            .repos
            .published_key_packages
            .create(
                hash_ref,
                &event_id.to_hex(),
                MLS_KEY_PACKAGE_KIND_LEGACY,
                None,
            )
            .await
        {
            tracing::warn!(
                target: "whitenoise::key_packages",
                "Published legacy key package but failed to track it: {}",
                e
            );
        }
    }

    async fn prepare_relays(&self) -> Result<Vec<Relay>> {
        let user =
            User::find_by_pubkey(&self.session.account_pubkey, &self.session.shared.database)
                .await
                .map_err(|_| WhitenoiseError::AccountNotFound)?;
        user.relays(RelayType::KeyPackage, &self.session.shared.database)
            .await
    }

    async fn cleanup_relay_scope(&self) -> Result<(Vec<Relay>, Vec<RelayUrl>)> {
        let user =
            User::find_by_pubkey(&self.session.account_pubkey, &self.session.shared.database)
                .await
                .map_err(|_| WhitenoiseError::AccountNotFound)?;
        let key_package_relays = user
            .relays(RelayType::KeyPackage, &self.session.shared.database)
            .await?;
        let nip65_relays = user
            .relays(RelayType::Nip65, &self.session.shared.database)
            .await?;

        let cleanup_relay_urls = merge_cleanup_relay_urls(
            key_package_relays.iter().map(|relay| relay.url.clone()),
            nip65_relays.into_iter().map(|relay| relay.url),
            self.session
                .shared
                .config
                .default_account_relays
                .iter()
                .cloned(),
        );

        Ok((key_package_relays, cleanup_relay_urls))
    }

    async fn has_recent_consumed_key_package(&self) -> Result<bool> {
        self.session
            .repos
            .published_key_packages
            .has_consumed_since(KEY_PACKAGE_RELAY_CLEANUP_QUIET_PERIOD_SECS as i64)
            .await
    }

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
            Ok(Some(_)) => {}
            Ok(None) => {
                tracing::warn!(
                    target: "whitenoise::key_packages",
                    "No published key package record for event {}, cannot delete local key material",
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

    fn delete_from_storage(&self, key_package_events: &[Event]) -> Result<()> {
        for event in key_package_events {
            match self.session.mdk.parse_key_package(event) {
                Ok(key_package) => {
                    if let Err(e) = self
                        .session
                        .mdk
                        .delete_key_package_from_storage(&key_package)
                    {
                        tracing::warn!(
                            target: "whitenoise::key_packages",
                            "Failed to delete key package from storage for event {}: {}",
                            event.id,
                            e
                        );
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
        Ok(())
    }

    #[perf_instrument("key_packages")]
    async fn publish_deletion(&self, event_ids: &[EventId], relay_urls: &[RelayUrl]) -> Result<()> {
        let result = self
            .session
            .ephemeral
            .publish_event_deletion(event_ids, relay_urls)
            .await?;
        if result.success.is_empty() {
            tracing::error!(
                target: "whitenoise::key_packages",
                "Batch deletion event was not accepted by any relay"
            );
        }
        Ok(())
    }

    // ── Testing helpers ────────────────────────────────────────────────

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

    #[cfg(feature = "integration-tests")]
    pub async fn track_published_for_testing(&self, hash_ref: &[u8], event_id: &str) -> Result<()> {
        self.session
            .repos
            .published_key_packages
            .create(hash_ref, event_id, MLS_KEY_PACKAGE_KIND_LEGACY, None)
            .await
    }

    #[cfg(feature = "integration-tests")]
    pub async fn backdate_consumed_at_for_testing(
        &self,
        event_id: &str,
        age_secs: i64,
    ) -> Result<()> {
        self.session
            .repos
            .published_key_packages
            .backdate_consumed_at(event_id, age_secs)
            .await
    }
}

fn merge_cleanup_relay_urls<KeyPackageRelays, Nip65Relays, DefaultRelays>(
    key_package_relays: KeyPackageRelays,
    nip65_relays: Nip65Relays,
    default_relays: DefaultRelays,
) -> Vec<RelayUrl>
where
    KeyPackageRelays: IntoIterator<Item = RelayUrl>,
    Nip65Relays: IntoIterator<Item = RelayUrl>,
    DefaultRelays: IntoIterator<Item = RelayUrl>,
{
    let mut seen = HashSet::new();
    key_package_relays
        .into_iter()
        .chain(nip65_relays)
        .chain(default_relays)
        .filter(|relay_url| seen.insert(relay_url.as_str().to_owned()))
        .collect()
}

fn published_key_package_pair_is_visible(events: &[Event], event_ids: &[EventId; 2]) -> bool {
    event_ids
        .iter()
        .all(|event_id| events.iter().any(|event| event.id == *event_id))
}

#[cfg(test)]
mod tests {
    use nostr_sdk::{EventBuilder, EventId, Keys, Kind, RelayUrl, Tag};

    use super::{
        KEY_PACKAGE_RELAY_CLEANUP_INVITE_GRACE_SECS, KEY_PACKAGE_RELAY_CLEANUP_QUIET_PERIOD_SECS,
        KEY_PACKAGE_RELAY_CLEANUP_TASK, KEY_PACKAGE_RELAY_CLEANUP_VERIFY_DELAY_MS,
        KeyPackageRelayCleanupOutcome, MLS_KEY_PACKAGE_KIND, MLS_KEY_PACKAGE_KIND_LEGACY,
        merge_cleanup_relay_urls, published_key_package_pair_is_visible,
    };
    use crate::RelayType;
    use crate::whitenoise::error::WhitenoiseError;
    use crate::whitenoise::relays::Relay;
    use crate::whitenoise::session::test_helpers::test_session;
    use crate::whitenoise::users::User;

    #[tokio::test]
    async fn delete_keeps_local_material_when_no_relays() {
        // Regression: when `prepare_relays` yields no relays, `delete()` must
        // short-circuit BEFORE wiping local MLS key material. Otherwise the
        // relay-side KP event stays live while local material is gone, leaving
        // peers with a broken key package.
        let pk = Keys::generate().public_key();
        let session = test_session(pk).await;

        // Create real MLS key material so that an incorrectly-ordered wipe
        // would actually succeed. Without this, the MDK delete would no-op on
        // a missing hash_ref and the test couldn't distinguish old from new.
        let dummy_url = RelayUrl::parse("wss://test.invalid").unwrap();
        let kp_data = session
            .mdk
            .create_key_package_for_event(&pk, vec![dummy_url])
            .expect("create key package");

        let event_id = EventId::all_zeros();
        session
            .repos
            .published_key_packages
            .create(
                &kp_data.hash_ref,
                &event_id.to_hex(),
                MLS_KEY_PACKAGE_KIND_LEGACY,
                None,
            )
            .await
            .expect("seed published_key_packages row");

        let result = session.key_packages().delete(&event_id, true).await;
        assert!(matches!(result, Ok(false)));

        let pkg = session
            .repos
            .published_key_packages
            .find_by_event_id(&event_id.to_hex())
            .await
            .expect("lookup")
            .expect("record exists");
        assert!(
            !pkg.key_material_deleted,
            "delete() wiped local key material despite relay check yielding no relays"
        );
    }

    #[tokio::test]
    async fn publish_fails_with_no_relays() {
        let pk = Keys::generate().public_key();
        let session = test_session(pk).await;

        let result = session.key_packages().publish().await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("key package relay") || err_msg.contains("AccountNotFound"),
            "Expected relay or account error, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn fetch_all_fails_with_no_relays() {
        let pk = Keys::generate().public_key();
        let session = test_session(pk).await;

        let result = session.key_packages().fetch_all().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn delete_batch_empty_returns_zero() {
        let pk = Keys::generate().public_key();
        let session = test_session(pk).await;

        let result = session.key_packages().delete_batch(vec![], false, 0).await;
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn delete_batch_from_relay_urls_empty_returns_zero_before_relay_check() {
        let pk = Keys::generate().public_key();
        let session = test_session(pk).await;

        let result = session
            .key_packages()
            .delete_batch_from_relay_urls(vec![], true, 0, &[])
            .await;

        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn delete_batch_from_relay_urls_requires_relays_for_nonempty_batch() {
        let keys = Keys::generate();
        let session = test_session(keys.public_key()).await;
        let event = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "key-package")
            .sign_with_keys(&keys)
            .unwrap();

        let result = session
            .key_packages()
            .delete_batch_from_relay_urls(vec![event], false, 0, &[])
            .await;

        assert!(matches!(
            result,
            Err(WhitenoiseError::AccountMissingKeyPackageRelays)
        ));
    }

    #[tokio::test]
    async fn filter_fetched_key_package_events_drops_wrong_kind_and_author() {
        let account_keys = Keys::generate();
        let session = test_session(account_keys.public_key()).await;
        let matching = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "legacy")
            .sign_with_keys(&account_keys)
            .unwrap();
        let wrong_kind = EventBuilder::new(Kind::TextNote, "not a key package")
            .sign_with_keys(&account_keys)
            .unwrap();
        let wrong_author = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "wrong author")
            .sign_with_keys(&Keys::generate())
            .unwrap();

        let filtered = session
            .key_packages()
            .filter_fetched_key_package_events(vec![matching.clone(), wrong_kind, wrong_author]);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, matching.id);
    }

    #[tokio::test]
    async fn delete_local_key_material_marks_hash_ref_group_deleted() {
        let account_keys = Keys::generate();
        let pk = account_keys.public_key();
        let session = test_session(pk).await;
        let dummy_url = RelayUrl::parse("wss://test.invalid").unwrap();
        let kp_data = session
            .mdk
            .create_key_package_for_event(&pk, vec![dummy_url])
            .expect("create key package");
        let canonical = EventBuilder::new(MLS_KEY_PACKAGE_KIND, &kp_data.content)
            .tags(kp_data.tags_30443.to_vec())
            .sign_with_keys(&account_keys)
            .unwrap();
        let legacy = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, &kp_data.content)
            .tags(kp_data.tags_443.to_vec())
            .sign_with_keys(&account_keys)
            .unwrap();

        session
            .repos
            .published_key_packages
            .create(
                &kp_data.hash_ref,
                &canonical.id.to_hex(),
                MLS_KEY_PACKAGE_KIND,
                Some(&kp_data.d_tag),
            )
            .await
            .unwrap();
        session
            .repos
            .published_key_packages
            .create(
                &kp_data.hash_ref,
                &legacy.id.to_hex(),
                MLS_KEY_PACKAGE_KIND_LEGACY,
                None,
            )
            .await
            .unwrap();

        session
            .key_packages()
            .delete_local_key_material(&canonical.id)
            .await;

        let records = session
            .repos
            .published_key_packages
            .find_by_hash_ref(&kp_data.hash_ref)
            .await
            .unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| record.key_material_deleted));
    }

    #[tokio::test]
    async fn delete_local_key_material_handles_missing_and_already_deleted_records() {
        let keys = Keys::generate();
        let session = test_session(keys.public_key()).await;
        let missing = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "missing")
            .sign_with_keys(&keys)
            .unwrap();

        session
            .key_packages()
            .delete_local_key_material(&missing.id)
            .await;

        session
            .repos
            .published_key_packages
            .create(
                b"already-deleted",
                &missing.id.to_hex(),
                MLS_KEY_PACKAGE_KIND_LEGACY,
                None,
            )
            .await
            .unwrap();
        let record = session
            .repos
            .published_key_packages
            .find_by_event_id(&missing.id.to_hex())
            .await
            .unwrap()
            .unwrap();
        session
            .repos
            .published_key_packages
            .mark_key_material_deleted(record.id)
            .await
            .unwrap();

        session
            .key_packages()
            .delete_local_key_material(&missing.id)
            .await;

        let record = session
            .repos
            .published_key_packages
            .find_by_event_id(&missing.id.to_hex())
            .await
            .unwrap()
            .unwrap();
        assert!(record.key_material_deleted);
    }

    #[tokio::test]
    async fn delete_from_storage_deletes_parseable_events_and_ignores_invalid_events() {
        let account_keys = Keys::generate();
        let pk = account_keys.public_key();
        let session = test_session(pk).await;
        let dummy_url = RelayUrl::parse("wss://test.invalid").unwrap();
        let kp_data = session
            .mdk
            .create_key_package_for_event(&pk, vec![dummy_url])
            .expect("create key package");
        let valid = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, &kp_data.content)
            .tags(kp_data.tags_443.to_vec())
            .sign_with_keys(&account_keys)
            .unwrap();
        let invalid = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "not-a-key-package")
            .sign_with_keys(&account_keys)
            .unwrap();

        session
            .key_packages()
            .delete_from_storage(&[valid, invalid])
            .unwrap();
    }

    #[tokio::test]
    async fn cleanup_once_returns_already_completed_before_relay_resolution() {
        let pk = Keys::generate().public_key();
        let session = test_session(pk).await;
        session
            .repos
            .maintenance_tasks
            .mark_completed(KEY_PACKAGE_RELAY_CLEANUP_TASK)
            .await
            .unwrap();

        let outcome = session
            .key_packages()
            .cleanup_relay_key_packages_once()
            .await
            .unwrap();

        assert_eq!(outcome, KeyPackageRelayCleanupOutcome::AlreadyCompleted);
    }

    #[tokio::test]
    async fn cleanup_once_defers_recent_consumption_before_purge() {
        let pk = Keys::generate().public_key();
        let session = test_session(pk).await;
        let event_id = EventId::all_zeros().to_hex();
        session
            .repos
            .published_key_packages
            .create(
                b"recent-consumed",
                &event_id,
                MLS_KEY_PACKAGE_KIND_LEGACY,
                None,
            )
            .await
            .unwrap();
        session
            .repos
            .published_key_packages
            .mark_consumed(&event_id)
            .await
            .unwrap();

        let outcome = session
            .key_packages()
            .cleanup_relay_key_packages_once()
            .await
            .unwrap();

        assert_eq!(
            outcome,
            KeyPackageRelayCleanupOutcome::DeferredRecentConsumedKeyPackage
        );
    }

    #[tokio::test]
    async fn cleanup_once_requires_publish_relays_when_not_deferred() {
        let pk = Keys::generate().public_key();
        let session = test_session(pk).await;

        let result = session
            .key_packages()
            .cleanup_relay_key_packages_once()
            .await;

        assert!(matches!(
            result,
            Err(WhitenoiseError::AccountMissingKeyPackageRelays)
        ));
    }

    #[tokio::test]
    async fn cleanup_relay_scope_reads_key_package_nip65_and_default_sources() {
        let pk = Keys::generate().public_key();
        let session = test_session(pk).await;
        let user = User::find_by_pubkey(&pk, &session.shared.database)
            .await
            .unwrap();
        let key_package_url = RelayUrl::parse("wss://cleanup-kp.example").unwrap();
        let nip65_url = RelayUrl::parse("wss://cleanup-nip65.example").unwrap();
        let key_package_relay =
            Relay::find_or_create_by_url(&key_package_url, &session.shared.database)
                .await
                .unwrap();
        let nip65_relay = Relay::find_or_create_by_url(&nip65_url, &session.shared.database)
            .await
            .unwrap();
        user.add_relays(
            &[key_package_relay],
            RelayType::KeyPackage,
            &session.shared.database,
        )
        .await
        .unwrap();
        user.add_relays(&[nip65_relay], RelayType::Nip65, &session.shared.database)
            .await
            .unwrap();

        let (publish_relays, cleanup_urls) =
            session.key_packages().cleanup_relay_scope().await.unwrap();

        assert_eq!(Relay::urls(&publish_relays), vec![key_package_url.clone()]);
        assert!(cleanup_urls.contains(&key_package_url));
        assert!(cleanup_urls.contains(&nip65_url));
        for default_url in &session.shared.config.default_account_relays {
            assert!(cleanup_urls.contains(default_url));
        }
    }

    #[test]
    fn relay_cleanup_policy_constants_are_stable() {
        assert_eq!(
            KEY_PACKAGE_RELAY_CLEANUP_TASK,
            "key_package_relay_cleanup_v1"
        );
        assert_eq!(KEY_PACKAGE_RELAY_CLEANUP_INVITE_GRACE_SECS, 30);
        assert_eq!(KEY_PACKAGE_RELAY_CLEANUP_QUIET_PERIOD_SECS, 30);
        assert_eq!(KEY_PACKAGE_RELAY_CLEANUP_VERIFY_DELAY_MS, 500);
        let outcomes = [
            KeyPackageRelayCleanupOutcome::AlreadyCompleted,
            KeyPackageRelayCleanupOutcome::DeferredRecentConsumedKeyPackage,
            KeyPackageRelayCleanupOutcome::DeletedAndPublished { deleted: 2 },
        ];
        assert_eq!(outcomes.len(), 3);
    }

    #[test]
    fn cleanup_relay_scope_merges_key_package_nip65_and_default_relays() {
        let kp = RelayUrl::parse("wss://kp.example").unwrap();
        let nip65 = RelayUrl::parse("wss://nip65.example").unwrap();
        let default = RelayUrl::parse("wss://default.example").unwrap();

        let merged = merge_cleanup_relay_urls(
            [kp.clone()],
            [nip65.clone(), kp.clone()],
            [default.clone(), nip65.clone()],
        );

        assert_eq!(
            merged,
            vec![kp, nip65, default],
            "cleanup scope should include all three sources, deduplicate, and preserve source order"
        );
    }

    #[test]
    fn published_pair_visibility_requires_both_replacement_event_ids() {
        let keys = Keys::generate();
        let canonical = EventBuilder::new(MLS_KEY_PACKAGE_KIND, "canonical")
            .tags([Tag::parse(["d", "slot"]).unwrap()])
            .sign_with_keys(&keys)
            .unwrap();
        let legacy = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "legacy")
            .sign_with_keys(&keys)
            .unwrap();
        let extra_legacy = EventBuilder::new(MLS_KEY_PACKAGE_KIND_LEGACY, "extra")
            .sign_with_keys(&keys)
            .unwrap();
        let event_ids = [canonical.id, legacy.id];

        assert!(published_key_package_pair_is_visible(
            &[canonical.clone(), legacy.clone(), extra_legacy],
            &event_ids
        ));
        assert!(!published_key_package_pair_is_visible(
            std::slice::from_ref(&canonical),
            &event_ids
        ));
    }
}
