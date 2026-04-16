use std::{sync::Arc, time::Duration as StdDuration};

#[cfg(any(test, feature = "integration-tests"))]
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
#[cfg(any(test, feature = "integration-tests"))]
use dashmap::DashMap;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    perf_instrument,
    relay_control::RelayControlPlane,
    whitenoise::{
        Whitenoise,
        database::Database,
        database::processed_events::ProcessedEvent,
        error::{Result, WhitenoiseError},
        event_tracker::EventTracker,
        relays::{Relay, RelayType},
        user_streaming::{UserUpdate, UserUpdateTrigger},
        utils::timestamp_to_datetime,
    },
};

mod key_package;
mod relay_sync;

pub use key_package::KeyPackageStatus;

#[cfg(test)]
use key_package::classify_key_package;

/// Timeout for a targeted discovery catch-up query.
///
/// This bounds the overall metadata catch-up window. It is separate from the
/// relay-plane connect/query timeouts so user lookup can fail fast even when
/// the discovery plane is willing to wait longer for other operations.
const USER_DISCOVERY_CATCH_UP_TIMEOUT: StdDuration = StdDuration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UserResolutionMode {
    LocalOnly,
    BackgroundIfUnknown,
    BlockingIfUnknown,
}

#[cfg(any(test, feature = "integration-tests"))]
static USER_RESOLUTION_RUN_COUNTS: LazyLock<DashMap<String, usize>> = LazyLock::new(DashMap::new);

#[derive(Clone)]
pub(crate) struct UserRelaySyncContext {
    database: Arc<Database>,
    event_tracker: Arc<dyn EventTracker>,
    relay_control: Arc<RelayControlPlane>,
}

