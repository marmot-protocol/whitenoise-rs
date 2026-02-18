use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    nostr_manager::NostrManager,
    whitenoise::{
        Whitenoise,
        database::processed_events::ProcessedEvent,
        error::{Result, WhitenoiseError},
        relays::{Relay, RelayType},
        utils::timestamp_to_datetime,
    },
};

/// TTL for user metadata before it's considered stale and needs refreshing
/// Set to 24 hours - metadata doesn't change frequently for most users
const METADATA_TTL_HOURS: i64 = 24;

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

/// Specifies how user metadata and relay lists should be synchronized when finding or creating a user.
///
/// This enum controls the synchronization behavior in `find_or_create_user_by_pubkey`, allowing
/// callers to choose between immediate blocking synchronization or background asynchronous updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UserSyncMode {
    /// Immediate blocking sync of metadata and relay lists.
    ///
    /// This mode performs synchronous network calls to fetch the latest user metadata
    /// and relay lists before returning. Use this when you need up-to-date information
    /// immediately (e.g., when displaying user profiles or adding users to groups).
    ///
    /// **Note:** This mode blocks the current async task until network operations complete.
    Blocking,

    /// Background sync with TTL-based refresh.
    ///
    /// This mode returns immediately with cached data (if available) and schedules
    /// background tasks to update stale information. New users or users with stale
    /// metadata (older than 24 hours) will have their data refreshed asynchronously.
    ///
    /// **Note:** This is the recommended mode for most use cases as it provides better
    /// performance and responsiveness.
    Background,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct User {
    pub id: Option<i64>,
    pub pubkey: PublicKey,
    pub metadata: Metadata,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl User {
    /// Checks if the user's metadata is stale and needs refreshing based on TTL.
    ///
    /// Returns `true` if the metadata was last updated more than `METADATA_TTL_HOURS` ago,
    /// or if this is a newly created user with default metadata.
    ///
    /// # Returns
    ///
    /// * `true` if metadata should be refreshed
    /// * `false` if metadata is still fresh
    fn needs_metadata_refresh(&self) -> bool {
        let now = Utc::now();
        let ttl_duration = Duration::hours(METADATA_TTL_HOURS);
        let stale_threshold = now - ttl_duration;

        // Always refresh if metadata is default (empty)
        if self.metadata == Metadata::new() {
            return true;
        }

        // Refresh if updated_at is older than TTL
        self.updated_at < stale_threshold
    }

    /// Syncs the user's metadata by fetching the latest version from Nostr relays.
    ///
    /// This method queries the user's configured relays (or default relays if none are configured)
    /// to fetch the most recent metadata event (kind 0) published by the user. If newer metadata
    /// is found that differs from the locally cached version, it updates the local record and
    /// saves the changes to the database.
    ///
    /// The method implements smart fetching by using the user's NIP-65 relay list when available,
    /// or falling back to default relays if the user hasn't published a relay list yet.
    ///
    /// # Arguments
    ///
    /// * `whitenoise` - The Whitenoise instance used to access the Nostr client and database
    pub async fn sync_metadata(&mut self, whitenoise: &Whitenoise) -> Result<()> {
        let relays_urls: Vec<_> = Relay::urls(&self.get_query_relays(whitenoise).await?);
        let metadata_event = whitenoise
            .nostr
            .fetch_metadata_from(&relays_urls, self.pubkey)
            .await?;
        if let Some(event) = metadata_event {
            let metadata = Metadata::from_json(&event.content)?;
            let should_update = self
                .should_update_metadata(&event, false, &whitenoise.database)
                .await?;

            if should_update {
                self.metadata = metadata;

                // Save the updated user metadata
                self.save(&whitenoise.database).await?;

                whitenoise
                    .nostr
                    .event_tracker
                    .track_processed_global_event(&event)
                    .await?;

                tracing::debug!(
                    target: "whitenoise::users::sync_metadata",
                    "Updated metadata for user {} with event timestamp {} via background sync",
                    self.pubkey,
                    event.created_at
                );
            }
        }
        Ok(())
    }

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
            .nostr
            .fetch_user_key_package(self.pubkey, &key_package_relays_urls)
            .await?;
        Ok(key_package_event)
    }

    /// Checks the status of a user's key package on relays.
    ///
    /// Similar to [`key_package_event`](Self::key_package_event), but returns a
    /// [`KeyPackageStatus`] that distinguishes between valid, missing, and incompatible.
    pub async fn key_package_status(&self, whitenoise: &Whitenoise) -> Result<KeyPackageStatus> {
        let event = self.key_package_event(whitenoise).await?;
        Ok(classify_key_package(event))
    }

    pub async fn relays_by_type(
        &self,
        relay_type: RelayType,
        whitenoise: &Whitenoise,
    ) -> Result<Vec<Relay>> {
        self.relays(relay_type, &whitenoise.database).await
    }

    /// Fetches the latest relay lists for this user from Nostr and updates the local database
    pub(crate) async fn update_relay_lists(&self, whitenoise: &Whitenoise) -> Result<()> {
        let initial_query_relays = self.get_query_relays(whitenoise).await?;

        tracing::info!(
            target: "whitenoise::users::update_relay_lists",
            "Updating relay lists for user {} using {} query relays",
            self.pubkey,
            initial_query_relays.len()
        );

        let updated_query_relays = self
            .update_nip65_relays(whitenoise, &initial_query_relays)
            .await?;

        self.update_secondary_relay_types(whitenoise, &updated_query_relays)
            .await?;

        tracing::info!(
            target: "whitenoise::users::update_relay_lists",
            "Successfully completed relay list updates for user {}",
            self.pubkey
        );

        Ok(())
    }

    async fn get_query_relays(&self, whitenoise: &Whitenoise) -> Result<Vec<Relay>> {
        let stored_relays = self.relays(RelayType::Nip65, &whitenoise.database).await?;

        if stored_relays.is_empty() {
            tracing::debug!(
                target: "whitenoise::users::get_query_relays",
                "User {} has no stored NIP-65 relays, using default relays",
                self.pubkey,
            );
            Ok(Relay::defaults())
        } else {
            Ok(stored_relays)
        }
    }

    async fn update_nip65_relays(
        &self,
        whitenoise: &Whitenoise,
        query_relays: &[Relay],
    ) -> Result<Vec<Relay>> {
        match self
            .sync_relays_for_type(whitenoise, RelayType::Nip65, query_relays)
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

    async fn update_secondary_relay_types(
        &self,
        whitenoise: &Whitenoise,
        query_relays: &[Relay],
    ) -> Result<()> {
        const SECONDARY_RELAY_TYPES: &[RelayType] = &[RelayType::Inbox, RelayType::KeyPackage];

        for &relay_type in SECONDARY_RELAY_TYPES {
            if let Err(e) = self
                .sync_relays_for_type(whitenoise, relay_type, query_relays)
                .await
            {
                tracing::warn!(
                    target: "whitenoise::users::update_secondary_relay_types",
                    "Failed to update {:?} relays for user {}: {}",
                    relay_type,
                    self.pubkey,
                    e
                );
                // Continue with other relay types - individual failures shouldn't stop the process
            }
        }

        Ok(())
    }

    /// Synchronizes stored relays with a new set of relay URLs
    ///
    /// This method compares the currently stored relays with a new set of relay URLs,
    /// removing relays that are no longer present and adding new ones. This is the
    /// core synchronization logic used by both network-fetched updates and direct
    /// event processing.
    ///
    /// Returns `true` if changes were made, `false` if no changes needed
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

        // Apply changes
        tracing::info!(
            target: "whitenoise::users::sync_relay_urls",
            "Updating {:?} relays for user {}: {} existing -> {} new",
            relay_type,
            self.pubkey,
            stored_urls.len(),
            new_urls_set.len()
        );

        // Remove relays that are no longer needed
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
            }
        }

        // Add new relays
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
                }
            }
        }

        Ok(true)
    }

    /// Synchronizes relays for a specific type with the network state
    ///
    /// Returns `true` if changes were made, `false` if no changes needed
    async fn sync_relays_for_type(
        &self,
        whitenoise: &Whitenoise,
        relay_type: RelayType,
        query_relays: &[Relay],
    ) -> Result<bool> {
        let relays_urls: Vec<_> = Relay::urls(query_relays);
        let relay_event = whitenoise
            .nostr
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

        match relay_event {
            Some(event) => {
                let relay_hashset: HashSet<_> = NostrManager::relay_urls_from_event(&event);
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
                        .nostr
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

    pub(crate) async fn all_users_with_relay_urls(
        whitenoise: &Whitenoise,
    ) -> Result<Vec<(PublicKey, Vec<RelayUrl>)>> {
        Self::all_with_nip65_relay_urls(&whitenoise.database).await
    }

    /// Determines whether metadata should be updated based on comprehensive event processing logic.
    ///
    /// This method implements the complete logic for deciding whether to process a metadata event:
    /// 1. Check if we've already processed this specific event (avoid double processing)
    /// 2. If user is newly created, always accept the event
    /// 3. If user has default metadata, always accept the event
    /// 4. Otherwise, check if the event timestamp is newer than or equal to stored timestamp
    ///
    /// # Arguments
    /// * `event_id` - The ID of the metadata event being considered
    /// * `event_datetime` - The datetime of the metadata event being considered
    /// * `newly_created` - Whether this user was just created
    /// * `database` - Database connection for checking processed events
    ///
    /// # Returns
    /// * `Ok(true)` if metadata should be updated
    /// * `Ok(false)` if the event should be ignored
    pub(crate) async fn should_update_metadata(
        &self,
        event: &Event,
        newly_created: bool,
        database: &crate::whitenoise::database::Database,
    ) -> Result<bool> {
        // Check if we've already processed this specific event from this author
        let already_processed = ProcessedEvent::exists(&event.id, None, database)
            .await
            .map_err(WhitenoiseError::Database)?;

        if already_processed {
            tracing::debug!(
                target: "whitenoise::users::should_update_metadata",
                "Skipping already processed metadata event {} from author {}",
                event.id.to_hex(),
                self.pubkey.to_hex()
            );
            return Ok(false);
        }

        // If user is newly created, always accept the metadata
        if newly_created {
            tracing::debug!(
                target: "whitenoise::users::should_update_metadata",
                "Accepting metadata event for newly created user {}",
                self.pubkey
            );
            return Ok(true);
        }

        // If the current metadata is the same as the new metadata, don't update
        if self.metadata == Metadata::from_json(&event.content)? {
            tracing::debug!(
                target: "whitenoise::users::should_update_metadata",
                "Skipping metadata event for user {} because it's the same as the current metadata",
                self.pubkey
            );
            return Ok(false);
        }

        // Check timestamp against most recent processed metadata event for this specific user
        let newest_processed_timestamp =
            ProcessedEvent::newest_event_timestamp_for_kind(None, 0, Some(&self.pubkey), database)
                .await
                .map_err(WhitenoiseError::Database)?;

        let should_update = match newest_processed_timestamp {
            None => {
                tracing::debug!(
                    target: "whitenoise::users::should_update_metadata",
                    "No processed metadata events for user {}, accepting new event",
                    self.pubkey
                );
                true
            }
            Some(stored_timestamp) => {
                let event_datetime = timestamp_to_datetime(event.created_at)?;
                let is_newer_or_equal =
                    event_datetime.timestamp_millis() >= stored_timestamp.timestamp_millis();
                if !is_newer_or_equal {
                    tracing::debug!(
                        target: "whitenoise::users::should_update_metadata",
                        "Ignoring stale metadata event for user {} (event: {}, stored: {})",
                        self.pubkey,
                        event_datetime,
                        stored_timestamp
                    );
                }
                is_newer_or_equal
            }
        };

        Ok(should_update)
    }
}

/// Checks whether a key package event has the required `["encoding", "base64"]` tag.
///
/// Per MIP-00/MIP-02, key package events must include an explicit encoding tag.
/// Old key packages published before this requirement lack this tag and are
/// incompatible with current clients.
fn has_valid_encoding_tag(event: &Event) -> bool {
    event.tags.iter().any(|tag| {
        let s = tag.as_slice();
        s.len() >= 2 && s[0] == "encoding" && s[1] == "base64"
    })
}

/// Determines [`KeyPackageStatus`] from an optional key package event.
fn classify_key_package(event: Option<Event>) -> KeyPackageStatus {
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

impl Whitenoise {
    /// Retrieves a user by their public key.
    ///
    /// This method looks up a user in the database using their Nostr public key.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The Nostr public key of the user to retrieve
    ///
    /// # Returns
    ///
    /// Returns a `Result<User>` containing:
    /// - `Ok(User)` - The user with the specified public key, including their metadata
    /// - `Err(WhitenoiseError)` - If the user is not found or there's a database error
    ///
    /// # Examples
    ///
    /// ```rust
    /// use nostr_sdk::PublicKey;
    /// use whitenoise::Whitenoise;
    ///
    /// # async fn example(whitenoise: &Whitenoise) -> Result<(), Box<dyn std::error::Error>> {
    /// let pubkey = PublicKey::parse("npub1...")?;
    /// let user = whitenoise.find_user_by_pubkey(&pubkey).await?;
    /// println!("Found user: {:?}", user.metadata.name);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// This method will return an error if:
    /// - The user with the specified public key doesn't exist in the database
    /// - There's a database connection or query error
    /// - The public key format is invalid (though this is typically caught at the type level)
    pub async fn find_user_by_pubkey(&self, pubkey: &PublicKey) -> Result<User> {
        User::find_by_pubkey(pubkey, &self.database).await
    }

    /// Finds a user by their public key or creates a new one if not found.
    ///
    /// This method looks up a user in the database using their Nostr public key.
    /// If the user doesn't exist, it creates a new user record and synchronizes
    /// their metadata and relay lists according to the specified sync mode.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The Nostr public key of the user to find or create
    /// * `sync_mode` - Controls how user data is synchronized:
    ///   - `UserSyncMode::Blocking` - Performs immediate blocking sync before returning
    ///   - `UserSyncMode::Background` - Returns immediately and syncs in the background
    ///
    /// # Returns
    ///
    /// Returns a `Result<User>` containing:
    /// - `Ok(User)` - The found or created user
    /// - `Err(WhitenoiseError)` - If there's a database error
    ///
    /// # Examples
    ///
    /// ```rust
    /// use nostr_sdk::PublicKey;
    /// use whitenoise::{UserSyncMode, Whitenoise};
    ///
    /// # async fn example(whitenoise: &Whitenoise) -> Result<(), Box<dyn std::error::Error>> {
    /// let pubkey = PublicKey::parse("npub1...")?;
    ///
    /// // Fast, non-blocking call with background sync
    /// let user = whitenoise
    ///     .find_or_create_user_by_pubkey(&pubkey, UserSyncMode::Background)
    ///     .await?;
    ///
    /// // Slower, blocking call with immediate metadata
    /// let user_with_metadata = whitenoise
    ///     .find_or_create_user_by_pubkey(&pubkey, UserSyncMode::Blocking)
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// This method will return an error if:
    /// - There's a database connection or query error
    /// - The public key format is invalid (though this is typically caught at the type level)
    /// - Network errors occur during blocking synchronization
    pub async fn find_or_create_user_by_pubkey(
        &self,
        pubkey: &PublicKey,
        sync_mode: UserSyncMode,
    ) -> Result<User> {
        let (user, created) = User::find_or_create_by_pubkey(pubkey, &self.database).await?;

        if sync_mode == UserSyncMode::Blocking {
            let updated_user = self.sync_user_blocking(&user, created).await?;
            Ok(updated_user)
        } else {
            self.sync_user_background(&user, created).await?;
            Ok(user)
        }
    }

    async fn sync_user_blocking(&self, user: &User, is_new: bool) -> Result<User> {
        tracing::debug!(
            target: "whitenoise::users::sync_user_blocking",
            "Force sync requested for user {}, performing blocking metadata and relay sync",
            user.pubkey
        );

        let mut user_clone = user.clone();

        if is_new {
            // For new users, sync relay lists first so we have a good chance of finding their events
            if let Err(e) = user_clone.update_relay_lists(self).await {
                tracing::warn!(
                    target: "whitenoise::users::sync_user_blocking",
                    "Failed to sync relay lists for new user {}: {}",
                    user_clone.pubkey,
                    e
                );
            }
            // For new users, we need to add the user to the global subscriptions batches so we get updates on their events
            if let Err(e) = self.refresh_global_subscription_for_user(&user_clone).await {
                tracing::warn!(
                    target: "whitenoise::users::sync_user_blocking",
                    "Failed to refresh global subscription for new user {}: {}",
                    user_clone.pubkey,
                    e
                );
            }
        }

        // Always sync metadata when force_sync is true
        if let Err(e) = user_clone.sync_metadata(self).await {
            tracing::warn!(
                target: "whitenoise::users::sync_user_blocking",
                "Failed to sync metadata for user {}: {}",
                user_clone.pubkey,
                e
            );
        }

        Ok(user_clone)
    }

    async fn sync_user_background(&self, user: &User, is_new: bool) -> Result<()> {
        if is_new {
            if let Err(e) = self.background_fetch_user_data(user).await {
                tracing::warn!(
                    target: "whitenoise::users::sync_user_background",
                    "Failed to start background fetch for new user {}: {}",
                    user.pubkey,
                    e
                );
            }
        } else if user.needs_metadata_refresh() {
            // For existing users, only sync if metadata is stale
            tracing::debug!(
                target: "whitenoise::users::sync_user_background",
                "User {} metadata is stale (updated_at: {}), starting background refresh",
                user.pubkey,
                user.updated_at
            );
            if let Err(e) = self.background_fetch_user_data(user).await {
                tracing::warn!(
                    target: "whitenoise::users::sync_user_background",
                    "Failed to start background fetch for stale user {}: {}",
                    user.pubkey,
                    e
                );
            }
        } else {
            tracing::debug!(
                target: "whitenoise::users::sync_user_background",
                "User {} metadata is fresh (updated_at: {}), skipping sync",
                user.pubkey,
                user.updated_at
            );
        }

        Ok(())
    }

    pub(crate) async fn background_fetch_user_data(&self, user: &User) -> Result<()> {
        let user_clone = user.clone();
        let mut mut_user_clone = user.clone();

        tokio::spawn(async move {
            let whitenoise = Whitenoise::get_instance()?;
            // Do these in series so that we fetch the user's relays before trying to fetch metadata
            // (more likely we find metadata looking on the right relays)
            let relay_result = user_clone.update_relay_lists(whitenoise).await;
            let metadata_result = mut_user_clone.sync_metadata(whitenoise).await;

            // Log errors but don't fail
            if let Err(e) = relay_result {
                tracing::warn!(
                    "Failed to fetch relay lists for {}: {}",
                    user_clone.pubkey,
                    e
                );
            }
            if let Err(e) = metadata_result {
                tracing::warn!("Failed to fetch metadata for {}: {}", user_clone.pubkey, e);
            }

            if let Err(e) = whitenoise
                .refresh_global_subscription_for_user(&user_clone)
                .await
            {
                tracing::warn!(
                    target: "whitenoise::users::background_fetch_user_data",
                    "Failed to refresh global subscription for {}: {}",
                    user_clone.pubkey,
                    e
                );
            }

            Ok::<(), WhitenoiseError>(())
        });
        Ok(())
    }

    /// **TEST-ONLY**: Sets the `updated_at` timestamp for a user to a specific value.
    ///
    /// This method is only available when the `integration-tests` feature is enabled
    /// and is intended for testing TTL-based metadata refresh logic.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The public key of the user to update
    /// * `updated_at` - The new timestamp to set
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if successful, or an error if the user doesn't exist or
    /// there's a database error.
    ///
    /// # Example (in integration tests)
    ///
    /// ```rust,ignore
    /// use chrono::{Utc, Duration};
    ///
    /// // Set user's metadata to be 25 hours old (stale)
    /// let stale_time = Utc::now() - Duration::hours(25);
    /// whitenoise.set_user_updated_at_for_testing(&pubkey, stale_time).await?;
    /// ```
    #[cfg(feature = "integration-tests")]
    pub async fn set_user_updated_at_for_testing(
        &self,
        pubkey: &PublicKey,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query("UPDATE users SET updated_at = ? WHERE pubkey = ?")
            .bind(updated_at.timestamp_millis())
            .bind(pubkey.to_hex())
            .execute(&self.database.pool)
            .await
            .map_err(crate::whitenoise::database::DatabaseError::Sqlx)?;

        tracing::debug!(
            target: "whitenoise::users::set_user_updated_at_for_testing",
            "Set updated_at for user {} to {} (TEST ONLY)",
            pubkey,
            updated_at
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use chrono::Utc;
    use std::collections::HashSet;

    #[test]
    fn test_basic_relay_url_equality() {
        let url1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
        let url2 = RelayUrl::parse("wss://relay1.example.com").unwrap();
        let url3 = RelayUrl::parse("wss://relay2.example.com").unwrap();

        assert_eq!(url1, url2);
        assert_ne!(url1, url3);

        let mut url_set = HashSet::new();
        url_set.insert(&url1);
        url_set.insert(&url2); // Should not increase size since url1 == url2
        url_set.insert(&url3);

        assert_eq!(url_set.len(), 2);
        assert!(url_set.contains(&url1));
        assert!(url_set.contains(&url3));
    }

    #[tokio::test]
    async fn test_update_relay_lists_success() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new().name("Test User"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let saved_user = user.save(&whitenoise.database).await.unwrap();
        let initial_relay_url = RelayUrl::parse("wss://initial.example.com").unwrap();
        let initial_relay = whitenoise
            .find_or_create_relay_by_url(&initial_relay_url)
            .await
            .unwrap();

        saved_user
            .add_relay(&initial_relay, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        saved_user.update_relay_lists(&whitenoise).await.unwrap();
        let relays = saved_user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].url, initial_relay_url);
    }

    #[tokio::test]
    async fn test_update_relay_lists_with_no_initial_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new().name("Test User No Relays"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let saved_user = user.save(&whitenoise.database).await.unwrap();

        saved_user.update_relay_lists(&whitenoise).await.unwrap();
        assert!(
            saved_user
                .relays(RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_get_query_relays_with_stored_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let saved_user = user.save(&whitenoise.database).await.unwrap();

        // Add a relay
        let relay_url = RelayUrl::parse("wss://test.example.com").unwrap();
        let relay = whitenoise
            .find_or_create_relay_by_url(&relay_url)
            .await
            .unwrap();
        saved_user
            .add_relay(&relay, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        // Test get_query_relays
        let query_relays = saved_user.get_query_relays(&whitenoise).await.unwrap();

        assert_eq!(query_relays.len(), 1);
        assert_eq!(query_relays[0].url, relay_url);
    }

    #[tokio::test]
    async fn test_get_query_relays_with_no_stored_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let saved_user = user.save(&whitenoise.database).await.unwrap();
        let query_relays = saved_user.get_query_relays(&whitenoise).await.unwrap();

        assert_eq!(Relay::urls(&query_relays), Relay::urls(&Relay::defaults()));
    }

    #[tokio::test]
    async fn test_update_metadata_with_working_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new().name("Original Name"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let mut saved_user = user.save(&whitenoise.database).await.unwrap();

        for default_relay in &Relay::defaults() {
            let relay = whitenoise
                .find_or_create_relay_by_url(&default_relay.url)
                .await
                .unwrap();
            saved_user
                .add_relay(&relay, RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();
        }

        let original_metadata = saved_user.metadata.clone();
        let result = saved_user.sync_metadata(&whitenoise).await;

        assert!(result.is_ok());

        let user_after = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(user_after.metadata.name, original_metadata.name);
        assert_eq!(user_after.pubkey, test_pubkey);
    }

    #[tokio::test]
    async fn test_update_metadata_with_no_nip65_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new().name("Test User"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let mut saved_user = user.save(&whitenoise.database).await.unwrap();
        let result = saved_user.sync_metadata(&whitenoise).await;

        assert!(result.is_ok());

        let user_after = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(user_after.metadata.name, Some("Test User".to_string()));
        assert_eq!(user_after.pubkey, test_pubkey);
    }

    #[tokio::test]
    async fn test_update_metadata_preserves_user_state() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new().name("Test User").about("Test description"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let mut saved_user = user.save(&whitenoise.database).await.unwrap();

        let relay_url = RelayUrl::parse("ws://localhost:7777").unwrap();
        let relay = whitenoise
            .find_or_create_relay_by_url(&relay_url)
            .await
            .unwrap();
        saved_user
            .add_relay(&relay, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        let original_id = saved_user.id;
        let result = saved_user.sync_metadata(&whitenoise).await;

        assert!(result.is_ok());

        let final_user = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(final_user.id, original_id);
        assert_eq!(final_user.pubkey, test_pubkey);
        assert_eq!(final_user.metadata.name, Some("Test User".to_string()));
        assert_eq!(
            final_user.metadata.about,
            Some("Test description".to_string())
        );
    }

    #[tokio::test]
    async fn test_all_users_with_relay_urls() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let users_with_relays = User::all_users_with_relay_urls(&whitenoise).await.unwrap();
        assert!(users_with_relays.is_empty());

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new().name("Test User"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let saved_user = user.save(&whitenoise.database).await.unwrap();
        let relay_url = RelayUrl::parse("wss://test.example.com").unwrap();
        let relay = whitenoise
            .find_or_create_relay_by_url(&relay_url)
            .await
            .unwrap();
        saved_user
            .add_relay(&relay, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        let users_with_relays = User::all_users_with_relay_urls(&whitenoise).await.unwrap();
        assert_eq!(users_with_relays.len(), 1);
        assert_eq!(users_with_relays[0].0, test_pubkey);
        assert_eq!(users_with_relays[0].1, vec![relay_url]);
    }

    #[tokio::test]
    async fn test_all_users_with_relay_urls_multiple_users_multiple_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create two users, each with two NIP-65 relays
        let pubkey_a = nostr_sdk::Keys::generate().public_key();
        let pubkey_b = nostr_sdk::Keys::generate().public_key();

        let user_a = User {
            id: None,
            pubkey: pubkey_a,
            metadata: Metadata::new().name("User A"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
        .save(&whitenoise.database)
        .await
        .unwrap();

        let user_b = User {
            id: None,
            pubkey: pubkey_b,
            metadata: Metadata::new().name("User B"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
        .save(&whitenoise.database)
        .await
        .unwrap();

        let relay1 = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://relay1.example.com").unwrap())
            .await
            .unwrap();
        let relay2 = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://relay2.example.com").unwrap())
            .await
            .unwrap();
        let relay3 = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://relay3.example.com").unwrap())
            .await
            .unwrap();

        user_a
            .add_relay(&relay1, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        user_a
            .add_relay(&relay2, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        user_b
            .add_relay(&relay2, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        user_b
            .add_relay(&relay3, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        let results = User::all_users_with_relay_urls(&whitenoise).await.unwrap();
        assert_eq!(results.len(), 2);

        // Find each user's entry (order is by pubkey hex, not insertion order)
        let a_entry = results.iter().find(|(pk, _)| *pk == pubkey_a).unwrap();
        let b_entry = results.iter().find(|(pk, _)| *pk == pubkey_b).unwrap();

        assert_eq!(a_entry.1.len(), 2);
        assert!(
            a_entry
                .1
                .contains(&RelayUrl::parse("wss://relay1.example.com").unwrap())
        );
        assert!(
            a_entry
                .1
                .contains(&RelayUrl::parse("wss://relay2.example.com").unwrap())
        );

        assert_eq!(b_entry.1.len(), 2);
        assert!(
            b_entry
                .1
                .contains(&RelayUrl::parse("wss://relay2.example.com").unwrap())
        );
        assert!(
            b_entry
                .1
                .contains(&RelayUrl::parse("wss://relay3.example.com").unwrap())
        );
    }

    #[tokio::test]
    async fn test_all_users_with_relay_urls_user_with_no_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a user with NIP-65 relays and one without
        let pubkey_with = nostr_sdk::Keys::generate().public_key();
        let pubkey_without = nostr_sdk::Keys::generate().public_key();

        let user_with = User {
            id: None,
            pubkey: pubkey_with,
            metadata: Metadata::new().name("Has Relays"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
        .save(&whitenoise.database)
        .await
        .unwrap();

        User {
            id: None,
            pubkey: pubkey_without,
            metadata: Metadata::new().name("No Relays"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
        .save(&whitenoise.database)
        .await
        .unwrap();

        let relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://relay.example.com").unwrap())
            .await
            .unwrap();
        user_with
            .add_relay(&relay, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        let results = User::all_users_with_relay_urls(&whitenoise).await.unwrap();
        assert_eq!(results.len(), 2, "Both users should appear in results");

        let with_entry = results.iter().find(|(pk, _)| *pk == pubkey_with).unwrap();
        let without_entry = results
            .iter()
            .find(|(pk, _)| *pk == pubkey_without)
            .unwrap();

        assert_eq!(with_entry.1.len(), 1);
        assert!(
            without_entry.1.is_empty(),
            "User with no NIP-65 relays should have empty relay list"
        );
    }

    #[tokio::test]
    async fn test_all_users_with_relay_urls_non_nip65_relays_excluded() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a user with only Inbox and KeyPackage relays (no NIP-65)
        let pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey,
            metadata: Metadata::new().name("Inbox Only"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
        .save(&whitenoise.database)
        .await
        .unwrap();

        let relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://inbox.example.com").unwrap())
            .await
            .unwrap();
        user.add_relay(&relay, RelayType::Inbox, &whitenoise.database)
            .await
            .unwrap();
        user.add_relay(&relay, RelayType::KeyPackage, &whitenoise.database)
            .await
            .unwrap();

        let results = User::all_users_with_relay_urls(&whitenoise).await.unwrap();
        assert_eq!(results.len(), 1, "User should appear in results");
        assert!(
            results[0].1.is_empty(),
            "Non-NIP-65 relays should not be included"
        );
    }

    #[tokio::test]
    async fn test_key_package_event_gradual_relay_addition() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new().name("Test User"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let saved_user = user.save(&whitenoise.database).await.unwrap();

        // Test 1: No relays - should return None
        let kp_relays = saved_user
            .relays(RelayType::KeyPackage, &whitenoise.database)
            .await
            .unwrap();
        assert!(kp_relays.is_empty());

        let nip65_relays = saved_user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        assert!(nip65_relays.is_empty());

        let result = saved_user.key_package_event(&whitenoise).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);

        // Test 2: Add only NIP-65 relays - expect Ok(None); actual usage not asserted here
        let nip65_relay_url = RelayUrl::parse("ws://localhost:7777").unwrap();
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&nip65_relay_url)
            .await
            .unwrap();
        saved_user
            .add_relay(&nip65_relay, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        let result = saved_user.key_package_event(&whitenoise).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);

        // Test 3: Add a key package relay - expect Ok(None); priority over NIP-65 not asserted here
        let kp_relay_url = RelayUrl::parse("ws://localhost:8080").unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&kp_relay_url)
            .await
            .unwrap();
        saved_user
            .add_relay(&kp_relay, RelayType::KeyPackage, &whitenoise.database)
            .await
            .unwrap();

        let result = saved_user.key_package_event(&whitenoise).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    mod should_update_metadata_tests {
        use super::*;
        use crate::whitenoise::database::processed_events::ProcessedEvent;

        async fn create_test_user(whitenoise: &Whitenoise) -> User {
            let keys = Keys::generate();
            User::find_or_create_by_pubkey(&keys.public_key(), &whitenoise.database)
                .await
                .unwrap()
                .0
        }

        async fn create_test_metadata_event(name: Option<String>) -> Event {
            let keys = Keys::generate();
            let name = name.unwrap_or_else(|| "Test User".to_string());
            let event_builder = EventBuilder::metadata(&Metadata::new().name(name));
            event_builder.sign(&keys).await.unwrap()
        }

        #[tokio::test]
        async fn test_should_update_metadata_already_processed() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let user = create_test_user(&whitenoise).await;
            let event = create_test_metadata_event(None).await;

            // First, create a processed event entry
            ProcessedEvent::create(
                &event.id,
                None, // Global events
                Some(timestamp_to_datetime(event.created_at).unwrap()),
                Some(Kind::Metadata), // Metadata kind
                Some(&user.pubkey),
                &whitenoise.database,
            )
            .await
            .unwrap();

            // Test that already processed event returns false
            let result = user
                .should_update_metadata(&event, false, &whitenoise.database)
                .await
                .unwrap();

            assert!(!result);
        }

        #[tokio::test]
        async fn test_should_update_metadata_newly_created_user() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let user = create_test_user(&whitenoise).await;
            let event = create_test_metadata_event(None).await;

            // Test that newly created user always returns true
            let result = user
                .should_update_metadata(&event, true, &whitenoise.database)
                .await
                .unwrap();

            assert!(result);
        }

        #[tokio::test]
        async fn test_should_update_metadata_default_metadata() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let mut user = create_test_user(&whitenoise).await;
            let event = create_test_metadata_event(None).await;

            // Ensure user has default metadata
            user.metadata = Metadata::default();
            user.save(&whitenoise.database).await.unwrap();

            // Test that user with default metadata returns true
            let result = user
                .should_update_metadata(&event, false, &whitenoise.database)
                .await
                .unwrap();

            assert!(result);
        }

        #[tokio::test]
        async fn test_should_update_metadata_no_processed_events() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let mut user = create_test_user(&whitenoise).await;
            let event = create_test_metadata_event(Some("Andy Waterman".to_string())).await;

            // Give user some non-default metadata
            user.metadata = Metadata::new().name("Test User");
            user.save(&whitenoise.database).await.unwrap();

            // Test that with no processed events, returns true
            let result = user
                .should_update_metadata(&event, false, &whitenoise.database)
                .await
                .unwrap();

            assert!(result);
        }

        #[tokio::test]
        async fn test_should_update_metadata_newer_event() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let mut user = create_test_user(&whitenoise).await;
            let old_event = create_test_metadata_event(None).await;

            // Give user some non-default metadata
            user.metadata = Metadata::new().name("Test User");
            user.save(&whitenoise.database).await.unwrap();

            // Create an older processed event
            ProcessedEvent::create(
                &old_event.id,
                None, // Global events
                Some(timestamp_to_datetime(old_event.created_at).unwrap()),
                Some(Kind::Metadata), // Metadata kind
                Some(&user.pubkey),
                &whitenoise.database,
            )
            .await
            .unwrap();

            // Create a newer event (just create a fresh one, it should be newer due to timing)
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            let new_event = create_test_metadata_event(Some("Bobby Tables".to_string())).await;

            // Test that newer event returns true
            let result = user
                .should_update_metadata(&new_event, false, &whitenoise.database)
                .await
                .unwrap();

            assert!(result);
        }

        #[tokio::test]
        async fn test_should_update_metadata_equal_timestamp() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let mut user = create_test_user(&whitenoise).await;
            let old_event = create_test_metadata_event(None).await;

            // Give user some non-default metadata
            user.metadata = Metadata::new().name("Test User");
            user.save(&whitenoise.database).await.unwrap();

            // Create a processed event
            ProcessedEvent::create(
                &old_event.id,
                None, // Global events
                Some(timestamp_to_datetime(old_event.created_at).unwrap()),
                Some(Kind::Metadata), // Metadata kind
                Some(&user.pubkey),
                &whitenoise.database,
            )
            .await
            .unwrap();

            // Create an event with the same timestamp but different content/ID
            let keys = Keys::generate();
            let event_builder = EventBuilder::metadata(&Metadata::new().name("Different Name"));
            let mut new_event = event_builder.sign(&keys).await.unwrap();
            // Force the same timestamp for testing
            new_event.created_at = old_event.created_at;

            // Test that equal timestamp returns true (newer or equal)
            let result = user
                .should_update_metadata(&new_event, false, &whitenoise.database)
                .await
                .unwrap();

            assert!(result);
        }

        #[tokio::test]
        async fn test_should_update_metadata_older_event() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let mut user = create_test_user(&whitenoise).await;
            let newer_event = create_test_metadata_event(None).await;

            // Give user some non-default metadata
            user.metadata = Metadata::new().name("Test User");
            user.save(&whitenoise.database).await.unwrap();

            // Create a newer processed event
            ProcessedEvent::create(
                &newer_event.id,
                None, // Global events
                Some(timestamp_to_datetime(newer_event.created_at).unwrap()),
                Some(Kind::Metadata), // Metadata kind
                Some(&user.pubkey),
                &whitenoise.database,
            )
            .await
            .unwrap();

            // Create an older event
            let keys = Keys::generate();
            let event_builder = EventBuilder::metadata(&Metadata::new().name("Old Name"));
            let mut old_event = event_builder.sign(&keys).await.unwrap();
            // Force an older timestamp for testing
            old_event.created_at = newer_event.created_at - 3600; // 1 hour earlier

            // Test that older event returns false
            let result = user
                .should_update_metadata(&old_event, false, &whitenoise.database)
                .await
                .unwrap();

            assert!(!result);
        }

        #[tokio::test]
        async fn test_should_update_metadata_priority_order() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let mut user = create_test_user(&whitenoise).await;
            let event = create_test_metadata_event(Some("Andy Waterman".to_string())).await;

            // Give user some non-default metadata
            user.metadata = Metadata::new().name("Test User");
            user.save(&whitenoise.database).await.unwrap();

            // Create a processed event entry for this exact event
            ProcessedEvent::create(
                &event.id,
                None, // Global events
                Some(timestamp_to_datetime(event.created_at).unwrap()),
                Some(Kind::Metadata), // Metadata kind
                Some(&user.pubkey),
                &whitenoise.database,
            )
            .await
            .unwrap();

            // Test that already processed takes priority over newly_created
            // Even though newly_created=true, it should return false because event is already processed
            let result = user
                .should_update_metadata(&event, true, &whitenoise.database)
                .await
                .unwrap();

            assert!(!result);
        }

        #[tokio::test]
        async fn test_should_update_metadata_newly_created_with_default_metadata() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let mut user = create_test_user(&whitenoise).await;
            let event = create_test_metadata_event(None).await;

            // Ensure user has default metadata (redundant but explicit)
            user.metadata = Metadata::default();
            user.save(&whitenoise.database).await.unwrap();

            // Test that newly created user with default metadata returns true
            // (both conditions would return true, but newly_created takes priority)
            let result = user
                .should_update_metadata(&event, true, &whitenoise.database)
                .await
                .unwrap();

            assert!(result);
        }

        #[tokio::test]
        async fn test_needs_metadata_refresh_default_metadata() {
            let test_pubkey = nostr_sdk::Keys::generate().public_key();
            let user = User {
                id: Some(1),
                pubkey: test_pubkey,
                metadata: Metadata::new(), // Default empty metadata
                created_at: Utc::now(),
                updated_at: Utc::now(), // Even if recently updated
            };

            // Should always refresh if metadata is default/empty
            assert!(user.needs_metadata_refresh());
        }

        #[tokio::test]
        async fn test_needs_metadata_refresh_fresh_metadata() {
            let test_pubkey = nostr_sdk::Keys::generate().public_key();
            let user = User {
                id: Some(1),
                pubkey: test_pubkey,
                metadata: Metadata::new().name("Test User"), // Non-default metadata
                created_at: Utc::now(),
                updated_at: Utc::now(), // Recently updated
            };

            // Should not refresh if metadata is fresh and non-default
            assert!(!user.needs_metadata_refresh());
        }

        #[tokio::test]
        async fn test_needs_metadata_refresh_stale_metadata() {
            let test_pubkey = nostr_sdk::Keys::generate().public_key();
            let stale_time = Utc::now() - Duration::hours(METADATA_TTL_HOURS + 1); // Older than TTL
            let user = User {
                id: Some(1),
                pubkey: test_pubkey,
                metadata: Metadata::new().name("Test User"), // Non-default metadata
                created_at: stale_time,
                updated_at: stale_time, // Old update time
            };

            // Should refresh if metadata is older than TTL
            assert!(user.needs_metadata_refresh());
        }

        #[tokio::test]
        async fn test_needs_metadata_refresh_boundary_case() {
            let test_pubkey = nostr_sdk::Keys::generate().public_key();
            let boundary_time =
                Utc::now() - Duration::hours(METADATA_TTL_HOURS) + Duration::minutes(1);
            let user = User {
                id: Some(1),
                pubkey: test_pubkey,
                metadata: Metadata::new().name("Test User"),
                created_at: boundary_time,
                updated_at: boundary_time,
            };

            // Should not refresh if just within TTL boundary
            assert!(!user.needs_metadata_refresh());
        }

        // NOTE: The following tests have LIMITED COVERAGE due to lack of mocking.
        // They can only verify database operations and method completion.
        // They CANNOT verify that sync methods (sync_metadata, update_relay_lists,
        // background_fetch_user_data) are actually called vs skipped.
        // For true behavioral testing, see integration tests in:
        // src/integration_tests/test_cases/user_discovery/find_or_create_user.rs

        #[tokio::test]
        async fn test_find_or_create_creates_new_user() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = nostr_sdk::Keys::generate().public_key();

            // Verify user doesn't exist
            assert!(whitenoise.find_user_by_pubkey(&test_pubkey).await.is_err());

            // Create user with Background mode to avoid network calls
            let user = whitenoise
                .find_or_create_user_by_pubkey(&test_pubkey, UserSyncMode::Background)
                .await
                .unwrap();

            // TESTS: User creation and database persistence
            assert_eq!(user.pubkey, test_pubkey);
            assert!(user.id.is_some());

            // LIMITATION: Cannot verify if background_fetch_user_data was called
            // TODO: Find a way to mock or spy on the sync methods to verify they were called
        }

        #[tokio::test]
        async fn test_find_or_create_returns_existing_user() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = nostr_sdk::Keys::generate().public_key();

            // Create user directly in database
            let original_user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new().name("Original User"),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = original_user.save(&whitenoise.database).await.unwrap();

            // Call find_or_create
            let found_user = whitenoise
                .find_or_create_user_by_pubkey(&test_pubkey, UserSyncMode::Background)
                .await
                .unwrap();

            // TESTS: Existing user retrieval
            assert_eq!(found_user.id, saved_user.id);
            assert_eq!(found_user.pubkey, test_pubkey);
            assert_eq!(found_user.metadata.name, Some("Original User".to_string()));

            // LIMITATION: Cannot verify if sync was skipped due to fresh metadata
            // TODO: Find a way to mock or spy on the sync methods to verify they were called
        }

        #[tokio::test]
        async fn test_find_or_create_returns_immediately_with_background_sync() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = nostr_sdk::Keys::generate().public_key();

            // TESTS: Method returns immediately without blocking on network
            let user = whitenoise
                .find_or_create_user_by_pubkey(&test_pubkey, UserSyncMode::Background)
                .await
                .unwrap();

            assert_eq!(user.pubkey, test_pubkey);
            assert!(user.id.is_some());
        }
    }

    mod sync_relay_urls_tests {
        use super::*;
        use crate::whitenoise::{
            database::processed_events::ProcessedEvent, test_utils::create_mock_whitenoise,
        };

        #[tokio::test]
        async fn test_sync_relay_urls_ignores_stale_event() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let relay_url = RelayUrl::parse("wss://relay1.example.com").unwrap();
            let relay = whitenoise
                .find_or_create_relay_by_url(&relay_url)
                .await
                .unwrap();
            saved_user
                .add_relay(&relay, RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            let newer_timestamp = Utc::now();
            ProcessedEvent::create(
                &EventId::all_zeros(),
                None,
                Some(newer_timestamp),
                Some(Kind::from(10002)),
                Some(&test_pubkey),
                &whitenoise.database,
            )
            .await
            .unwrap();

            let stale_timestamp = newer_timestamp - Duration::hours(1);
            let new_relay_urls =
                HashSet::from([RelayUrl::parse("wss://relay2.example.com").unwrap()]);

            let changed = saved_user
                .sync_relay_urls(
                    &whitenoise,
                    RelayType::Nip65,
                    &new_relay_urls,
                    Some(stale_timestamp),
                )
                .await
                .unwrap();

            assert!(!changed);
            let relays = saved_user
                .relays(RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(relays.len(), 1);
            assert_eq!(relays[0].url, relay_url);
        }

        #[tokio::test]
        async fn test_sync_relay_urls_accepts_newer_event() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let old_relay_url = RelayUrl::parse("wss://relay1.example.com").unwrap();
            let relay = whitenoise
                .find_or_create_relay_by_url(&old_relay_url)
                .await
                .unwrap();
            saved_user
                .add_relay(&relay, RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            let old_timestamp = Utc::now() - Duration::hours(2);
            ProcessedEvent::create(
                &EventId::all_zeros(),
                None,
                Some(old_timestamp),
                Some(Kind::from(10002)),
                Some(&test_pubkey),
                &whitenoise.database,
            )
            .await
            .unwrap();

            let newer_timestamp = Utc::now() - Duration::hours(1);
            let new_relay_urls =
                HashSet::from([RelayUrl::parse("wss://relay2.example.com").unwrap()]);

            let changed = saved_user
                .sync_relay_urls(
                    &whitenoise,
                    RelayType::Nip65,
                    &new_relay_urls,
                    Some(newer_timestamp),
                )
                .await
                .unwrap();

            assert!(changed);
            let relays = saved_user
                .relays(RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(relays.len(), 1);
            assert_eq!(relays[0].url, new_relay_urls.iter().next().unwrap().clone());
        }

        #[tokio::test]
        async fn test_sync_relay_urls_rejects_equal_timestamp() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
            let r1 = whitenoise
                .find_or_create_relay_by_url(&relay1)
                .await
                .unwrap();
            saved_user
                .add_relay(&r1, RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            let timestamp = Utc::now() - Duration::hours(1);
            ProcessedEvent::create(
                &EventId::all_zeros(),
                None,
                Some(timestamp),
                Some(Kind::from(10002)),
                Some(&test_pubkey),
                &whitenoise.database,
            )
            .await
            .unwrap();

            let new_relay_urls =
                HashSet::from([RelayUrl::parse("wss://relay2.example.com").unwrap()]);

            let changed = saved_user
                .sync_relay_urls(
                    &whitenoise,
                    RelayType::Nip65,
                    &new_relay_urls,
                    Some(timestamp),
                )
                .await
                .unwrap();

            assert!(!changed);
            let relays = saved_user
                .relays(RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(relays.len(), 1);
            assert_eq!(relays[0].url, relay1);
        }

        #[tokio::test]
        async fn test_sync_relay_urls_no_timestamp_always_accepts() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let new_relay_urls =
                HashSet::from([RelayUrl::parse("wss://relay1.example.com").unwrap()]);

            let changed = saved_user
                .sync_relay_urls(&whitenoise, RelayType::Nip65, &new_relay_urls, None)
                .await
                .unwrap();

            assert!(changed);
            let relays = saved_user
                .relays(RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(relays.len(), 1);
        }

        #[tokio::test]
        async fn test_sync_relay_urls_no_changes_needed() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let relay_url = RelayUrl::parse("wss://relay1.example.com").unwrap();
            let relay = whitenoise
                .find_or_create_relay_by_url(&relay_url)
                .await
                .unwrap();
            saved_user
                .add_relay(&relay, RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            let same_relay_urls = HashSet::from([relay_url.clone()]);

            let changed = saved_user
                .sync_relay_urls(
                    &whitenoise,
                    RelayType::Nip65,
                    &same_relay_urls,
                    Some(Utc::now()),
                )
                .await
                .unwrap();

            assert!(!changed);
            let relays = saved_user
                .relays(RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(relays.len(), 1);
            assert_eq!(relays[0].url, relay_url);
        }

        #[tokio::test]
        async fn test_sync_relay_urls_adds_new_relays() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
            let relay = whitenoise
                .find_or_create_relay_by_url(&relay1)
                .await
                .unwrap();
            saved_user
                .add_relay(&relay, RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            let new_relay_urls = HashSet::from([
                relay1.clone(),
                RelayUrl::parse("wss://relay2.example.com").unwrap(),
                RelayUrl::parse("wss://relay3.example.com").unwrap(),
            ]);

            let changed = saved_user
                .sync_relay_urls(
                    &whitenoise,
                    RelayType::Nip65,
                    &new_relay_urls,
                    Some(Utc::now()),
                )
                .await
                .unwrap();

            assert!(changed);
            let relays = saved_user
                .relays(RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(relays.len(), 3);
        }

        #[tokio::test]
        async fn test_sync_relay_urls_removes_old_relays() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
            let relay2 = RelayUrl::parse("wss://relay2.example.com").unwrap();
            let relay3 = RelayUrl::parse("wss://relay3.example.com").unwrap();

            for url in [&relay1, &relay2, &relay3] {
                let relay = whitenoise.find_or_create_relay_by_url(url).await.unwrap();
                saved_user
                    .add_relay(&relay, RelayType::Nip65, &whitenoise.database)
                    .await
                    .unwrap();
            }

            let new_relay_urls = HashSet::from([relay2.clone()]);

            let changed = saved_user
                .sync_relay_urls(
                    &whitenoise,
                    RelayType::Nip65,
                    &new_relay_urls,
                    Some(Utc::now()),
                )
                .await
                .unwrap();

            assert!(changed);
            let relays = saved_user
                .relays(RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(relays.len(), 1);
            assert_eq!(relays[0].url, relay2);
        }

        #[tokio::test]
        async fn test_sync_relay_urls_adds_and_removes() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
            let relay2 = RelayUrl::parse("wss://relay2.example.com").unwrap();

            for url in [&relay1, &relay2] {
                let relay = whitenoise.find_or_create_relay_by_url(url).await.unwrap();
                saved_user
                    .add_relay(&relay, RelayType::Nip65, &whitenoise.database)
                    .await
                    .unwrap();
            }

            let relay3 = RelayUrl::parse("wss://relay3.example.com").unwrap();
            let relay4 = RelayUrl::parse("wss://relay4.example.com").unwrap();
            let new_relay_urls = HashSet::from([relay2.clone(), relay3.clone(), relay4.clone()]);

            let changed = saved_user
                .sync_relay_urls(
                    &whitenoise,
                    RelayType::Nip65,
                    &new_relay_urls,
                    Some(Utc::now()),
                )
                .await
                .unwrap();

            assert!(changed);
            let relays = saved_user
                .relays(RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(relays.len(), 3);

            let urls: HashSet<_> = relays.iter().map(|r| &r.url).collect();
            assert!(urls.contains(&relay2));
            assert!(urls.contains(&relay3));
            assert!(urls.contains(&relay4));
            assert!(!urls.contains(&relay1));
        }

        #[tokio::test]
        async fn test_sync_relay_urls_different_relay_types_independent() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
            let relay2 = RelayUrl::parse("wss://relay2.example.com").unwrap();

            let r1 = whitenoise
                .find_or_create_relay_by_url(&relay1)
                .await
                .unwrap();
            saved_user
                .add_relay(&r1, RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            let r2 = whitenoise
                .find_or_create_relay_by_url(&relay2)
                .await
                .unwrap();
            saved_user
                .add_relay(&r2, RelayType::Inbox, &whitenoise.database)
                .await
                .unwrap();

            let new_nip65_urls = HashSet::from([relay2.clone()]);
            let changed = saved_user
                .sync_relay_urls(
                    &whitenoise,
                    RelayType::Nip65,
                    &new_nip65_urls,
                    Some(Utc::now()),
                )
                .await
                .unwrap();

            assert!(changed);
            let nip65_relays = saved_user
                .relays(RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(nip65_relays.len(), 1);
            assert_eq!(nip65_relays[0].url, relay2);

            let inbox_relays = saved_user
                .relays(RelayType::Inbox, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(inbox_relays.len(), 1);
            assert_eq!(inbox_relays[0].url, relay2);
        }

        #[tokio::test]
        async fn test_sync_relay_urls_empty_new_urls_removes_all() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let relay1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
            let r1 = whitenoise
                .find_or_create_relay_by_url(&relay1)
                .await
                .unwrap();
            saved_user
                .add_relay(&r1, RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            let empty_urls = HashSet::new();
            let changed = saved_user
                .sync_relay_urls(&whitenoise, RelayType::Nip65, &empty_urls, Some(Utc::now()))
                .await
                .unwrap();

            assert!(changed);
            let relays = saved_user
                .relays(RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(relays.len(), 0);
        }

        #[tokio::test]
        async fn test_sync_relay_urls_no_stored_timestamp_accepts_new() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let new_relay_urls =
                HashSet::from([RelayUrl::parse("wss://relay1.example.com").unwrap()]);

            let changed = saved_user
                .sync_relay_urls(
                    &whitenoise,
                    RelayType::Nip65,
                    &new_relay_urls,
                    Some(Utc::now()),
                )
                .await
                .unwrap();

            assert!(changed);
            let relays = saved_user
                .relays(RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(relays.len(), 1);
        }
    }

    mod update_nip65_relays_tests {
        use super::*;
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        #[tokio::test]
        async fn test_update_nip65_relays_returns_query_relays_on_no_event() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let query_relays = Relay::defaults();
            let result = saved_user
                .update_nip65_relays(&whitenoise, &query_relays)
                .await
                .unwrap();

            assert_eq!(result.len(), query_relays.len());
            for (r1, r2) in result.iter().zip(query_relays.iter()) {
                assert_eq!(r1.url, r2.url);
            }
        }

        #[tokio::test]
        async fn test_update_nip65_relays_returns_query_relays_on_error() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let query_relays = vec![];
            let result = saved_user
                .update_nip65_relays(&whitenoise, &query_relays)
                .await
                .unwrap();

            assert_eq!(result.len(), 0);
        }

        #[tokio::test]
        async fn test_update_nip65_relays_returns_updated_on_change() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let initial_relay = RelayUrl::parse("wss://initial.example.com").unwrap();
            let r = whitenoise
                .find_or_create_relay_by_url(&initial_relay)
                .await
                .unwrap();
            saved_user
                .add_relay(&r, RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            let query_relays = vec![crate::whitenoise::relays::Relay {
                id: None,
                url: initial_relay.clone(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }];

            let result = saved_user
                .update_nip65_relays(&whitenoise, &query_relays)
                .await
                .unwrap();

            assert_eq!(result.len(), 1);
            assert_eq!(result[0].url, initial_relay);
        }
    }

    mod update_secondary_relay_types_tests {
        use super::*;
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        #[tokio::test]
        async fn test_update_secondary_relay_types_succeeds_with_no_relays() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let query_relays = vec![];
            let result = saved_user
                .update_secondary_relay_types(&whitenoise, &query_relays)
                .await;

            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn test_update_secondary_relay_types_continues_on_individual_failure() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let query_relays = vec![];
            let result = saved_user
                .update_secondary_relay_types(&whitenoise, &query_relays)
                .await;

            assert!(result.is_ok());
        }

        #[tokio::test]
        async fn test_update_secondary_relay_types_with_valid_query_relays() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
            let query_relays = vec![crate::whitenoise::relays::Relay {
                id: None,
                url: relay_url,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }];

            let result = saved_user
                .update_secondary_relay_types(&whitenoise, &query_relays)
                .await;

            assert!(result.is_ok());
        }
    }

    mod has_valid_encoding_tag_tests {
        use super::*;

        async fn create_key_package_event_with_tags(tags: Vec<Tag>) -> Event {
            let keys = Keys::generate();
            let builder = EventBuilder::new(Kind::MlsKeyPackage, "test-content").tags(tags);
            builder.sign(&keys).await.unwrap()
        }

        #[tokio::test]
        async fn test_valid_encoding_tag() {
            let event = create_key_package_event_with_tags(vec![Tag::custom(
                TagKind::Custom("encoding".into()),
                vec!["base64"],
            )])
            .await;
            assert!(has_valid_encoding_tag(&event));
        }

        #[tokio::test]
        async fn test_no_encoding_tag() {
            let event = create_key_package_event_with_tags(vec![Tag::custom(
                TagKind::Custom("mls_protocol_version".into()),
                vec!["1.0"],
            )])
            .await;
            assert!(!has_valid_encoding_tag(&event));
        }

        #[tokio::test]
        async fn test_wrong_encoding_value() {
            let event = create_key_package_event_with_tags(vec![Tag::custom(
                TagKind::Custom("encoding".into()),
                vec!["hex"],
            )])
            .await;
            assert!(!has_valid_encoding_tag(&event));
        }

        #[tokio::test]
        async fn test_encoding_tag_among_other_tags() {
            let event = create_key_package_event_with_tags(vec![
                Tag::custom(TagKind::Custom("mls_protocol_version".into()), vec!["1.0"]),
                Tag::custom(TagKind::Custom("mls_ciphersuite".into()), vec!["0x0001"]),
                Tag::custom(TagKind::Custom("encoding".into()), vec!["base64"]),
                Tag::custom(
                    TagKind::Custom("relays".into()),
                    vec!["wss://relay.example.com"],
                ),
            ])
            .await;
            assert!(has_valid_encoding_tag(&event));
        }

        #[tokio::test]
        async fn test_empty_tags() {
            let event = create_key_package_event_with_tags(vec![]).await;
            assert!(!has_valid_encoding_tag(&event));
        }
    }

    mod classify_key_package_tests {
        use super::*;

        async fn create_event_with_tags(tags: Vec<Tag>) -> Event {
            let keys = Keys::generate();
            let builder = EventBuilder::new(Kind::MlsKeyPackage, "test-content").tags(tags);
            builder.sign(&keys).await.unwrap()
        }

        #[test]
        fn test_none_returns_not_found() {
            assert_eq!(classify_key_package(None), KeyPackageStatus::NotFound);
        }

        #[tokio::test]
        async fn test_valid_event_returns_valid() {
            let event = create_event_with_tags(vec![Tag::custom(
                TagKind::Custom("encoding".into()),
                vec!["base64"],
            )])
            .await;
            let result = classify_key_package(Some(event));
            assert!(matches!(result, KeyPackageStatus::Valid(_)));
        }

        #[tokio::test]
        async fn test_incompatible_event_returns_incompatible() {
            let event = create_event_with_tags(vec![]).await;
            assert_eq!(
                classify_key_package(Some(event)),
                KeyPackageStatus::Incompatible
            );
        }
    }

    mod sync_relays_for_type_tests {
        use super::*;
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        #[tokio::test]
        async fn test_sync_relays_for_type_no_event_found() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let query_relays = Relay::defaults();
            let changed = saved_user
                .sync_relays_for_type(&whitenoise, RelayType::Nip65, &query_relays)
                .await
                .unwrap();

            assert!(!changed);
        }

        #[tokio::test]
        async fn test_sync_relays_for_type_with_empty_query_relays() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let query_relays = vec![];
            let result = saved_user
                .sync_relays_for_type(&whitenoise, RelayType::Inbox, &query_relays)
                .await;

            assert!(result.is_err());
        }

        #[tokio::test]
        async fn test_sync_relays_for_type_different_relay_types() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let query_relays = Relay::defaults();

            for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
                let changed = saved_user
                    .sync_relays_for_type(&whitenoise, relay_type, &query_relays)
                    .await
                    .unwrap();

                assert!(!changed);
            }
        }
    }
}