impl UserRelaySyncContext {
    pub(crate) fn new(
        database: Arc<Database>,
        event_tracker: Arc<dyn EventTracker>,
        relay_control: Arc<RelayControlPlane>,
    ) -> Self {
        Self {
            database,
            event_tracker,
            relay_control,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct User {
    pub id: Option<i64>,
    pub pubkey: PublicKey,
    pub metadata: Metadata,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub metadata_known_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

impl User {
    pub fn metadata_is_known(&self) -> bool {
        self.metadata_known_at.is_some()
    }

    pub fn metadata_is_unknown(&self) -> bool {
        !self.metadata_is_known()
    }

    pub fn mark_metadata_known_now(&mut self) {
        self.metadata_known_at = Some(Utc::now());
    }

    pub async fn relays_by_type(
        &self,
        relay_type: RelayType,
        whitenoise: &Whitenoise,
    ) -> Result<Vec<Relay>> {
        self.relays(relay_type, &whitenoise.database).await
    }

    /// Determines whether a metadata event should be accepted by the guarded processing path.
    ///
    /// Metadata state is explicit:
    /// - newly created users accept the first valid metadata event
    /// - users with unknown metadata accept a matching event so the state can become known
    /// - users with known metadata ignore identical events and older processed events
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

        // If user is newly created, always accept the metadata.
        if newly_created {
            tracing::debug!(
                target: "whitenoise::users::should_update_metadata",
                "Accepting metadata event for newly created user {}",
                self.pubkey
            );
            return Ok(true);
        }

        let new_metadata = Metadata::from_json(&event.content)?;

        // Unknown metadata should still accept a matching event so the user can
        // transition from unknown to known, including valid blank `{}` metadata.
        if self.metadata_is_known() && self.metadata == new_metadata {
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
                        "Ignoring older metadata event for user {} (event: {}, stored: {})",
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

impl Whitenoise {
    pub(crate) fn user_relay_sync_context(&self) -> UserRelaySyncContext {
        UserRelaySyncContext::new(
            Arc::clone(&self.database),
            Arc::clone(&self.event_tracker),
            Arc::clone(&self.relay_control),
        )
    }

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

    #[perf_instrument("users")]
    pub(crate) async fn sync_user_relay_type_from_query_relays(
        &self,
        pubkey: &PublicKey,
        relay_type: RelayType,
        query_relay_urls: &[RelayUrl],
    ) -> Result<Vec<Relay>> {
        self.user_relay_sync_context()
            .sync_relay_type_for_pubkey(*pubkey, relay_type, query_relay_urls)
            .await
    }

    fn targeted_discovery_filter(pubkey: PublicKey) -> Filter {
        Filter::new().author(pubkey).kinds([
            Kind::Metadata,
            Kind::RelayList,
            Kind::InboxRelays,
            Kind::MlsKeyPackageRelays,
        ])
    }

    fn prepare_targeted_discovery_events(events: Events) -> Vec<Event> {
        events
            .into_iter()
            .filter_map(|event| match event.kind {
                Kind::Metadata if Metadata::from_json(&event.content).is_err() => {
                    tracing::debug!(
                        target: "whitenoise::users::targeted_discovery",
                        "Skipping invalid metadata event {} during targeted discovery catch-up",
                        event.id.to_hex()
                    );
                    None
                }
                Kind::Metadata
                | Kind::RelayList
                | Kind::InboxRelays
                | Kind::MlsKeyPackageRelays => Some(event),
                _ => None,
            })
            .collect()
    }

    async fn fetch_targeted_discovery_events(&self, pubkey: PublicKey) -> Result<Vec<Event>> {
        if self.relay_control.discovery().relays().is_empty() {
            tracing::warn!(
                target: "whitenoise::users::targeted_discovery",
                "Skipping targeted discovery catch-up for {} because no discovery relays are configured",
                pubkey
            );
            return Ok(Vec::new());
        }

        let events = self
            .relay_control
            .discovery()
            .fetch_events(
                Self::targeted_discovery_filter(pubkey),
                USER_DISCOVERY_CATCH_UP_TIMEOUT,
            )
            .await?;

        Ok(Self::prepare_targeted_discovery_events(events))
    }

    async fn process_targeted_discovery_events(&self, events: &[Event]) {
        for event in events {
            let result = match event.kind {
                Kind::Metadata => self.handle_metadata(event.clone()).await,
                Kind::RelayList | Kind::InboxRelays | Kind::MlsKeyPackageRelays => {
                    self.handle_relay_list(event.clone()).await
                }
                _ => continue,
            };

            if let Err(error) = result {
                tracing::warn!(
                    target: "whitenoise::users::targeted_discovery",
                    "Failed to process targeted discovery event {} for {}: {}",
                    event.id.to_hex(),
                    event.pubkey,
                    error
                );
            }
        }
    }

    fn emit_user_created_if_needed(&self, user: &User, created: bool) {
        if !created {
            return;
        }

        self.user_stream_manager.emit(
            &user.pubkey,
            UserUpdate {
                trigger: UserUpdateTrigger::UserCreated,
                user: user.clone(),
            },
        );
    }

    fn refresh_discovery_for_new_user_if_needed(&self, created: bool, mode: UserResolutionMode) {
        if !created || mode == UserResolutionMode::LocalOnly {
            return;
        }

        self.discovery_sync_worker.request_rebuild();
    }

    fn user_resolution_guard(&self, pubkey: PublicKey) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.user_resolution_guards
            .entry(pubkey)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    #[perf_instrument("users")]
    async fn resolve_user_with_mode(
        &self,
        pubkey: &PublicKey,
        mode: UserResolutionMode,
    ) -> Result<User> {
        let (user, created) = User::find_or_create_by_pubkey(pubkey, &self.database).await?;
        self.emit_user_created_if_needed(&user, created);

        if user.metadata_is_unknown() {
            match mode {
                UserResolutionMode::LocalOnly => {}
                UserResolutionMode::BackgroundIfUnknown => {
                    self.start_background_user_resolution_if_unknown(user.pubkey);
                }
                UserResolutionMode::BlockingIfUnknown => {
                    self.resolve_unknown_user_blocking_if_needed(user.pubkey)
                        .await?;
                }
            }
        }

        self.refresh_discovery_for_new_user_if_needed(created, mode);

        User::find_by_pubkey(pubkey, &self.database).await
    }

    /// Look up a local user row or create an empty unknown-metadata row.
    ///
    /// This method is strictly local-only: it never performs relay fetches or
    /// other discovery work. When a new row is inserted, subscribers receive a
    /// `UserCreated` update.
    #[perf_instrument("users")]
    pub async fn get_or_create_user_local(&self, pubkey: &PublicKey) -> Result<User> {
        self.resolve_user_with_mode(pubkey, UserResolutionMode::LocalOnly)
            .await
    }

    /// Ensure a local user row exists and kick off discovery only when metadata is unknown.
    ///
    /// Returns the current local snapshot immediately. If the user is still
    /// unknown, targeted discovery resolution is started or reused in the
    /// background.
    #[perf_instrument("users")]
    pub async fn resolve_user(&self, pubkey: &PublicKey) -> Result<User> {
        self.resolve_user_with_mode(pubkey, UserResolutionMode::BackgroundIfUnknown)
            .await
    }

    /// Ensure a local user row exists and resolve unknown metadata before returning.
    ///
    /// Returns immediately for users whose metadata is already known. Unknown
    /// users run one guarded targeted discovery resolution and then return the
    /// freshest local snapshot.
    #[perf_instrument("users")]
    pub async fn resolve_user_blocking(&self, pubkey: &PublicKey) -> Result<User> {
        self.resolve_user_with_mode(pubkey, UserResolutionMode::BlockingIfUnknown)
            .await
    }

    async fn resolve_unknown_user_under_guard(&self, pubkey: PublicKey) -> Result<()> {
        let user = User::find_by_pubkey(&pubkey, &self.database).await?;
        if user.metadata_is_known() {
            tracing::debug!(
                target: "whitenoise::users::resolve_unknown_user_under_guard",
                "User {} metadata is already known, skipping targeted discovery",
                pubkey
            );
            return Ok(());
        }

        #[cfg(any(test, feature = "integration-tests"))]
        {
            let pubkey_hex = pubkey.to_hex();
            USER_RESOLUTION_RUN_COUNTS
                .entry(pubkey_hex)
                .and_modify(|count| *count += 1)
                .or_insert(1);
        }

        tracing::debug!(
            target: "whitenoise::users::resolve_unknown_user_under_guard",
            "User {} metadata is unknown, performing targeted discovery catch-up",
            pubkey
        );

        match self.fetch_targeted_discovery_events(pubkey).await {
            Ok(events) => {
                self.process_targeted_discovery_events(&events).await;
            }
            Err(error) => {
                tracing::warn!(
                    target: "whitenoise::users::resolve_unknown_user_under_guard",
                    "Failed to perform targeted discovery catch-up for user {}: {}",
                    pubkey,
                    error
                );
            }
        }

        Ok(())
    }

    async fn resolve_unknown_user_blocking_if_needed(&self, pubkey: PublicKey) -> Result<()> {
        let resolution_guard = self.user_resolution_guard(pubkey);
        let _resolution_guard = resolution_guard.lock().await;
        self.resolve_unknown_user_under_guard(pubkey).await
    }

    pub(crate) fn start_background_user_resolution_if_unknown(&self, pubkey: PublicKey) {
        let resolution_guard = self.user_resolution_guard(pubkey);
        let Ok(resolution_guard) = resolution_guard.try_lock_owned() else {
            tracing::debug!(
                target: "whitenoise::users::start_background_user_resolution_if_unknown",
                "User {} resolution already in progress, reusing existing background discovery",
                pubkey
            );
            return;
        };

        tracing::debug!(
            target: "whitenoise::users::start_background_user_resolution_if_unknown",
            "Starting background discovery catch-up for user {}",
            pubkey
        );

        tokio::spawn(async move {
            let _resolution_guard = resolution_guard;
            let result = async {
                let whitenoise = Self::get_instance()?;
                whitenoise.resolve_unknown_user_under_guard(pubkey).await
            }
            .await;

            if let Err(error) = result {
                tracing::warn!(
                    target: "whitenoise::users::start_background_user_resolution_if_unknown",
                    "Background discovery catch-up task failed for user {}: {}",
                    pubkey,
                    error
                );
            }
        });
    }

    /// **TEST-ONLY**: Override `metadata_known_at` for a user.
    ///
    /// This is used to simulate legacy rows whose metadata payload exists but
    /// whose known/unknown state has not been backfilled yet.
    #[cfg(feature = "integration-tests")]
    pub async fn set_user_metadata_known_at_for_testing(
        &self,
        pubkey: &PublicKey,
        metadata_known_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        sqlx::query("UPDATE users SET metadata_known_at = ? WHERE pubkey = ?")
            .bind(metadata_known_at.map(|timestamp| timestamp.timestamp_millis()))
            .bind(pubkey.to_hex())
            .execute(&self.database.pool)
            .await
            .map_err(crate::whitenoise::database::DatabaseError::Sqlx)?;

        tracing::debug!(
            target: "whitenoise::users::set_user_metadata_known_at_for_testing",
            "Set metadata_known_at for user {} to {:?} (TEST ONLY)",
            pubkey,
            metadata_known_at
        );

        Ok(())
    }

    #[cfg(any(test, feature = "integration-tests"))]
    pub(crate) fn reset_user_resolution_run_count_for_testing(&self, pubkey: &PublicKey) {
        USER_RESOLUTION_RUN_COUNTS.remove(&pubkey.to_hex());
    }

    #[cfg(any(test, feature = "integration-tests"))]
    pub(crate) fn user_resolution_run_count_for_testing(&self, pubkey: &PublicKey) -> usize {
        USER_RESOLUTION_RUN_COUNTS
            .get(&pubkey.to_hex())
            .map(|count| *count)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::{create_mock_whitenoise, test_get_whitenoise};
    use chrono::{Duration, Utc};
    use std::collections::HashSet;

    async fn publish_metadata_to_discovery(keys: &Keys, metadata: &Metadata) {
        let client = Client::new(keys.clone());

        for relay in Relay::defaults() {
            client.add_relay(relay.url.as_str()).await.unwrap();
        }

        client.connect().await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        client
            .send_event_builder(EventBuilder::metadata(metadata))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        client.disconnect().await;
    }

    async fn watched_user_count(whitenoise: &Whitenoise) -> usize {
        whitenoise
            .get_relay_control_state()
            .await
            .discovery
            .watched_user_count
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
            metadata_known_at: None,
            updated_at: Utc::now(),
        };
        let saved_user = user.save(&whitenoise.database).await.unwrap();
        let initial_relay_url = RelayUrl::parse("ws://localhost:8080").unwrap();
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
            metadata_known_at: None,
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
            metadata_known_at: None,
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
    async fn test_get_query_relays_with_no_stored_relays_uses_discovery_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new(),
            created_at: Utc::now(),
            metadata_known_at: None,
            updated_at: Utc::now(),
        };
        let saved_user = user.save(&whitenoise.database).await.unwrap();
        let query_relays = saved_user.get_query_relays(&whitenoise).await.unwrap();
        let query_urls: std::collections::HashSet<RelayUrl> =
            Relay::urls(&query_relays).into_iter().collect();

        for url in &whitenoise.config.discovery_relays {
            assert!(
                query_urls.contains(url),
                "Fallback query relays should include discovery relay: {}",
                url
            );
        }
    }

    #[tokio::test]
    async fn test_get_query_relays_with_no_stored_relays_excludes_unknown_relay() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        // A relay not configured in the discovery plane must not appear in fallback.
        let extra_url = RelayUrl::parse("wss://extra.relay.test").unwrap();

        let test_pubkey = nostr_sdk::Keys::generate().public_key();
        let user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new(),
            created_at: Utc::now(),
            metadata_known_at: None,
            updated_at: Utc::now(),
        };
        let saved_user = user.save(&whitenoise.database).await.unwrap();
        let query_relays = saved_user.get_query_relays(&whitenoise).await.unwrap();
        let query_urls: Vec<RelayUrl> = Relay::urls(&query_relays);

        assert!(
            !query_urls.contains(&extra_url),
            "Fallback query relays should not include a relay not in the discovery plane"
        );
    }

    #[tokio::test]
    async fn test_get_or_create_user_local_creates_row_without_refreshing_discovery() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        whitenoise.create_identity().await.unwrap();

        let before = watched_user_count(&whitenoise).await;
        let test_pubkey = Keys::generate().public_key();

        let user = whitenoise
            .get_or_create_user_local(&test_pubkey)
            .await
            .unwrap();
        let after = watched_user_count(&whitenoise).await;

        assert_eq!(after, before);
        assert_eq!(user.pubkey, test_pubkey);
        assert!(user.id.is_some());
        assert!(user.metadata_is_unknown());
    }

    #[tokio::test]
    async fn test_get_or_create_user_local_emits_user_created_update() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let test_pubkey = Keys::generate().public_key();
        let mut updates = whitenoise.user_stream_manager.subscribe(&test_pubkey);

        let user = whitenoise
            .get_or_create_user_local(&test_pubkey)
            .await
            .unwrap();
        let update = updates.try_recv().unwrap();

        assert_eq!(update.trigger, UserUpdateTrigger::UserCreated);
        assert_eq!(update.user, user);
    }

    #[tokio::test]
    async fn test_resolve_user_returns_unknown_local_snapshot_immediately() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let test_pubkey = Keys::generate().public_key();

        let user = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            whitenoise.resolve_user(&test_pubkey),
        )
        .await
        .unwrap()
        .unwrap();

        let stored_user = whitenoise.find_user_by_pubkey(&test_pubkey).await.unwrap();
        assert_eq!(user, stored_user);
        assert!(user.metadata_is_unknown());
    }

    #[tokio::test]
    async fn test_resolve_user_refreshes_discovery_only_for_new_users() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        whitenoise.create_identity().await.unwrap();
        // Flush fire-and-forget rebuild (worker handles this in production)
        whitenoise.sync_discovery_subscriptions().await.unwrap();

        let before = watched_user_count(&whitenoise).await;
        let test_pubkey = Keys::generate().public_key();

        whitenoise.resolve_user(&test_pubkey).await.unwrap();
        // Flush fire-and-forget rebuild (worker handles this in production)
        whitenoise.sync_discovery_subscriptions().await.unwrap();
        let after_create = watched_user_count(&whitenoise).await;

        whitenoise.resolve_user(&test_pubkey).await.unwrap();
        // No rebuild needed — user already exists, request_rebuild not called
        let after_second_call = watched_user_count(&whitenoise).await;

        assert_eq!(after_create, before + 1);
        assert_eq!(after_second_call, after_create);
    }

    #[tokio::test]
    async fn test_resolve_user_dedupes_background_resolution_for_same_unknown_user() {
        let whitenoise = test_get_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();
        let metadata = Metadata::new().name("Background Deduped User");
        publish_metadata_to_discovery(&keys, &metadata).await;

        whitenoise.get_or_create_user_local(&pubkey).await.unwrap();

        whitenoise.reset_user_resolution_run_count_for_testing(&pubkey);

        let (first, second) = tokio::join!(
            whitenoise.resolve_user(&pubkey),
            whitenoise.resolve_user(&pubkey),
        );

        first.unwrap();
        second.unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let user = whitenoise.find_user_by_pubkey(&pubkey).await.unwrap();
                if user.metadata_is_known() {
                    break user;
                }

                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();

        assert_eq!(whitenoise.user_resolution_run_count_for_testing(&pubkey), 1);
    }

    #[tokio::test]
    async fn test_resolve_unknown_user_blocking_if_needed_dedupes_concurrent_calls() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let metadata = Metadata::new().name("Deduped User");
        publish_metadata_to_discovery(&keys, &metadata).await;

        whitenoise
            .get_or_create_user_local(&keys.public_key())
            .await
            .unwrap();

        whitenoise.reset_user_resolution_run_count_for_testing(&keys.public_key());

        let (first, second) = tokio::join!(
            whitenoise.resolve_unknown_user_blocking_if_needed(keys.public_key()),
            whitenoise.resolve_unknown_user_blocking_if_needed(keys.public_key()),
        );

        first.unwrap();
        second.unwrap();

        let user = whitenoise
            .find_user_by_pubkey(&keys.public_key())
            .await
            .unwrap();
        assert_eq!(user.metadata.name, metadata.name);
        assert_eq!(
            whitenoise.user_resolution_run_count_for_testing(&keys.public_key()),
            1
        );
    }

    #[tokio::test]
    async fn test_resolve_user_blocking_returns_immediately_for_known_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let test_pubkey = Keys::generate().public_key();
        let mut user = User {
            id: None,
            pubkey: test_pubkey,
            metadata: Metadata::new().name("Known User"),
            created_at: Utc::now(),
            metadata_known_at: None,
            updated_at: Utc::now(),
        };
        user.mark_metadata_known_now();
        let saved_user = user.save(&whitenoise.database).await.unwrap();

        whitenoise.reset_user_resolution_run_count_for_testing(&test_pubkey);

        let returned_user = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            whitenoise.resolve_user_blocking(&test_pubkey),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(returned_user, saved_user);
        assert_eq!(
            whitenoise.user_resolution_run_count_for_testing(&test_pubkey),
            0
        );
    }

    #[tokio::test]
    async fn test_resolve_user_blocking_returns_fresh_snapshot_for_unknown_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let metadata = Metadata::new().name("Blocking User");
        publish_metadata_to_discovery(&keys, &metadata).await;

        whitenoise.reset_user_resolution_run_count_for_testing(&keys.public_key());

        let user = whitenoise
            .resolve_user_blocking(&keys.public_key())
            .await
            .unwrap();

        assert_eq!(user.pubkey, keys.public_key());
        assert_eq!(user.metadata.name, metadata.name);
        assert!(user.metadata_is_known());
        assert_eq!(
            whitenoise.user_resolution_run_count_for_testing(&keys.public_key()),
            1
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
            metadata_known_at: None,
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
            metadata_known_at: None,
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
            metadata_known_at: None,
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
            metadata_known_at: None,
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
            metadata_known_at: None,
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
            metadata_known_at: None,
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
            metadata_known_at: None,
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

    mod key_package_relay_urls_tests {
        use super::*;

        #[tokio::test]
        async fn test_returns_key_package_relays_when_present() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = nostr_sdk::Keys::generate().public_key();
            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                metadata_known_at: None,
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let kp_url = RelayUrl::parse("wss://kp.example.com").unwrap();
            let kp_relay = whitenoise
                .find_or_create_relay_by_url(&kp_url)
                .await
                .unwrap();
            saved_user
                .add_relay(&kp_relay, RelayType::KeyPackage, &whitenoise.database)
                .await
                .unwrap();

            // Also add NIP-65 to prove KP takes priority
            let nip65_url = RelayUrl::parse("wss://nip65.example.com").unwrap();
            let nip65_relay = whitenoise
                .find_or_create_relay_by_url(&nip65_url)
                .await
                .unwrap();
            saved_user
                .add_relay(&nip65_relay, RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            let urls = saved_user
                .key_package_relay_urls(&whitenoise)
                .await
                .unwrap();
            assert_eq!(urls, vec![kp_url]);
        }

        #[tokio::test]
        async fn test_falls_back_to_nip65_when_no_kp_relays() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = nostr_sdk::Keys::generate().public_key();
            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                metadata_known_at: None,
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let nip65_url = RelayUrl::parse("wss://nip65.example.com").unwrap();
            let nip65_relay = whitenoise
                .find_or_create_relay_by_url(&nip65_url)
                .await
                .unwrap();
            saved_user
                .add_relay(&nip65_relay, RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            let urls = saved_user
                .key_package_relay_urls(&whitenoise)
                .await
                .unwrap();
            assert_eq!(urls, vec![nip65_url]);
        }

        #[tokio::test]
        async fn test_falls_back_to_configured_discovery_relays_when_no_user_relays() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = nostr_sdk::Keys::generate().public_key();
            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                metadata_known_at: None,
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let urls = saved_user
                .key_package_relay_urls(&whitenoise)
                .await
                .unwrap();
            let expected = whitenoise.fallback_relay_urls().await;
            assert_eq!(urls, expected);
            assert!(
                !urls.is_empty(),
                "Configured discovery relays should be available as the fallback"
            );
        }
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

        #[test]
        fn test_metadata_known_helpers() {
            let test_pubkey = nostr_sdk::Keys::generate().public_key();
            let mut user = User {
                id: Some(1),
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                metadata_known_at: None,
                updated_at: Utc::now(),
            };

            assert!(user.metadata_is_unknown());
            assert!(!user.metadata_is_known());

            user.mark_metadata_known_now();

            assert!(user.metadata_is_known());
            assert!(!user.metadata_is_unknown());
        }

        #[tokio::test]
        async fn test_should_update_metadata_accepts_matching_blank_event_when_unknown() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let user = create_test_user(&whitenoise).await;
            let keys = Keys::generate();
            let event = EventBuilder::metadata(&Metadata::new())
                .sign(&keys)
                .await
                .unwrap();

            let result = user
                .should_update_metadata(&event, false, &whitenoise.database)
                .await
                .unwrap();

            assert!(result);
        }

        #[tokio::test]
        async fn test_get_or_create_user_local_creates_new_user() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = nostr_sdk::Keys::generate().public_key();

            // Verify user doesn't exist
            assert!(whitenoise.find_user_by_pubkey(&test_pubkey).await.is_err());

            let user = whitenoise
                .get_or_create_user_local(&test_pubkey)
                .await
                .unwrap();

            // TESTS: User creation and database persistence
            assert_eq!(user.pubkey, test_pubkey);
            assert!(user.id.is_some());
            assert_eq!(user.metadata, Metadata::new());
            assert!(user.metadata_is_unknown());
        }

        #[tokio::test]
        async fn test_get_or_create_user_local_returns_existing_user() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = nostr_sdk::Keys::generate().public_key();

            // Create user directly in database
            let mut original_user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new().name("Original User"),
                created_at: Utc::now(),
                metadata_known_at: None,
                updated_at: Utc::now(),
            };
            original_user.mark_metadata_known_now();
            let saved_user = original_user.save(&whitenoise.database).await.unwrap();

            let found_user = whitenoise
                .get_or_create_user_local(&test_pubkey)
                .await
                .unwrap();

            // TESTS: Existing user retrieval
            assert_eq!(found_user.id, saved_user.id);
            assert_eq!(found_user.pubkey, test_pubkey);
            assert_eq!(found_user.metadata.name, Some("Original User".to_string()));
            assert!(found_user.metadata_is_known());
        }

        #[tokio::test]
        async fn test_resolve_user_returns_local_snapshot_for_existing_user() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = nostr_sdk::Keys::generate().public_key();
            let user = whitenoise
                .get_or_create_user_local(&test_pubkey)
                .await
                .unwrap();

            let resolved_user = whitenoise.resolve_user(&test_pubkey).await.unwrap();

            assert_eq!(resolved_user, user);
        }

        #[tokio::test]
        async fn test_unknown_metadata_remains_unknown_when_discovery_finds_nothing() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = nostr_sdk::Keys::generate().public_key();

            whitenoise
                .resolve_user_blocking(&test_pubkey)
                .await
                .unwrap();

            let user_from_db = User::find_by_pubkey(&test_pubkey, &whitenoise.database)
                .await
                .unwrap();
            assert!(user_from_db.metadata_is_unknown());
            assert_eq!(user_from_db.metadata, Metadata::new());
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let initial_relay = RelayUrl::parse("ws://localhost:8080").unwrap();
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let relay_url = RelayUrl::parse("ws://localhost:7777").unwrap();
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

    mod classify_key_package_tests {
        use super::*;

        async fn create_event_with_tags(tags: Vec<Tag>) -> Event {
            let keys = Keys::generate();
            let builder = EventBuilder::new(Kind::MlsKeyPackage, "dGVzdF9jb250ZW50").tags(tags);
            builder.sign(&keys).await.unwrap()
        }

        #[test]
        fn test_none_returns_not_found() {
            assert_eq!(classify_key_package(None), KeyPackageStatus::NotFound);
        }

        #[tokio::test]
        async fn test_valid_event_returns_valid() {
            let event = create_event_with_tags(vec![
                Tag::custom(TagKind::Custom("mls_ciphersuite".into()), vec!["0x0001"]),
                Tag::custom(
                    TagKind::Custom("mls_extensions".into()),
                    vec!["0x000a", "0xf2ee"],
                ),
                Tag::custom(TagKind::Custom("mls_proposals".into()), vec!["0x000a"]),
                Tag::custom(TagKind::Custom("encoding".into()), vec!["base64"]),
            ])
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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
                metadata_known_at: None,
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

        #[tokio::test]
        async fn test_sync_user_relay_type_from_query_relays_returns_existing_relays_when_query_list_is_empty()
         {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();
            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                metadata_known_at: None,
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();
            let relay_url = RelayUrl::parse("wss://inbox.example.com").unwrap();
            let relay = whitenoise
                .find_or_create_relay_by_url(&relay_url)
                .await
                .unwrap();

            saved_user
                .add_relay(&relay, RelayType::Inbox, &whitenoise.database)
                .await
                .unwrap();

            let relays = whitenoise
                .sync_user_relay_type_from_query_relays(&test_pubkey, RelayType::Inbox, &[])
                .await
                .unwrap();

            assert_eq!(Relay::urls(&relays), vec![relay_url]);
        }
    }

    mod key_package_status_tests {
        use super::*;
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        #[tokio::test]
        async fn test_not_found_with_empty_relay_list_retries_after_sync() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                metadata_known_at: None,
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            // User has no relays in DB, so key_package_status should attempt
            // relay sync and retry. With no real relays to reach, the result
            // should still be NotFound but the retry path is exercised.
            let status = saved_user.key_package_status(&whitenoise).await.unwrap();
            assert_eq!(status, KeyPackageStatus::NotFound);
        }

        #[tokio::test]
        async fn test_not_found_with_populated_relay_list_does_not_retry() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let test_pubkey = Keys::generate().public_key();

            let user = User {
                id: None,
                pubkey: test_pubkey,
                metadata: Metadata::new(),
                created_at: Utc::now(),
                metadata_known_at: None,
                updated_at: Utc::now(),
            };
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            // Add a key package relay using a local test relay so the connection succeeds
            let relay_url = RelayUrl::parse("ws://localhost:8080").unwrap();
            let relay = whitenoise
                .find_or_create_relay_by_url(&relay_url)
                .await
                .unwrap();
            saved_user
                .add_relay(&relay, RelayType::KeyPackage, &whitenoise.database)
                .await
                .unwrap();

            // With a relay present, key_package_status should return NotFound
            // without attempting relay sync (no retry path).
            let status = saved_user.key_package_status(&whitenoise).await.unwrap();
            assert_eq!(status, KeyPackageStatus::NotFound);
        }
    }
}
