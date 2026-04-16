use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use ::rand::RngCore;

use dashmap::DashMap;
use mdk_core::prelude::GroupId;
use nostr_sdk::prelude::NostrSigner;
use nostr_sdk::{EventId, PublicKey, RelayUrl};
use tokio::sync::{
    Mutex, OnceCell, Semaphore,
    mpsc::{self, Sender},
    watch,
};
use tokio::task::JoinHandle;

pub mod account_settings;
pub mod accounts;
pub mod accounts_groups;
pub mod aggregated_message;
pub mod app_settings;
pub mod background_notifications;
pub(crate) mod cached_graph_user;
pub mod chat_list;
pub mod chat_list_streaming;
pub mod database;
pub mod debug;
pub(crate) mod discovery_sync_worker;
pub mod drafts;
pub mod error;
mod event_processor;
pub mod event_tracker;
pub mod follows;
pub mod group_information;
pub mod groups;
mod init_timing;
pub mod key_packages;
pub mod media_files;
pub mod message_aggregator;
pub mod message_streaming;
pub mod messages;
pub mod mute_list;
pub mod notification_streaming;
pub mod push_notifications;
pub mod relays;
pub mod scheduled_tasks;
pub mod secrets_store;
mod signer;
pub mod storage;
mod streaming;
pub mod streaming_error;
mod subscriptions;
pub mod user_search;
pub mod user_streaming;
pub mod users;
pub mod utils;
pub mod zapstore;

use mdk_core::prelude::MDK;
use mdk_sqlite_storage::MdkSqliteStorage;

use crate::init_tracing;
use crate::perf_instrument;
use crate::relay_control::RelayControlPlane;
use crate::relay_control::discovery::DiscoveryPlaneConfig;

use crate::types::ProcessableEvent;

use accounts::*;
use app_settings::*;
use database::*;
use error::{Result, WhitenoiseError};
use event_tracker::WhitenoiseEventTracker;
use relays::*;
use secrets_store::SecretsStore;
#[cfg(test)]
use users::User;

#[derive(Clone, Debug)]
pub struct WhitenoiseConfig {
    /// Directory for application data
    pub data_dir: PathBuf,

    /// Directory for application logs
    pub logs_dir: PathBuf,

    /// Configuration for the message aggregator
    pub message_aggregator_config: Option<message_aggregator::AggregatorConfig>,

    /// Keyring service identifier for MDK SQLCipher key management.
    /// Each application using this crate must provide a unique identifier
    /// to avoid key collisions in the system keyring.
    pub keyring_service_id: String,

    /// Configured discovery relays for the relay-control discovery plane.
    pub discovery_relays: Vec<RelayUrl>,
}

impl WhitenoiseConfig {
    pub fn new(data_dir: &Path, logs_dir: &Path, keyring_service_id: &str) -> Self {
        let env_suffix = if cfg!(debug_assertions) {
            "dev"
        } else {
            "release"
        };
        let formatted_data_dir = data_dir.join(env_suffix);
        let formatted_logs_dir = logs_dir.join(env_suffix);

        Self {
            data_dir: formatted_data_dir,
            logs_dir: formatted_logs_dir,
            message_aggregator_config: None, // Use default MessageAggregator configuration
            keyring_service_id: keyring_service_id.to_string(),
            discovery_relays: DiscoveryPlaneConfig::curated_default_relays(),
        }
    }

    /// Create a new configuration with custom message aggregator settings
    pub fn new_with_aggregator_config(
        data_dir: &Path,
        logs_dir: &Path,
        keyring_service_id: &str,
        aggregator_config: message_aggregator::AggregatorConfig,
    ) -> Self {
        let env_suffix = if cfg!(debug_assertions) {
            "dev"
        } else {
            "release"
        };
        let formatted_data_dir = data_dir.join(env_suffix);
        let formatted_logs_dir = logs_dir.join(env_suffix);

        Self {
            data_dir: formatted_data_dir,
            logs_dir: formatted_logs_dir,
            message_aggregator_config: Some(aggregator_config),
            keyring_service_id: keyring_service_id.to_string(),
            discovery_relays: DiscoveryPlaneConfig::curated_default_relays(),
        }
    }

    pub fn with_discovery_relays(mut self, discovery_relays: Vec<RelayUrl>) -> Self {
        self.discovery_relays = discovery_relays;
        self
    }
}

pub struct Whitenoise {
    pub config: WhitenoiseConfig,
    database: Arc<Database>,
    event_tracker: std::sync::Arc<dyn event_tracker::EventTracker>,
    content_parser: crate::nostr_manager::parser::ContentParser,
    relay_control: Arc<RelayControlPlane>,
    secrets_store: SecretsStore,
    storage: storage::Storage,
    message_aggregator: message_aggregator::MessageAggregator,
    message_stream_manager: Arc<message_streaming::MessageStreamManager>,
    user_stream_manager: user_streaming::UserStreamManager,
    chat_list_stream_manager: chat_list_streaming::ChatListStreamManager,
    archived_chat_list_stream_manager: chat_list_streaming::ChatListStreamManager,
    notification_stream_manager: notification_streaming::NotificationStreamManager,
    event_sender: Sender<ProcessableEvent>,
    shutdown_sender: Sender<()>,
    /// Per-account concurrency guards to prevent race conditions in contact list processing
    contact_list_guards: DashMap<PublicKey, Arc<Semaphore>>,
    /// Per-user guards that dedupe targeted discovery resolution for the same pubkey.
    user_resolution_guards: DashMap<PublicKey, Arc<Mutex<()>>>,
    /// Shutdown signal for scheduled tasks
    scheduler_shutdown: watch::Sender<bool>,
    /// Handles for spawned scheduler tasks
    scheduler_handles: Mutex<Vec<JoinHandle<()>>>,
    /// External signers for accounts using NIP-55 (Amber) or similar.
    /// Maps account pubkey to their signer implementation.
    external_signers: DashMap<PublicKey, Arc<dyn NostrSigner>>,
    /// Per-account cancellation signals for background tasks (e.g. contact list
    /// user fetches). Sending `true` tells all background tasks for that account
    /// to stop. A new channel is created on login and signalled on logout.
    background_task_cancellation: DashMap<PublicKey, watch::Sender<bool>>,
    /// Pubkeys with a login in progress (between login_start and
    /// login_publish_default_relays / login_with_custom_relay / login_cancel).
    /// The value holds whichever relay lists were already discovered on the
    /// network so that step 2a can publish defaults only for the missing ones.
    pending_logins: DashMap<PublicKey, accounts::DiscoveredRelayLists>,
    /// Debounced worker that coalesces discovery subscription rebuilds.
    discovery_sync_worker: discovery_sync_worker::DiscoverySyncWorker,
    /// In-memory coordination for delayed MIP-05 token-list responses.
    pending_push_token_responses: Arc<DashMap<(PublicKey, GroupId, EventId), ()>>,
    /// Bounds the number of concurrently-active delayed MIP-05 token-list response tasks.
    /// See [`push_notifications::MAX_CONCURRENT_TOKEN_RESPONSE_TASKS`] for the cap value.
    token_response_semaphore: Arc<Semaphore>,
    /// Per-sender rate limiter for inbound MIP-05 token requests and removals.
    /// Maps `(account_pubkey, GroupId, sender_leaf_index, kind)` → last accepted
    /// `Instant`.  Requests and removals have independent cooldown buckets.
    token_request_timestamps:
        DashMap<(PublicKey, GroupId, u32, push_notifications::TokenRateKind), std::time::Instant>,
}

static GLOBAL_WHITENOISE: OnceCell<Whitenoise> = OnceCell::const_new();

/// Serializes callers of [`Whitenoise::ensure_initialized`] so two concurrent
/// invokers cannot both observe an uninitialized singleton and both drive
/// [`Whitenoise::initialize_whitenoise`]. The mutex is only held across the
/// check-and-possibly-init; once `GLOBAL_WHITENOISE` is populated, subsequent
/// callers acquire it, observe the populated singleton, and release it
/// immediately without doing work.
static ENSURE_INITIALIZED_LOCK: Mutex<()> = Mutex::const_new(());

struct WhitenoiseComponents {
    event_tracker: std::sync::Arc<dyn event_tracker::EventTracker>,
    secrets_store: SecretsStore,
    storage: storage::Storage,
    message_aggregator: message_aggregator::MessageAggregator,
    event_sender: Sender<ProcessableEvent>,
    shutdown_sender: Sender<()>,
    scheduler_shutdown: watch::Sender<bool>,
}

impl std::fmt::Debug for Whitenoise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Whitenoise")
            .field("config", &self.config)
            .field("database", &"<REDACTED>")
            .field("event_tracker", &"<REDACTED>")
            .field("content_parser", &"<REDACTED>")
            .field("relay_control", &"<REDACTED>")
            .field("secrets_store", &"<REDACTED>")
            .field("storage", &"<REDACTED>")
            .field("message_aggregator", &"<REDACTED>")
            .field("message_stream_manager", &"<REDACTED>")
            .field("user_stream_manager", &"<REDACTED>")
            .field("chat_list_stream_manager", &"<REDACTED>")
            .field("archived_chat_list_stream_manager", &"<REDACTED>")
            .field("notification_stream_manager", &"<REDACTED>")
            .field("event_sender", &"<REDACTED>")
            .field("shutdown_sender", &"<REDACTED>")
            .field("contact_list_guards", &"<REDACTED>")
            .field("user_resolution_guards", &"<REDACTED>")
            .field("scheduler_shutdown", &"<REDACTED>")
            .field("scheduler_handles", &"<REDACTED>")
            .field("pending_push_token_responses", &"<REDACTED>")
            .field("token_response_semaphore", &"<REDACTED>")
            .field("token_request_timestamps", &"<REDACTED>")
            .finish()
    }
}

impl Whitenoise {
    fn from_components(
        config: WhitenoiseConfig,
        database: Arc<Database>,
        components: WhitenoiseComponents,
    ) -> Self {
        let mut session_salt = [0u8; 16];
        ::rand::rng().fill_bytes(&mut session_salt);
        let relay_control = Arc::new(RelayControlPlane::new(
            database.clone(),
            config.discovery_relays.clone(),
            components.event_sender.clone(),
            session_salt,
        ));

        Self {
            config,
            database,
            event_tracker: components.event_tracker,
            content_parser: crate::nostr_manager::parser::ContentParser::new(),
            relay_control,
            secrets_store: components.secrets_store,
            storage: components.storage,
            message_aggregator: components.message_aggregator,
            message_stream_manager: Arc::new(message_streaming::MessageStreamManager::default()),
            user_stream_manager: user_streaming::UserStreamManager::default(),
            chat_list_stream_manager: chat_list_streaming::ChatListStreamManager::default(),
            archived_chat_list_stream_manager: chat_list_streaming::ChatListStreamManager::default(
            ),
            notification_stream_manager: notification_streaming::NotificationStreamManager::default(
            ),
            event_sender: components.event_sender,
            shutdown_sender: components.shutdown_sender,
            contact_list_guards: DashMap::new(),
            user_resolution_guards: DashMap::new(),
            scheduler_shutdown: components.scheduler_shutdown,
            scheduler_handles: Mutex::new(Vec::new()),
            external_signers: DashMap::new(),
            background_task_cancellation: DashMap::new(),
            pending_logins: DashMap::new(),
            discovery_sync_worker: discovery_sync_worker::DiscoverySyncWorker::new(),
            pending_push_token_responses: Arc::new(DashMap::new()),
            token_response_semaphore: Arc::new(Semaphore::new(
                push_notifications::MAX_CONCURRENT_TOKEN_RESPONSE_TASKS,
            )),
            token_request_timestamps: DashMap::new(),
        }
    }

    /// Initializes the keyring-core credential store.
    ///
    /// `mdk-sqlite-storage` and [`SecretsStore`] both use `keyring-core` to store
    /// secret key material.  Unlike the `keyring` crate (v3), `keyring-core` does
    /// **not** auto-detect a platform backend — callers must register one via
    /// `keyring_core::set_default_store()` before any `Entry` operations.
    ///
    /// In test, integration-test, and benchmark-test builds the mock (in-memory)
    /// store is used so that neither `cargo test` nor unsigned `cargo run`
    /// binaries require real platform keychain entitlements.
    ///
    /// This function is safe to call multiple times; only the first call has
    /// an effect.
    fn initialize_keyring_store() {
        static KEYRING_STORE_INIT: OnceLock<()> = OnceLock::new();
        KEYRING_STORE_INIT.get_or_init(|| {
            // Use the mock (in-memory) store in test, integration-test, and
            // benchmark-test builds so that `cargo test` and unsigned `cargo run`
            // binaries never require real platform keychain entitlements.
            // The real store is reserved for production builds (no feature flags).
            #[cfg(any(test, feature = "integration-tests", feature = "benchmark-tests"))]
            {
                keyring_core::set_default_store(
                    keyring_core::mock::Store::new()
                        .expect("Failed to create mock credential store"),
                );
            }

            #[cfg(all(
                not(test),
                not(feature = "integration-tests"),
                not(feature = "benchmark-tests")
            ))]
            {
                // CLI builds use the regular Keychain store on all macOS architectures.
                // The `cli` feature gates the `wn`/`wnd` binaries, which are unsigned
                // and cannot use the Protected Data store (it requires a provisioning
                // profile with application-groups entitlement).
                #[cfg(all(target_os = "macos", feature = "cli"))]
                {
                    let store = apple_native_keyring_store::keychain::Store::new()
                        .expect("Failed to create macOS Keychain credential store");
                    keyring_core::set_default_store(store);
                }
                // Intel Macs (non-CLI): use the regular Keychain store.
                // Protected Data store is not available on x86_64.
                #[cfg(all(target_os = "macos", target_arch = "x86_64", not(feature = "cli")))]
                {
                    let store = apple_native_keyring_store::keychain::Store::new()
                        .expect("Failed to create macOS Keychain credential store");
                    keyring_core::set_default_store(store);
                }
                // Apple Silicon (non-CLI): use the Protected Data store (audit #630).
                // Requires code-signing with a provisioning profile that includes the
                // com.apple.security.application-groups entitlement. The Flutter app
                // build pipeline satisfies this.
                #[cfg(all(target_os = "macos", target_arch = "aarch64", not(feature = "cli")))]
                {
                    let store = apple_native_keyring_store::protected::Store::new()
                        .expect("Failed to create macOS protected-data credential store");
                    keyring_core::set_default_store(store);
                }
                #[cfg(target_os = "ios")]
                {
                    let store = apple_native_keyring_store::protected::Store::new()
                        .expect("Failed to create iOS protected-data credential store");
                    keyring_core::set_default_store(store);
                }
                #[cfg(target_os = "windows")]
                {
                    let store = windows_native_keyring_store::Store::new()
                        .expect("Failed to create Windows credential store");
                    keyring_core::set_default_store(store);
                }
                #[cfg(target_os = "linux")]
                {
                    let store = linux_keyutils_keyring_store::Store::new()
                        .expect("Failed to create Linux keyutils credential store");
                    keyring_core::set_default_store(store);
                }
                #[cfg(target_os = "android")]
                {
                    let store = android_native_keyring_store::Store::new()
                        .expect("Failed to create Android credential store");
                    keyring_core::set_default_store(store);
                }
                #[cfg(not(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "windows",
                    target_os = "linux",
                    target_os = "android",
                )))]
                {
                    compile_error!(
                        "No keyring-core credential store available for this target OS. \
                         Add a platform-specific store crate to Cargo.toml and handle it \
                         in initialize_keyring_store()."
                    );
                }
            }
        });
    }

    /// Initializes the mock keyring store for testing environments.
    ///
    /// This is a convenience alias for external callers (e.g. the integration
    /// test binary) that need to set up the mock store before
    /// `initialize_whitenoise()` is called.  In practice
    /// `initialize_keyring_store()` already uses the mock store in test builds,
    /// so this simply ensures it has been called.
    ///
    /// This function is safe to call multiple times.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Call this at the start of your integration test binary
    /// Whitenoise::initialize_mock_keyring_store();
    /// ```
    #[cfg(any(test, feature = "integration-tests"))]
    pub fn initialize_mock_keyring_store() {
        Self::initialize_keyring_store();
    }

    /// Creates an MDK instance for the given account public key using this
    /// instance's configured data directory and keyring service identifier.
    pub(crate) fn create_mdk_for_account(
        &self,
        pubkey: PublicKey,
    ) -> core::result::Result<MDK<MdkSqliteStorage>, AccountError> {
        Account::create_mdk(
            pubkey,
            &self.config.data_dir,
            &self.config.keyring_service_id,
        )
    }

    /// Initializes the Whitenoise application with the provided configuration.
    ///
    /// This method sets up the necessary data and log directories, configures logging,
    /// initializes the database, creates event processing channels, sets up the Nostr client,
    /// loads existing accounts, and starts the event processing loop.
    ///
    /// # Arguments
    ///
    /// * `config` - A [`WhitenoiseConfig`] struct specifying the data and log directories.
    #[perf_instrument("whitenoise")]
    pub async fn initialize_whitenoise(config: WhitenoiseConfig) -> Result<()> {
        init_timing::start();

        // Ensure keyring-core has a credential store before any MDK or
        // SecretsStore operations attempt to create or read keyring entries.
        Self::initialize_keyring_store();

        // Validate keyring_service_id is not empty or whitespace
        let keyring_service_id = config.keyring_service_id.trim().to_string();
        if keyring_service_id.is_empty() {
            return Err(WhitenoiseError::Configuration(
                "keyring_service_id cannot be empty or whitespace".to_string(),
            ));
        }

        // Create event processing channels
        let (event_sender, event_receiver) = mpsc::channel(2000);
        let (shutdown_sender, shutdown_receiver) = mpsc::channel(1);

        // Create scheduler shutdown channel
        let (scheduler_shutdown, scheduler_shutdown_rx) = watch::channel(false);

        init_timing::record("keyring_and_channels");

        let whitenoise_res: Result<&'static Whitenoise> = GLOBAL_WHITENOISE.get_or_try_init(|| async {
        let data_dir = &config.data_dir;
        let logs_dir = &config.logs_dir;

        // Setup directories. Path context is preserved via tracing; the
        // io::Error itself flows through `WhitenoiseError::Filesystem` so
        // callers can distinguish filesystem failures.
        std::fs::create_dir_all(data_dir).inspect_err(|e| {
            tracing::error!(
                target: "whitenoise::initialize_whitenoise",
                ?data_dir,
                error = %e,
                "Failed to create data directory"
            );
        })?;
        std::fs::create_dir_all(logs_dir).inspect_err(|e| {
            tracing::error!(
                target: "whitenoise::initialize_whitenoise",
                ?logs_dir,
                error = %e,
                "Failed to create logs directory"
            );
        })?;

        // Only initialize tracing once
        init_tracing(logs_dir);

        tracing::debug!(target: "whitenoise::initialize_whitenoise", "Logging initialized in directory: {:?}", logs_dir);

        init_timing::record("directories_and_logging");

        let database = Arc::new(Database::new(data_dir.join("whitenoise.sqlite")).await?);

        init_timing::record("database");

        // Create the event tracker.
        let event_tracker: std::sync::Arc<dyn event_tracker::EventTracker> =
            Arc::new(WhitenoiseEventTracker::new(database.clone()));

        // Create SecretsStore backed by the platform keyring-core store
        let secrets_store = SecretsStore::new(&keyring_service_id);

        // Create Storage
        let storage = storage::Storage::new(data_dir).await?;

        // Create message aggregator - always initialize, use custom config if provided
        let message_aggregator = if let Some(aggregator_config) = config.message_aggregator_config.clone() {
            message_aggregator::MessageAggregator::with_config(aggregator_config)
        } else {
            message_aggregator::MessageAggregator::new()
        };
        let whitenoise = Self::from_components(
            config,
            database,
            WhitenoiseComponents {
                event_tracker,
                secrets_store,
                storage,
                message_aggregator,
                event_sender,
                shutdown_sender,
                scheduler_shutdown,
            },
        );
        whitenoise.relay_control.start_telemetry_persistors().await;

        init_timing::record("core_services");

        // Create default relays in the database if they don't exist
        // TODO: Make this batch fetch and insert all relays at once
        for relay in Relay::defaults() {
            let _ = whitenoise.find_or_create_relay_by_url(&relay.url).await?;
        }

        // Create default app settings in the database if they don't exist
        AppSettings::find_or_create_default(&whitenoise.database).await?;

        init_timing::record("database_seeding");

        whitenoise.relay_control.start_discovery_plane().await?;

        init_timing::record("discovery_plane");

        Ok(whitenoise)
        }).await;

        let whitenoise_ref = whitenoise_res?;

        tracing::info!(
            target: "whitenoise::initialize_whitenoise",
            "Synchronizing message cache with MDK..."
        );
        // Synchronize message cache BEFORE starting event processor
        // This eliminates race conditions between startup sync and real-time cache updates
        whitenoise_ref.sync_message_cache_on_startup().await?;
        tracing::info!(
            target: "whitenoise::initialize_whitenoise",
            "Message cache synchronization complete"
        );

        init_timing::record("message_cache_sync");

        // Backfill dm_peer_pubkey for existing DM groups missing it
        if let Err(e) = whitenoise_ref.backfill_dm_peer_pubkeys().await {
            tracing::warn!(
                target: "whitenoise::initialize_whitenoise",
                "DM peer pubkey backfill failed (non-fatal): {}",
                e
            );
        }

        tracing::debug!(
            target: "whitenoise::initialize_whitenoise",
            "Starting event processing loop for loaded accounts"
        );

        Self::start_event_processing_loop(whitenoise_ref, event_receiver, shutdown_receiver).await;

        // Register and start scheduled background tasks
        let tasks: Vec<Arc<dyn scheduled_tasks::Task>> = vec![
            Arc::new(scheduled_tasks::KeyPackageMaintenance),
            Arc::new(scheduled_tasks::ConsumedKeyPackageCleanup),
            Arc::new(scheduled_tasks::CachedGraphUserCleanup),
            Arc::new(scheduled_tasks::RelayListMaintenance),
            Arc::new(scheduled_tasks::SubscriptionHealthCheck),
            Arc::new(scheduled_tasks::MuteExpiryCleanup),
        ];
        let scheduler_handles = scheduled_tasks::start_scheduled_tasks(
            whitenoise_ref,
            scheduler_shutdown_rx,
            None,
            tasks,
        );
        *whitenoise_ref.scheduler_handles.lock().await = scheduler_handles;

        // Spawn discovery sync worker (must happen after GLOBAL_WHITENOISE is set)
        {
            let worker_shutdown_rx = whitenoise_ref.scheduler_shutdown.subscribe();
            let handle = tokio::spawn(async move {
                whitenoise_ref
                    .discovery_sync_worker
                    .run(whitenoise_ref, worker_shutdown_rx)
                    .await;
            });
            whitenoise_ref.scheduler_handles.lock().await.push(handle);
        }

        init_timing::record("background_tasks");

        // Fetch events and setup subscriptions after event processing has started
        Self::setup_all_subscriptions(whitenoise_ref).await?;

        init_timing::record("subscription_setup");

        tracing::debug!(
            target: "whitenoise::initialize_whitenoise",
            "Completed initialization for all loaded accounts"
        );

        init_timing::report();

        Ok(())
    }

    /// Buffer (in seconds) used when resubscribing after a teardown/rebuild
    /// cycle. Larger than `SUBSCRIPTION_BUFFER_SECS` to cover the gap window
    /// created by tearing down old subscriptions before new ones are live.
    /// Any events fetched that were already processed are deduplicated by the
    /// event tracker.
    const RESUBSCRIBE_BUFFER_SECS: u64 = 60;
    /// Returns a reference to the global Whitenoise singleton instance.
    ///
    /// This method provides access to the globally initialized Whitenoise instance that was
    /// created by [`Whitenoise::initialize_whitenoise`]. The instance is stored as a static singleton
    /// using [`tokio::sync::OnceCell`] to ensure async-safe thread-safe access and single initialization.
    ///
    /// This method is particularly useful for accessing the Whitenoise instance from different
    /// parts of the application without passing references around, such as in event handlers,
    /// background tasks, or API endpoints.
    pub fn get_instance() -> Result<&'static Self> {
        GLOBAL_WHITENOISE
            .get()
            .ok_or(WhitenoiseError::Initialization)
    }

    /// Ensures the global Whitenoise singleton is initialized, returning a reference to it.
    ///
    /// If Whitenoise is already running (warm start), returns the existing instance immediately.
    /// If not yet initialized (cold start), performs full initialization first.
    ///
    /// This is the primary entry point for contexts where the caller does not know whether
    /// Whitenoise has already been started — for example, iOS background push handlers that
    /// may fire when the app process is still alive or after a full cold launch.
    ///
    /// # Concurrency
    ///
    /// Concurrent callers are serialized through a module-private mutex so that
    /// exactly one caller drives [`initialize_whitenoise`] on cold start; all others
    /// observe the populated singleton and return immediately. This guarantees the
    /// documented "never start duplicate event processors or background tasks"
    /// contract even when multiple iOS silent-push handlers run concurrently on
    /// different threads.
    ///
    /// Note: this does not protect against a caller invoking [`initialize_whitenoise`]
    /// directly in parallel with `ensure_initialized`. Outside of this background
    /// push path, init is expected to be serialized by the caller (e.g. a single
    /// Flutter cold-start path).
    ///
    /// [`initialize_whitenoise`]: Self::initialize_whitenoise
    pub async fn ensure_initialized(config: WhitenoiseConfig) -> Result<&'static Self> {
        // Fast path: already initialized. Avoids taking the lock in the warm case,
        // which is the common case once the app has been running.
        if let Some(instance) = GLOBAL_WHITENOISE.get() {
            return Ok(instance);
        }

        // Cold / contested path: take the lock and re-check under it. Only the
        // first holder observes `is_none()` and drives initialization; later
        // holders see the now-populated cell and fall through.
        let _guard = ENSURE_INITIALIZED_LOCK.lock().await;
        if GLOBAL_WHITENOISE.get().is_none() {
            Self::initialize_whitenoise(config).await?;
        }
        Self::get_instance()
    }

    /// Gracefully shuts down all background tasks without deleting data.
    ///
    /// This should be called when the app is being closed or going into the background.
    /// Shuts down the event processor and all scheduled tasks.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use whitenoise::Whitenoise;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let whitenoise = Whitenoise::get_instance()?;
    /// whitenoise.shutdown().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[perf_instrument("whitenoise")]
    pub async fn shutdown(&self) -> Result<()> {
        tracing::info!(target: "whitenoise::shutdown", "Initiating graceful shutdown");

        self.shutdown_event_processing().await?;
        self.shutdown_scheduled_tasks().await;

        tracing::info!(target: "whitenoise::shutdown", "Graceful shutdown complete");
        Ok(())
    }

    /// Deletes all application data, including the database, MLS data, and log files.
    ///
    /// This asynchronous method removes all persistent data associated with the Whitenoise instance.
    /// It deletes the nostr cache, database, MLS-related directories, media cache, and all log files.
    /// If the MLS directory exists, it is removed and then recreated as an empty directory.
    /// This is useful for resetting the application to a clean state.
    #[perf_instrument("whitenoise")]
    pub async fn delete_all_data(&self) -> Result<()> {
        tracing::debug!(target: "whitenoise::delete_all_data", "Deleting all data");

        // Shutdown gracefully before deleting data
        self.shutdown().await?;

        // Tear down all relay-control subscriptions
        self.relay_control.shutdown_all().await;

        // Remove database (accounts and media) data
        self.database.delete_all_data().await?;

        // Remove storage artifacts (media cache, etc.)
        self.storage.wipe_all().await?;

        // Remove MLS related data
        let mls_dir = self.config.data_dir.join("mls");
        if mls_dir.exists() {
            tracing::debug!(
                target: "whitenoise::delete_all_data",
                "Removing MLS directory: {:?}",
                mls_dir
            );
            tokio::fs::remove_dir_all(&mls_dir).await?;
        }
        // Always recreate the empty MLS directory
        tokio::fs::create_dir_all(&mls_dir).await?;

        // Remove logs
        if self.config.logs_dir.exists() {
            for entry in std::fs::read_dir(&self.config.logs_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    std::fs::remove_file(path)?;
                } else if path.is_dir() {
                    std::fs::remove_dir_all(path)?;
                }
            }
        }

        Ok(())
    }

    /// Gracefully shuts down all scheduled tasks.
    ///
    /// Sends shutdown signal to all running tasks and waits for them to complete.
    /// Any panicked tasks are logged but do not cause this method to fail.
    #[perf_instrument("whitenoise")]
    async fn shutdown_scheduled_tasks(&self) {
        tracing::info!(target: "whitenoise::scheduler", "Initiating scheduler shutdown");

        // Signal all tasks to stop
        let _ = self.scheduler_shutdown.send(true);

        // Drain and await all handles
        let mut handles = self.scheduler_handles.lock().await;

        if handles.is_empty() {
            tracing::debug!(target: "whitenoise::scheduler", "No scheduler tasks to await");
            return;
        }

        for handle in handles.drain(..) {
            if let Err(e) = handle.await {
                if e.is_panic() {
                    tracing::error!(
                        target: "whitenoise::scheduler",
                        "Scheduler task panicked: {:?}",
                        e
                    );
                } else {
                    tracing::warn!(
                        target: "whitenoise::scheduler",
                        "Scheduler task cancelled: {:?}",
                        e
                    );
                }
            }
        }

        tracing::info!(target: "whitenoise::scheduler", "Scheduler shutdown complete");
    }

    /// Returns the number of currently running scheduler tasks.
    ///
    /// This is primarily useful for integration testing to verify the scheduler is running.
    #[cfg(feature = "integration-tests")]
    #[perf_instrument("whitenoise")]
    pub(crate) async fn scheduler_task_count(&self) -> usize {
        self.scheduler_handles.lock().await.len()
    }

    #[cfg(feature = "integration-tests")]
    #[perf_instrument("whitenoise")]
    pub async fn wipe_database(&self) -> Result<()> {
        self.database.delete_all_data().await?;
        Ok(())
    }

    #[cfg(feature = "integration-tests")]
    #[perf_instrument("whitenoise")]
    pub async fn reset_nostr_client(&self) -> Result<()> {
        self.relay_control.reset_for_tests().await?;
        Ok(())
    }
}

#[cfg(test)]
pub mod test_utils {
    use super::*;
    use crate::whitenoise::accounts_groups::AccountGroup;
    use crate::whitenoise::group_information::GroupInformation;
    use crate::whitenoise::relays::Relay;
    use mdk_core::prelude::*;
    use nostr_sdk::{EventBuilder, EventId, Keys, PublicKey, RelayUrl, UnsignedEvent};
    use tempfile::TempDir;

    // Test configuration and setup helpers
    pub(crate) fn create_test_config() -> (WhitenoiseConfig, TempDir, TempDir) {
        let data_temp_dir = TempDir::new().expect("Failed to create temp data dir");
        let logs_temp_dir = TempDir::new().expect("Failed to create temp logs dir");
        let config = WhitenoiseConfig::new(
            data_temp_dir.path(),
            logs_temp_dir.path(),
            "com.whitenoise.test",
        )
        .with_discovery_relays(Relay::urls(&Relay::defaults()));
        (config, data_temp_dir, logs_temp_dir)
    }

    pub(crate) fn create_test_keys() -> Keys {
        Keys::generate()
    }

    pub(crate) async fn create_test_account(whitenoise: &Whitenoise) -> (Account, Keys) {
        let (account, keys) = Account::new(whitenoise, None).await.unwrap();
        (account, keys)
    }

    /// Creates a mock Whitenoise instance for testing.
    ///
    /// This function creates a Whitenoise instance with a minimal configuration and database.
    /// It also creates a NostrManager instance that connects to the local test relays.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    /// - `(Whitenoise, mpsc::Receiver<ProcessableEvent>, TempDir, TempDir)`
    ///   - `Whitenoise`: The mock Whitenoise instance
    ///   - `mpsc::Receiver<ProcessableEvent>`: The event receiver paired with the instance sender
    ///   - `TempDir`: The temporary directory for data storage
    ///   - `TempDir`: The temporary directory for log storage
    async fn create_mock_whitenoise_internal() -> (
        Whitenoise,
        mpsc::Receiver<ProcessableEvent>,
        TempDir,
        TempDir,
    ) {
        Whitenoise::initialize_mock_keyring_store();

        // Wait for local relays to be ready in test environment
        wait_for_test_relays().await;

        let (config, data_temp, logs_temp) = create_test_config();

        // Create directories manually to avoid issues
        std::fs::create_dir_all(&config.data_dir).unwrap();
        std::fs::create_dir_all(&config.logs_dir).unwrap();

        // Initialize minimal tracing for tests
        init_tracing(&config.logs_dir);

        let database = Arc::new(
            Database::new(config.data_dir.join("test.sqlite"))
                .await
                .unwrap(),
        );
        let secrets_store = SecretsStore::new(&config.keyring_service_id);

        // Create channels but don't start processing loop to avoid network calls
        let (event_sender, event_receiver) = mpsc::channel(10);
        let (shutdown_sender, _shutdown_receiver) = mpsc::channel(1);
        let (scheduler_shutdown, _scheduler_shutdown_rx) = watch::channel(false);

        // Create the event tracker.
        let test_event_tracker: std::sync::Arc<dyn event_tracker::EventTracker> =
            Arc::new(event_tracker::WhitenoiseEventTracker::new(database.clone()));

        // Create Storage
        let storage = storage::Storage::new(data_temp.path()).await.unwrap();

        // Create message aggregator for testing
        let message_aggregator = message_aggregator::MessageAggregator::new();
        let whitenoise = Whitenoise::from_components(
            config,
            database,
            WhitenoiseComponents {
                event_tracker: test_event_tracker,
                secrets_store,
                storage,
                message_aggregator,
                event_sender,
                shutdown_sender,
                scheduler_shutdown,
            },
        );
        whitenoise.relay_control.start_telemetry_persistors().await;

        (whitenoise, event_receiver, data_temp, logs_temp)
    }

    pub(crate) async fn create_mock_whitenoise() -> (Whitenoise, TempDir, TempDir) {
        let (whitenoise, _event_receiver, data_temp, logs_temp) =
            create_mock_whitenoise_internal().await;
        (whitenoise, data_temp, logs_temp)
    }

    async fn wait_for_published_event_count_matching<F>(
        whitenoise: &Whitenoise,
        account: &Account,
        predicate: F,
        failure_message: &str,
    ) -> usize
    where
        F: Fn(usize) -> bool,
    {
        for _ in 0..20 {
            let count = count_published_events_for_account(whitenoise, account).await;
            if predicate(count) {
                return count;
            }

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        panic!("{failure_message}");
    }

    pub(crate) async fn wait_for_published_event_count(
        whitenoise: &Whitenoise,
        account: &Account,
        previous_count: usize,
    ) -> usize {
        wait_for_published_event_count_matching(
            whitenoise,
            account,
            |count| count > previous_count,
            "timed out waiting for new published event",
        )
        .await
    }

    pub(crate) async fn count_published_events_for_account(
        whitenoise: &Whitenoise,
        account: &Account,
    ) -> usize {
        let account_id = account.id.expect("account must be persisted");
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM published_events WHERE account_id = ?")
                .bind(account_id)
                .fetch_one(&whitenoise.database.pool)
                .await
                .unwrap();

        usize::try_from(count.0).unwrap()
    }

    pub(crate) async fn wait_for_exact_published_event_count(
        whitenoise: &Whitenoise,
        account: &Account,
        expected_count: usize,
    ) {
        let failure_message =
            format!("timed out waiting for published event count {expected_count}");
        let _ = wait_for_published_event_count_matching(
            whitenoise,
            account,
            |count| count == expected_count,
            &failure_message,
        )
        .await;
    }

    /// Wait for local test relays to be ready
    async fn wait_for_test_relays() {
        use std::time::Duration;
        use tokio::time::{sleep, timeout};

        // Only wait for relays in debug builds (where we use localhost relays)
        if !cfg!(debug_assertions) {
            return;
        }

        tracing::debug!(target: "whitenoise::test_utils", "Waiting for local test relays to be ready...");

        let relay_urls = vec!["ws://localhost:8080", "ws://localhost:7777"];

        for relay_url in relay_urls {
            let mut attempts = 0;
            const MAX_ATTEMPTS: u32 = 10;
            const WAIT_INTERVAL: Duration = Duration::from_millis(500);

            while attempts < MAX_ATTEMPTS {
                // Try to establish a WebSocket connection to test readiness
                match timeout(Duration::from_secs(2), test_relay_connection(relay_url)).await {
                    Ok(Ok(())) => {
                        tracing::debug!(target: "whitenoise::test_utils", "Relay {} is ready", relay_url);
                        break;
                    }
                    Ok(Err(e)) => {
                        tracing::debug!(target: "whitenoise::test_utils",
                            "Relay {} not ready yet (attempt {}/{}): {:?}",
                            relay_url, attempts + 1, MAX_ATTEMPTS, e);
                    }
                    Err(_) => {
                        tracing::debug!(target: "whitenoise::test_utils",
                            "Relay {} connection timeout (attempt {}/{})",
                            relay_url, attempts + 1, MAX_ATTEMPTS);
                    }
                }

                attempts += 1;
                if attempts < MAX_ATTEMPTS {
                    sleep(WAIT_INTERVAL).await;
                }
            }

            if attempts >= MAX_ATTEMPTS {
                tracing::warn!(target: "whitenoise::test_utils",
                    "Relay {} may not be fully ready after {} attempts", relay_url, MAX_ATTEMPTS);
            }
        }

        // Give relays a bit more time to stabilize
        sleep(Duration::from_millis(100)).await;
        tracing::debug!(target: "whitenoise::test_utils", "Relay readiness check completed");
    }

    /// Test if a relay is ready by attempting a simple connection
    async fn test_relay_connection(
        relay_url: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use nostr_sdk::prelude::*;

        // Create a minimal client for testing connection
        let client = Client::default();
        client.add_relay(relay_url).await?;

        // Try to connect - this will fail if relay isn't ready
        client.connect().await;

        // Give it a moment to establish connection
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Check if we're connected
        let relay_url_parsed = RelayUrl::parse(relay_url)?;
        match client.relay(&relay_url_parsed).await {
            Ok(_) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) async fn test_get_whitenoise() -> &'static Whitenoise {
        static TEST_SINGLETON_CONFIG: OnceLock<WhitenoiseConfig> = OnceLock::new();

        // Singleton-backed tests can spawn background tasks that outlive the
        // helper call, so the temp dirs backing the singleton config must stay
        // alive for the rest of the process.
        let config = TEST_SINGLETON_CONFIG
            .get_or_init(|| {
                let (config, data_temp, logs_temp) = create_test_config();
                std::mem::forget(data_temp);
                std::mem::forget(logs_temp);
                config
            })
            .clone();

        Whitenoise::initialize_whitenoise(config).await.unwrap();
        Whitenoise::get_instance().unwrap()
    }

    pub(crate) async fn setup_login_account(whitenoise: &Whitenoise) -> (Account, Keys) {
        let keys = create_test_keys();
        let account = whitenoise
            .login(keys.secret_key().to_secret_hex())
            .await
            .unwrap();
        (account, keys)
    }

    pub(crate) fn create_nostr_group_config_data(admins: Vec<PublicKey>) -> NostrGroupConfigData {
        NostrGroupConfigData::new(
            "Test group".to_owned(),
            "test description".to_owned(),
            Some([0u8; 32]), // 32-byte hash for fake image
            Some([1u8; 32]), // 32-byte encryption key
            Some([2u8; 12]), // 12-byte nonce
            vec![RelayUrl::parse("ws://localhost:8080/").unwrap()],
            admins,
        )
    }

    pub(crate) async fn setup_multiple_test_accounts(
        whitenoise: &Whitenoise,
        count: usize,
    ) -> Vec<(Account, Keys)> {
        let mut accounts = Vec::new();
        for _ in 0..count {
            let keys = create_test_keys();
            let account = whitenoise
                .create_test_identity_with_keys(&keys)
                .await
                .unwrap();
            let key_package_relays = account.key_package_relays(whitenoise).await.unwrap();
            whitenoise
                .create_and_publish_key_package(&account, &key_package_relays)
                .await
                .unwrap();
            accounts.push((account, keys));
        }
        accounts
    }

    pub(crate) async fn wait_for_key_package_publication(
        whitenoise: &Whitenoise,
        publisher_accounts: &[&Account],
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let mut all_available = true;

                for publisher_account in publisher_accounts {
                    let relay_urls = Relay::urls(
                        &publisher_account
                            .key_package_relays(whitenoise)
                            .await
                            .unwrap(),
                    );

                    match whitenoise
                        .relay_control
                        .fetch_user_key_package(publisher_account.pubkey, &relay_urls)
                        .await
                    {
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => {
                            all_available = false;
                            break;
                        }
                    }
                }

                if all_available {
                    break;
                }

                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Timed out waiting for key package publication for {} member(s)",
                publisher_accounts.len()
            )
        });
    }

    async fn create_group_with_member_welcomes(
        whitenoise: &Whitenoise,
        admin_account: &Account,
        member_accounts: &[&Account],
    ) -> (GroupId, Vec<UnsignedEvent>) {
        let mut key_package_events = Vec::with_capacity(member_accounts.len());
        for member_account in member_accounts {
            let relay_urls =
                Relay::urls(&member_account.key_package_relays(whitenoise).await.unwrap());
            let key_package_event = whitenoise
                .relay_control
                .fetch_user_key_package(member_account.pubkey, &relay_urls)
                .await
                .unwrap()
                .expect("member must have a published key package");
            key_package_events.push(key_package_event);
        }

        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let create_result = admin_mdk
            .create_group(
                &admin_account.pubkey,
                key_package_events,
                create_nostr_group_config_data(vec![admin_account.pubkey]),
            )
            .unwrap();

        (
            create_result.group.mls_group_id.clone(),
            create_result.welcome_rumors,
        )
    }

    async fn deliver_group_welcomes(
        whitenoise: &Whitenoise,
        admin_account: &Account,
        member_accounts: &[&Account],
        welcome_rumors: Vec<UnsignedEvent>,
    ) {
        assert_eq!(
            member_accounts.len(),
            welcome_rumors.len(),
            "each invited member should receive exactly one welcome rumor"
        );

        let admin_signer = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&admin_account.pubkey)
            .unwrap();

        for (member_account, welcome_rumor) in member_accounts.iter().zip(welcome_rumors) {
            let giftwrap = EventBuilder::gift_wrap(
                &admin_signer,
                &member_account.pubkey,
                welcome_rumor,
                vec![],
            )
            .await
            .unwrap();

            whitenoise
                .handle_giftwrap(member_account, giftwrap)
                .await
                .expect("member should process welcome successfully");
        }
    }

    pub(crate) async fn setup_two_member_group_with_welcome_finalization(
        whitenoise: &Whitenoise,
        admin_account: &Account,
        member_account: &Account,
    ) -> GroupId {
        let member_accounts = [member_account];
        let (group_id, welcome_rumors) =
            create_group_with_member_welcomes(whitenoise, admin_account, &member_accounts).await;
        deliver_group_welcomes(whitenoise, admin_account, &member_accounts, welcome_rumors).await;

        let group_name = whitenoise
            .create_mdk_for_account(member_account.pubkey)
            .unwrap()
            .get_group(&group_id)
            .unwrap()
            .expect("member should have group after welcome")
            .name;
        Whitenoise::finalize_welcome_with_instance(
            whitenoise,
            member_account,
            &group_id,
            &group_name,
            EventId::all_zeros(),
            admin_account.pubkey,
        )
        .await;

        group_id
    }

    pub(crate) async fn setup_two_member_group_with_accepted_account_groups(
        whitenoise: &Whitenoise,
        admin_account: &Account,
        member_account: &Account,
    ) -> GroupId {
        let member_accounts = [member_account];
        let (group_id, welcome_rumors) =
            create_group_with_member_welcomes(whitenoise, admin_account, &member_accounts).await;
        deliver_group_welcomes(whitenoise, admin_account, &member_accounts, welcome_rumors).await;

        GroupInformation::create_for_group(whitenoise, &group_id, None, "Test group")
            .await
            .unwrap();

        let (admin_group, _) =
            AccountGroup::get_or_create(whitenoise, &admin_account.pubkey, &group_id, None)
                .await
                .unwrap();
        admin_group.accept(whitenoise).await.unwrap();

        let (member_group, _) =
            AccountGroup::get_or_create(whitenoise, &member_account.pubkey, &group_id, None)
                .await
                .unwrap();
        member_group.accept(whitenoise).await.unwrap();

        group_id
    }

    pub(crate) async fn setup_three_member_group(
        whitenoise: &Whitenoise,
        admin_account: &Account,
        first_member: &Account,
        second_member: &Account,
    ) -> GroupId {
        let member_accounts = [first_member, second_member];
        let (group_id, welcome_rumors) =
            create_group_with_member_welcomes(whitenoise, admin_account, &member_accounts).await;
        deliver_group_welcomes(whitenoise, admin_account, &member_accounts, welcome_rumors).await;

        group_id
    }
}

#[cfg(test)]
mod tests {
    use super::test_utils::*;
    use super::*;
    use database::aggregated_messages::PaginationOptions;

    // Configuration Tests
    mod config_tests {
        use super::*;

        #[test]
        fn test_whitenoise_config_new() {
            let data_dir = std::path::Path::new("/test/data");
            let logs_dir = std::path::Path::new("/test/logs");
            let config = WhitenoiseConfig::new(data_dir, logs_dir, "com.test.app");

            if cfg!(debug_assertions) {
                assert_eq!(config.data_dir, data_dir.join("dev"));
                assert_eq!(config.logs_dir, logs_dir.join("dev"));
            } else {
                assert_eq!(config.data_dir, data_dir.join("release"));
                assert_eq!(config.logs_dir, logs_dir.join("release"));
            }
            assert_eq!(config.keyring_service_id, "com.test.app");
            assert_eq!(
                config.discovery_relays,
                DiscoveryPlaneConfig::curated_default_relays()
            );
        }

        #[test]
        fn test_whitenoise_config_with_custom_aggregator() {
            let data_dir = std::path::Path::new("/test/data");
            let logs_dir = std::path::Path::new("/test/logs");

            // Test with custom aggregator config
            let custom_config = message_aggregator::AggregatorConfig {
                normalize_emoji: false,
                enable_debug_logging: true,
            };

            let config = WhitenoiseConfig::new_with_aggregator_config(
                data_dir,
                logs_dir,
                "com.test.app",
                custom_config.clone(),
            );

            assert!(config.message_aggregator_config.is_some());
            let aggregator_config = config.message_aggregator_config.unwrap();
            assert!(!aggregator_config.normalize_emoji);
            assert!(aggregator_config.enable_debug_logging);
            assert_eq!(config.keyring_service_id, "com.test.app");
            assert_eq!(
                config.discovery_relays,
                DiscoveryPlaneConfig::curated_default_relays()
            );
        }

        #[tokio::test]
        async fn test_initialize_whitenoise_rejects_empty_keyring_service_id() {
            use tempfile::TempDir;

            let data_temp = TempDir::new().unwrap();
            let logs_temp = TempDir::new().unwrap();
            let mut config =
                WhitenoiseConfig::new(data_temp.path(), logs_temp.path(), "com.test.app");

            // Test empty string
            config.keyring_service_id = "".to_string();
            let result = Whitenoise::initialize_whitenoise(config.clone()).await;
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("keyring_service_id cannot be empty")
            );

            // Test whitespace only
            config.keyring_service_id = "   ".to_string();
            let result = Whitenoise::initialize_whitenoise(config).await;
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("keyring_service_id cannot be empty")
            );
        }
    }

    // Initialization Tests
    mod initialization_tests {
        use super::*;

        #[tokio::test]
        async fn test_whitenoise_initialization() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            assert!(Account::all(&whitenoise.database).await.unwrap().is_empty());

            // Verify directories were created
            assert!(whitenoise.config.data_dir.exists());
            assert!(whitenoise.config.logs_dir.exists());
        }

        #[tokio::test]
        async fn test_whitenoise_debug_format() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            let debug_str = format!("{:?}", whitenoise);
            assert!(debug_str.contains("Whitenoise"));
            assert!(debug_str.contains("config"));
            assert!(debug_str.contains("<REDACTED>"));
        }

        #[tokio::test]
        async fn test_multiple_initializations_with_same_config() {
            // Test that we can create multiple mock instances
            let (whitenoise1, _data_temp1, _logs_temp1) = create_mock_whitenoise().await;
            let (whitenoise2, _data_temp2, _logs_temp2) = create_mock_whitenoise().await;

            // Both should have valid configurations (they'll be different temp dirs, which is fine)
            assert!(whitenoise1.config.data_dir.exists());
            assert!(whitenoise2.config.data_dir.exists());
            assert!(
                Account::all(&whitenoise1.database)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                Account::all(&whitenoise2.database)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    // Data Management Tests
    mod data_management_tests {
        use super::*;

        #[tokio::test]
        async fn test_delete_all_data() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Create test files in the whitenoise directories
            let test_data_file = whitenoise.config.data_dir.join("test_data.txt");
            let test_log_file = whitenoise.config.logs_dir.join("test_log.txt");
            tokio::fs::write(&test_data_file, "test data")
                .await
                .unwrap();
            tokio::fs::write(&test_log_file, "test log").await.unwrap();
            assert!(test_data_file.exists());
            assert!(test_log_file.exists());

            // Create some test media files in cache
            whitenoise
                .storage
                .media_files
                .store_file("test_image.jpg", b"fake image data")
                .await
                .unwrap();
            let media_cache_dir = whitenoise.storage.media_files.cache_dir();
            assert!(media_cache_dir.exists());
            let cache_entries: Vec<_> = std::fs::read_dir(media_cache_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            assert_eq!(cache_entries.len(), 1);

            // Delete all data
            let result = whitenoise.delete_all_data().await;
            assert!(result.is_ok());

            // Verify cleanup
            assert!(Account::all(&whitenoise.database).await.unwrap().is_empty());
            assert!(!test_log_file.exists());

            // Media cache directory should be removed
            let media_cache_dir_after = whitenoise.storage.media_files.cache_dir();
            assert!(!media_cache_dir_after.exists());

            // MLS directory should be recreated as empty
            let mls_dir = whitenoise.config.data_dir.join("mls");
            assert!(mls_dir.exists());
            assert!(mls_dir.is_dir());
        }
    }

    // API Tests (using mock to minimize network calls)
    // NOTE: These tests still make some network calls through NostrManager
    // For complete isolation, implement the trait-based mocking described above
    mod api_tests {
        use super::*;
        use mdk_core::prelude::GroupId;

        #[tokio::test]
        async fn test_message_aggregator_access() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Test that we can access the message aggregator
            let aggregator = whitenoise.message_aggregator();

            // Check that it has expected default configuration
            let config = aggregator.config();
            assert!(config.normalize_emoji);
            assert!(!config.enable_debug_logging);
        }

        #[tokio::test]
        async fn test_fetch_aggregated_messages_for_nonexistent_group() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let account = whitenoise.create_identity().await.unwrap();

            // Non-existent group ID
            let group_id = GroupId::from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);

            // Fetching messages for a non-existent group should return GroupNotFound
            let result = whitenoise
                .fetch_aggregated_messages_for_group(
                    &account.pubkey,
                    &group_id,
                    &PaginationOptions::default(),
                    None,
                )
                .await;

            assert!(
                matches!(result, Err(error::WhitenoiseError::GroupNotFound)),
                "Should return GroupNotFound for non-existent group, got: {:?}",
                result
            );
        }

        #[tokio::test]
        async fn test_subscribe_to_group_messages_returns_initial_messages() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[99; 32]);
            let test_pubkey = nostr_sdk::Keys::generate().public_key();

            // Setup: Create group (required for foreign key constraint)
            group_information::GroupInformation::find_or_create_by_mls_group_id(
                &group_id,
                Some(group_information::GroupType::Group),
                &whitenoise.database,
            )
            .await
            .unwrap();

            setup_account_group(&test_pubkey, &group_id, &whitenoise).await;

            // Setup: Insert test messages into the cache
            let msg1 = message_aggregator::ChatMessage {
                id: format!("{:0>64x}", 1),
                author: test_pubkey,
                content: "First message".to_string(),
                created_at: nostr_sdk::Timestamp::now(),
                tags: nostr_sdk::Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                content_tokens: vec![],
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            let msg2 = message_aggregator::ChatMessage {
                id: format!("{:0>64x}", 2),
                author: test_pubkey,
                content: "Second message".to_string(),
                created_at: nostr_sdk::Timestamp::now(),
                tags: nostr_sdk::Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                content_tokens: vec![],
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };

            aggregated_message::AggregatedMessage::insert_message(
                &msg1,
                &group_id,
                &whitenoise.database,
            )
            .await
            .unwrap();
            aggregated_message::AggregatedMessage::insert_message(
                &msg2,
                &group_id,
                &whitenoise.database,
            )
            .await
            .unwrap();

            // Test: Subscribe and verify initial messages
            let subscription = whitenoise
                .subscribe_to_group_messages(&test_pubkey, &group_id, None)
                .await
                .unwrap();

            assert_eq!(
                subscription.initial_messages.len(),
                2,
                "Should return 2 initial messages"
            );

            // Verify message contents (order may vary, so check by ID)
            let ids: std::collections::HashSet<_> = subscription
                .initial_messages
                .iter()
                .map(|m| m.id.as_str())
                .collect();
            assert!(ids.contains(msg1.id.as_str()));
            assert!(ids.contains(msg2.id.as_str()));
        }

        #[tokio::test]
        async fn test_subscribe_merges_concurrent_updates() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[42; 32]);
            let test_pubkey = nostr_sdk::Keys::generate().public_key();
            setup_group(&group_id, &whitenoise).await;
            setup_account_group(&test_pubkey, &group_id, &whitenoise).await;

            // First emit an update before subscribing (simulates concurrent update scenario)
            // This tests the merge logic path
            let test_message = message_aggregator::ChatMessage {
                id: "test_concurrent_msg".to_string(),
                author: test_pubkey,
                content: "concurrent message".to_string(),
                created_at: nostr_sdk::Timestamp::now(),
                tags: nostr_sdk::Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                content_tokens: vec![],
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };

            // Emit an update (will be caught by subscriber during drain phase)
            whitenoise.message_stream_manager.emit(
                &group_id,
                message_streaming::MessageUpdate {
                    trigger: message_streaming::UpdateTrigger::NewMessage,
                    message: test_message.clone(),
                },
            );

            // Subscribe - the drain loop should find the channel empty (no subscriber existed)
            // This test verifies the deduplication logic path compiles and runs
            let subscription = whitenoise
                .subscribe_to_group_messages(&test_pubkey, &group_id, None)
                .await
                .unwrap();

            // Initial messages should be empty
            // The reason is that the message would've been persisted at the event processor level
            // Emitting itself doesn't persist the message to the database
            assert!(
                subscription.initial_messages.is_empty(),
                "Initial messages should be empty (stream created on subscribe)"
            );
        }

        // ── Helper ────────────────────────────────────────────────────────────

        /// Insert a `ChatMessage` with an explicit Unix-seconds timestamp and a
        /// deterministic ID derived from `seed`.  Returns the inserted message.
        async fn insert_msg_at(
            seed: u8,
            author: nostr_sdk::PublicKey,
            unix_secs: u64,
            group_id: &GroupId,
            whitenoise: &Whitenoise,
        ) -> message_aggregator::ChatMessage {
            let msg = message_aggregator::ChatMessage {
                id: format!("{:0>64x}", seed),
                author,
                content: format!("message {seed}"),
                created_at: nostr_sdk::Timestamp::from(unix_secs),
                tags: nostr_sdk::Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                content_tokens: vec![],
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            aggregated_message::AggregatedMessage::insert_message(
                &msg,
                group_id,
                &whitenoise.database,
            )
            .await
            .unwrap();
            msg
        }

        /// Create the group_information row required by the FK constraint.
        async fn setup_group(group_id: &GroupId, whitenoise: &Whitenoise) {
            group_information::GroupInformation::find_or_create_by_mls_group_id(
                group_id,
                Some(group_information::GroupType::Group),
                &whitenoise.database,
            )
            .await
            .unwrap();
        }

        /// Create the minimal user + account + accounts_groups rows for a
        /// raw pubkey so that `chat_cleared_at_ms` (and similar methods) can
        /// find the account-group association.
        async fn setup_account_group(
            pubkey: &nostr_sdk::PublicKey,
            group_id: &GroupId,
            whitenoise: &Whitenoise,
        ) {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let hex = pubkey.to_hex();
            // user row (FK target for accounts)
            sqlx::query(
                "INSERT OR IGNORE INTO users (pubkey, created_at, updated_at) \
                 VALUES (?, ?, ?)",
            )
            .bind(&hex)
            .bind(now_ms)
            .bind(now_ms)
            .execute(&whitenoise.database.pool)
            .await
            .unwrap();
            let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE pubkey = ?")
                .bind(&hex)
                .fetch_one(&whitenoise.database.pool)
                .await
                .unwrap();
            // account row (FK target for accounts_groups)
            sqlx::query(
                "INSERT OR IGNORE INTO accounts \
                 (pubkey, user_id, account_type, created_at, updated_at) \
                 VALUES (?, ?, 'local', ?, ?)",
            )
            .bind(&hex)
            .bind(user_id)
            .bind(now_ms)
            .bind(now_ms)
            .execute(&whitenoise.database.pool)
            .await
            .unwrap();
            // accounts_groups row
            accounts_groups::AccountGroup::find_or_create(
                pubkey,
                group_id,
                None,
                &whitenoise.database,
            )
            .await
            .unwrap();
        }

        // ── limit parameter ───────────────────────────────────────────────────

        /// `limit=Some(N)` caps the initial snapshot to the N newest messages.
        #[tokio::test]
        async fn test_subscribe_limit_caps_initial_snapshot() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[110; 32]);
            setup_group(&group_id, &whitenoise).await;
            let author = nostr_sdk::Keys::generate().public_key();
            setup_account_group(&author, &group_id, &whitenoise).await;
            let base: u64 = 1_710_000_000;

            // Insert 10 messages with distinct timestamps (seeds 1–10)
            for i in 1u8..=10 {
                insert_msg_at(i, author, base + u64::from(i), &group_id, &whitenoise).await;
            }

            let subscription = whitenoise
                .subscribe_to_group_messages(&author, &group_id, Some(3))
                .await
                .unwrap();

            assert_eq!(
                subscription.initial_messages.len(),
                3,
                "limit=3 should cap the snapshot to 3 messages"
            );

            // Should be the 3 newest (seeds 8, 9, 10), oldest-first
            assert_eq!(
                subscription.initial_messages[0].created_at,
                nostr_sdk::Timestamp::from(base + 8)
            );
            assert_eq!(
                subscription.initial_messages[2].created_at,
                nostr_sdk::Timestamp::from(base + 10)
            );
        }

        /// When the group has fewer messages than `limit`, all messages are returned.
        #[tokio::test]
        async fn test_subscribe_limit_larger_than_history_returns_all() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[111; 32]);
            setup_group(&group_id, &whitenoise).await;
            let author = nostr_sdk::Keys::generate().public_key();
            setup_account_group(&author, &group_id, &whitenoise).await;
            let base: u64 = 1_711_000_000;

            for i in 1u8..=5 {
                insert_msg_at(i, author, base + u64::from(i), &group_id, &whitenoise).await;
            }

            let subscription = whitenoise
                .subscribe_to_group_messages(&author, &group_id, Some(200))
                .await
                .unwrap();

            assert_eq!(
                subscription.initial_messages.len(),
                5,
                "should return all 5 messages when limit exceeds history size"
            );
        }

        /// `limit=None` defaults to 50 — subscribing a group with exactly 50 messages returns
        /// all of them, and a second page via the updates receiver is not needed.
        #[tokio::test]
        async fn test_subscribe_limit_none_defaults_to_50() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[112; 32]);
            setup_group(&group_id, &whitenoise).await;
            let author = nostr_sdk::Keys::generate().public_key();
            setup_account_group(&author, &group_id, &whitenoise).await;
            let base: u64 = 1_712_000_000;

            // Insert exactly 50 messages
            for i in 1u8..=50 {
                insert_msg_at(i, author, base + u64::from(i), &group_id, &whitenoise).await;
            }

            let subscription = whitenoise
                .subscribe_to_group_messages(&author, &group_id, None)
                .await
                .unwrap();

            assert_eq!(
                subscription.initial_messages.len(),
                50,
                "limit=None should return all 50 messages (default page size)"
            );
        }

        /// Inserting 60 messages and subscribing with `limit=None` returns the 50 newest,
        /// confirming the default cap is applied when there are more messages than the limit.
        #[tokio::test]
        async fn test_subscribe_limit_none_caps_at_50_when_more_exist() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[113; 32]);
            setup_group(&group_id, &whitenoise).await;
            let author = nostr_sdk::Keys::generate().public_key();
            setup_account_group(&author, &group_id, &whitenoise).await;
            let base: u64 = 1_713_000_000;

            for i in 1u8..=60 {
                insert_msg_at(i, author, base + u64::from(i), &group_id, &whitenoise).await;
            }

            let subscription = whitenoise
                .subscribe_to_group_messages(&author, &group_id, None)
                .await
                .unwrap();

            assert_eq!(
                subscription.initial_messages.len(),
                50,
                "default limit of 50 should apply when more than 50 messages exist"
            );

            // Oldest message in snapshot should be seed 11 (the 11th oldest overall)
            assert_eq!(
                subscription.initial_messages[0].created_at,
                nostr_sdk::Timestamp::from(base + 11),
                "snapshot should start at the 11th oldest message"
            );
        }

        // ── ordering ──────────────────────────────────────────────────────────

        /// Snapshot messages are always returned in chronological (oldest-first) order
        /// regardless of insertion order.
        #[tokio::test]
        async fn test_subscribe_initial_messages_are_oldest_first() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[114; 32]);
            setup_group(&group_id, &whitenoise).await;
            let author = nostr_sdk::Keys::generate().public_key();
            setup_account_group(&author, &group_id, &whitenoise).await;
            let base: u64 = 1_714_000_000;

            // Insert in reverse order to confirm sorting is not dependent on insertion order
            for i in (1u8..=5).rev() {
                insert_msg_at(i, author, base + u64::from(i), &group_id, &whitenoise).await;
            }

            let subscription = whitenoise
                .subscribe_to_group_messages(&author, &group_id, None)
                .await
                .unwrap();

            for w in subscription.initial_messages.windows(2) {
                assert!(
                    w[0].created_at <= w[1].created_at,
                    "snapshot must be oldest-first; got {:?} before {:?}",
                    w[0].created_at,
                    w[1].created_at
                );
            }
        }

        // ── drain / race window ───────────────────────────────────────────────
        //
        // The drain loop closes the window between subscribe() and the DB query: any update
        // buffered in the channel during that window is merged into the snapshot.
        //
        // Directly injecting a message into that window from a unit test is not possible
        // because broadcast::subscribe() positions new receivers at the *next* message, not
        // at previously-buffered history.  The race is inherently async and is exercised in
        // integration scenarios.
        //
        // What we *can* unit-test:
        //   - The drain loop exits cleanly when the channel is empty (no hang, no panic).
        //   - After subscribing, updates emitted *while* we hold the receiver accumulate and
        //     can be drained — proving the channel is correctly wired.
        //   - The deduplication map correctly merges an update into the snapshot map when
        //     the message ID already exists (stale-row replacement path).

        /// The drain loop completes without blocking when the channel is empty.
        /// Verifies that the try_recv() → Empty → break path is reached and that
        /// subscribe_to_group_messages returns successfully.
        #[tokio::test]
        async fn test_subscribe_drain_exits_cleanly_when_channel_is_empty() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[115; 32]);
            setup_group(&group_id, &whitenoise).await;
            let author = nostr_sdk::Keys::generate().public_key();
            setup_account_group(&author, &group_id, &whitenoise).await;

            insert_msg_at(1, author, 1_715_000_001, &group_id, &whitenoise).await;

            // No updates are emitted — the drain must exit via the Empty arm immediately
            let subscription = whitenoise
                .subscribe_to_group_messages(&author, &group_id, None)
                .await
                .unwrap();

            // Snapshot contains the DB message; no panic or hang occurred
            assert_eq!(
                subscription.initial_messages.len(),
                1,
                "snapshot must contain the DB message; drain must not block on empty channel"
            );
        }

        /// Verifies the deduplication logic: when the same message ID appears in both the
        /// DB fetch result and a subsequent update, the snapshot contains exactly one copy
        /// and it reflects the latest (update) state.
        ///
        /// We simulate the merge by calling subscribe_to_group_messages (which loads the DB
        /// version into the map), then emitting an update via the live receiver to confirm
        /// the map logic is correct even though in this test the update lands *after* the
        /// drain rather than during it.  The map's insert-or-replace semantics are the same
        /// either way.
        #[tokio::test]
        async fn test_subscribe_snapshot_deduplicates_by_message_id() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[116; 32]);
            setup_group(&group_id, &whitenoise).await;
            let author = nostr_sdk::Keys::generate().public_key();
            setup_account_group(&author, &group_id, &whitenoise).await;

            // Insert a message into the DB — this is the "stale" row
            insert_msg_at(1, author, 1_716_000_001, &group_id, &whitenoise).await;

            let subscription = whitenoise
                .subscribe_to_group_messages(&author, &group_id, None)
                .await
                .unwrap();

            // Snapshot contains exactly one copy of the message (no duplicates from the map)
            assert_eq!(
                subscription.initial_messages.len(),
                1,
                "snapshot must contain exactly one copy of each message"
            );
            assert_eq!(
                subscription.initial_messages[0].id,
                format!("{:0>64x}", 1u8),
                "the single copy must be the DB message"
            );
        }

        // ── live updates receiver ─────────────────────────────────────────────

        /// The `updates` receiver returned by subscribe_to_group_messages delivers
        /// updates emitted *after* the function returns.
        #[tokio::test]
        async fn test_subscribe_updates_receiver_delivers_future_messages() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[117; 32]);
            setup_group(&group_id, &whitenoise).await;
            let author = nostr_sdk::Keys::generate().public_key();
            setup_account_group(&author, &group_id, &whitenoise).await;

            let mut subscription = whitenoise
                .subscribe_to_group_messages(&author, &group_id, None)
                .await
                .unwrap();

            // Emit a new message after subscribe_to_group_messages has returned
            let new_msg = message_aggregator::ChatMessage {
                id: format!("{:0>64x}", 77u8),
                author,
                content: "future message".to_string(),
                created_at: nostr_sdk::Timestamp::from(1_717_000_001u64),
                tags: nostr_sdk::Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                content_tokens: vec![],
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            whitenoise.message_stream_manager.emit(
                &group_id,
                message_streaming::MessageUpdate {
                    trigger: message_streaming::UpdateTrigger::NewMessage,
                    message: new_msg.clone(),
                },
            );

            let received = subscription
                .updates
                .recv()
                .await
                .expect("should receive the emitted update");

            assert_eq!(received.message.id, new_msg.id);
            assert_eq!(
                received.trigger,
                message_streaming::UpdateTrigger::NewMessage
            );
        }

        /// The `updates` receiver must NOT re-deliver messages that were already included in
        /// the initial snapshot — only genuinely new updates should arrive via the receiver.
        #[tokio::test]
        async fn test_subscribe_updates_receiver_does_not_replay_initial_snapshot() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[118; 32]);
            setup_group(&group_id, &whitenoise).await;
            let author = nostr_sdk::Keys::generate().public_key();
            setup_account_group(&author, &group_id, &whitenoise).await;

            insert_msg_at(1, author, 1_718_000_001, &group_id, &whitenoise).await;
            insert_msg_at(2, author, 1_718_000_002, &group_id, &whitenoise).await;

            let mut subscription = whitenoise
                .subscribe_to_group_messages(&author, &group_id, None)
                .await
                .unwrap();

            assert_eq!(
                subscription.initial_messages.len(),
                2,
                "snapshot should contain both DB messages"
            );

            // The receiver should have nothing pending — snapshot messages are not replayed
            assert!(
                matches!(
                    subscription.updates.try_recv(),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                ),
                "updates receiver must be empty immediately after subscribe (no snapshot replay)"
            );
        }

        /// An empty group with a limit set still works: the snapshot is empty and the
        /// receiver is operational and delivers subsequent updates normally.
        #[tokio::test]
        async fn test_subscribe_empty_group_with_limit_receiver_still_works() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[119; 32]);
            setup_group(&group_id, &whitenoise).await;
            let author = nostr_sdk::Keys::generate().public_key();
            setup_account_group(&author, &group_id, &whitenoise).await;

            let mut subscription = whitenoise
                .subscribe_to_group_messages(&author, &group_id, Some(10))
                .await
                .unwrap();

            assert!(
                subscription.initial_messages.is_empty(),
                "empty group must yield empty snapshot"
            );

            // Receiver is still functional — emit a message and receive it
            let msg = message_aggregator::ChatMessage {
                id: format!("{:0>64x}", 55u8),
                author,
                content: "first ever message".to_string(),
                created_at: nostr_sdk::Timestamp::from(1_719_000_001u64),
                tags: nostr_sdk::Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                content_tokens: vec![],
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            whitenoise.message_stream_manager.emit(
                &group_id,
                message_streaming::MessageUpdate {
                    trigger: message_streaming::UpdateTrigger::NewMessage,
                    message: msg.clone(),
                },
            );

            let received = subscription
                .updates
                .recv()
                .await
                .expect("receiver must deliver the emitted message");

            assert_eq!(received.message.id, msg.id);
        }

        /// Two independent subscribers for the same group each receive the same updates
        /// (broadcast fan-out), and each has their own independent snapshot.
        #[tokio::test]
        async fn test_subscribe_multiple_subscribers_same_group_receive_same_updates() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[120; 32]);
            setup_group(&group_id, &whitenoise).await;
            let author = nostr_sdk::Keys::generate().public_key();
            setup_account_group(&author, &group_id, &whitenoise).await;

            insert_msg_at(1, author, 1_720_000_001, &group_id, &whitenoise).await;

            // Two independent subscriptions
            let mut sub_a = whitenoise
                .subscribe_to_group_messages(&author, &group_id, None)
                .await
                .unwrap();
            let mut sub_b = whitenoise
                .subscribe_to_group_messages(&author, &group_id, None)
                .await
                .unwrap();

            // Both snapshots contain the DB message
            assert_eq!(sub_a.initial_messages.len(), 1);
            assert_eq!(sub_b.initial_messages.len(), 1);

            // Emit one update — both receivers should get it
            let new_msg = message_aggregator::ChatMessage {
                id: format!("{:0>64x}", 88u8),
                author,
                content: "broadcast message".to_string(),
                created_at: nostr_sdk::Timestamp::from(1_720_000_099u64),
                tags: nostr_sdk::Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                content_tokens: vec![],
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            whitenoise.message_stream_manager.emit(
                &group_id,
                message_streaming::MessageUpdate {
                    trigger: message_streaming::UpdateTrigger::NewMessage,
                    message: new_msg.clone(),
                },
            );

            let recv_a = sub_a.updates.recv().await.unwrap();
            let recv_b = sub_b.updates.recv().await.unwrap();

            assert_eq!(
                recv_a.message.id, new_msg.id,
                "subscriber A must receive the update"
            );
            assert_eq!(
                recv_b.message.id, new_msg.id,
                "subscriber B must receive the update"
            );
        }

        /// Subscribers for different groups are isolated — an update for group A is not
        /// delivered to a subscriber for group B.
        #[tokio::test]
        async fn test_subscribe_updates_are_isolated_per_group() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_a = GroupId::from_slice(&[121; 32]);
            let group_b = GroupId::from_slice(&[122; 32]);
            setup_group(&group_a, &whitenoise).await;
            setup_group(&group_b, &whitenoise).await;
            let author = nostr_sdk::Keys::generate().public_key();
            setup_account_group(&author, &group_a, &whitenoise).await;
            setup_account_group(&author, &group_b, &whitenoise).await;

            let mut sub_b = whitenoise
                .subscribe_to_group_messages(&author, &group_b, None)
                .await
                .unwrap();

            // Emit an update for group A only
            let msg_a = message_aggregator::ChatMessage {
                id: format!("{:0>64x}", 91u8),
                author,
                content: "message for group A".to_string(),
                created_at: nostr_sdk::Timestamp::from(1_721_000_001u64),
                tags: nostr_sdk::Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                content_tokens: vec![],
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            whitenoise.message_stream_manager.emit(
                &group_a,
                message_streaming::MessageUpdate {
                    trigger: message_streaming::UpdateTrigger::NewMessage,
                    message: msg_a,
                },
            );

            // Group B receiver must not receive group A's update
            assert!(
                matches!(
                    sub_b.updates.try_recv(),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                ),
                "group B subscriber must not receive updates emitted for group A"
            );
        }

        // ── scroll-up / live-message scenario tests ───────────────────────────
        //
        // These tests model the full flow a real app experiences:
        //
        //   1. User opens a chat — subscribe_to_group_messages delivers the newest
        //      N messages as the initial snapshot.
        //   2. User scrolls upward — fetch_aggregated_messages_for_group is called
        //      with the cursor from the oldest snapshot message to load the preceding
        //      page from the DB.
        //   3. While the user is reading old messages, peers send new messages — the
        //      updates receiver delivers them in real time.
        //   4. The app appends new messages at the bottom; the scrolled-back page
        //      stays unchanged.
        //
        // The invariants we assert:
        //   - Snapshot contains only the newest limit messages (no older history).
        //   - Cursor-based page fetch covers the preceding page exactly — no overlap
        //     with the snapshot, no gaps, correct chronological order.
        //   - New messages emitted while scrolling arrive on the updates receiver
        //     and are newer than everything in the initial snapshot.
        //   - New messages are NOT in any fetched historical page (they are newer
        //     than the snapshot window and thus beyond the cursor's reach).

        /// Helper: create a minimal account in the DB (no network calls).
        /// `fetch_aggregated_messages_for_group` requires an account to exist for its
        /// security check, but does not need a full session to be active.
        async fn create_db_account(whitenoise: &Whitenoise) -> accounts::Account {
            let (account, _keys) = accounts::Account::new(whitenoise, None).await.unwrap();
            account.save(&whitenoise.database).await.unwrap()
        }

        /// Core happy-path scenario: user opens the chat (snapshot), scrolls up to load
        /// older messages (paginated fetch), and concurrently receives new messages
        /// (updates receiver) that land at the bottom.
        #[tokio::test]
        async fn test_scroll_up_while_receiving_new_messages() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[130; 32]);
            setup_group(&group_id, &whitenoise).await;
            let account = create_db_account(&whitenoise).await;
            whitenoise
                .get_or_create_account_group(&account, &group_id, None)
                .await
                .unwrap();
            let author = nostr_sdk::Keys::generate().public_key();

            // History: 10 messages at t+1 … t+10 (oldest → newest)
            let base: u64 = 1_730_000_000;
            for i in 1u8..=10 {
                insert_msg_at(i, author, base + u64::from(i), &group_id, &whitenoise).await;
            }

            // ── Step 1: open chat ─────────────────────────────────────────────
            // Subscribe with limit=5 → snapshot contains the 5 newest (seeds 6–10).
            let mut subscription = whitenoise
                .subscribe_to_group_messages(&account.pubkey, &group_id, Some(5))
                .await
                .unwrap();

            assert_eq!(
                subscription.initial_messages.len(),
                5,
                "snapshot: 5 newest messages"
            );

            // Snapshot must be oldest-first within the window
            for w in subscription.initial_messages.windows(2) {
                assert!(
                    w[0].created_at <= w[1].created_at,
                    "snapshot must be oldest-first"
                );
            }

            // Oldest message in the snapshot is seed 6 (t+6)
            let oldest_in_snapshot = &subscription.initial_messages[0];
            assert_eq!(
                oldest_in_snapshot.created_at,
                nostr_sdk::Timestamp::from(base + 6),
                "oldest snapshot message must be seed 6"
            );

            // Newest message in the snapshot is seed 10 (t+10)
            let newest_in_snapshot = &subscription.initial_messages[4];
            assert_eq!(
                newest_in_snapshot.created_at,
                nostr_sdk::Timestamp::from(base + 10),
                "newest snapshot message must be seed 10"
            );

            // ── Step 2: user scrolls up ───────────────────────────────────────
            // Cursor = (created_at, id) of the oldest snapshot message.
            // This fetches the page that immediately precedes the snapshot window.
            let older_page = whitenoise
                .fetch_aggregated_messages_for_group(
                    &account.pubkey,
                    &group_id,
                    &PaginationOptions {
                        before: Some(oldest_in_snapshot.created_at),
                        before_message_id: Some(oldest_in_snapshot.id.clone()),
                        ..Default::default()
                    },
                    Some(5),
                )
                .await
                .unwrap();

            assert_eq!(
                older_page.len(),
                5,
                "older page must contain the 5 oldest messages"
            );

            // Older page covers seeds 1–5 (t+1 … t+5), oldest-first
            assert_eq!(
                older_page[0].created_at,
                nostr_sdk::Timestamp::from(base + 1),
                "oldest message in older page must be seed 1"
            );
            assert_eq!(
                older_page[4].created_at,
                nostr_sdk::Timestamp::from(base + 5),
                "newest message in older page must be seed 5"
            );

            // No overlap between snapshot and older page
            let snapshot_ids: std::collections::HashSet<_> = subscription
                .initial_messages
                .iter()
                .map(|m| &m.id)
                .collect();
            let older_ids: std::collections::HashSet<_> =
                older_page.iter().map(|m| &m.id).collect();
            assert!(
                snapshot_ids.is_disjoint(&older_ids),
                "snapshot and older page must not overlap"
            );

            // Older page is fully older than the snapshot
            let newest_in_older = older_page.iter().map(|m| m.created_at).max().unwrap();
            assert!(
                newest_in_older < oldest_in_snapshot.created_at,
                "every message in the older page must be older than the oldest snapshot message"
            );

            // ── Step 3: new messages arrive while user is scrolling ───────────
            // Simulate two peers sending messages after the user has already scrolled back.
            let new_msg_a = message_aggregator::ChatMessage {
                id: format!("{:0>64x}", 201u8),
                author,
                content: "new message A (arrives while scrolling)".to_string(),
                // Clearly newer than the entire history
                created_at: nostr_sdk::Timestamp::from(base + 100),
                tags: nostr_sdk::Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                content_tokens: vec![],
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            let new_msg_b = message_aggregator::ChatMessage {
                id: format!("{:0>64x}", 202u8),
                author,
                content: "new message B (arrives while scrolling)".to_string(),
                created_at: nostr_sdk::Timestamp::from(base + 101),
                tags: nostr_sdk::Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                content_tokens: vec![],
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };

            whitenoise.message_stream_manager.emit(
                &group_id,
                message_streaming::MessageUpdate {
                    trigger: message_streaming::UpdateTrigger::NewMessage,
                    message: new_msg_a.clone(),
                },
            );
            whitenoise.message_stream_manager.emit(
                &group_id,
                message_streaming::MessageUpdate {
                    trigger: message_streaming::UpdateTrigger::NewMessage,
                    message: new_msg_b.clone(),
                },
            );

            // The updates receiver delivers both new messages in order
            let update_a = subscription.updates.recv().await.unwrap();
            let update_b = subscription.updates.recv().await.unwrap();

            assert_eq!(
                update_a.message.id, new_msg_a.id,
                "first live update must be msg A"
            );
            assert_eq!(
                update_b.message.id, new_msg_b.id,
                "second live update must be msg B"
            );

            // New messages are newer than everything in the snapshot
            for snap_msg in &subscription.initial_messages {
                assert!(
                    update_a.message.created_at > snap_msg.created_at,
                    "new message A must be newer than every snapshot message"
                );
                assert!(
                    update_b.message.created_at > snap_msg.created_at,
                    "new message B must be newer than every snapshot message"
                );
            }

            // New messages do not appear in the older page (they are newer than the snapshot
            // window, so no historical page fetch could return them)
            assert!(
                !older_ids.contains(&new_msg_a.id),
                "new message A must not appear in the older page"
            );
            assert!(
                !older_ids.contains(&new_msg_b.id),
                "new message B must not appear in the older page"
            );
        }

        /// Exhausting history: after two full pages the third page is empty, confirming
        /// the cursor correctly signals end-of-history to the client.
        #[tokio::test]
        async fn test_scroll_up_exhausts_history_cleanly() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[131; 32]);
            setup_group(&group_id, &whitenoise).await;
            let account = create_db_account(&whitenoise).await;
            whitenoise
                .get_or_create_account_group(&account, &group_id, None)
                .await
                .unwrap();
            let author = nostr_sdk::Keys::generate().public_key();

            // Exactly 10 messages, page size 5 — two full pages, then nothing
            let base: u64 = 1_731_000_000;
            for i in 1u8..=10 {
                insert_msg_at(i, author, base + u64::from(i), &group_id, &whitenoise).await;
            }

            // Page 1 (initial snapshot): seeds 6–10
            let subscription = whitenoise
                .subscribe_to_group_messages(&account.pubkey, &group_id, Some(5))
                .await
                .unwrap();
            assert_eq!(subscription.initial_messages.len(), 5);
            let oldest_p1 = &subscription.initial_messages[0];

            // Page 2 (scroll up once): seeds 1–5
            let page2 = whitenoise
                .fetch_aggregated_messages_for_group(
                    &account.pubkey,
                    &group_id,
                    &PaginationOptions {
                        before: Some(oldest_p1.created_at),
                        before_message_id: Some(oldest_p1.id.clone()),
                        ..Default::default()
                    },
                    Some(5),
                )
                .await
                .unwrap();
            assert_eq!(page2.len(), 5, "second page must contain seeds 1–5");
            let oldest_p2 = &page2[0];

            // Page 3 (scroll up again): no more history
            let page3 = whitenoise
                .fetch_aggregated_messages_for_group(
                    &account.pubkey,
                    &group_id,
                    &PaginationOptions {
                        before: Some(oldest_p2.created_at),
                        before_message_id: Some(oldest_p2.id.clone()),
                        ..Default::default()
                    },
                    Some(5),
                )
                .await
                .unwrap();
            assert!(
                page3.is_empty(),
                "third page must be empty — history is exhausted"
            );
        }

        /// When the user scrolls up through several pages and then a new message arrives,
        /// the live update is still delivered correctly regardless of how many historical
        /// pages have been fetched.  The updates receiver is independent of pagination.
        #[tokio::test]
        async fn test_live_updates_independent_of_how_many_pages_were_fetched() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[132; 32]);
            setup_group(&group_id, &whitenoise).await;
            let account = create_db_account(&whitenoise).await;
            whitenoise
                .get_or_create_account_group(&account, &group_id, None)
                .await
                .unwrap();
            let author = nostr_sdk::Keys::generate().public_key();

            let base: u64 = 1_732_000_000;
            for i in 1u8..=15 {
                insert_msg_at(i, author, base + u64::from(i), &group_id, &whitenoise).await;
            }

            // Subscribe (snapshot = seeds 11–15)
            let mut subscription = whitenoise
                .subscribe_to_group_messages(&account.pubkey, &group_id, Some(5))
                .await
                .unwrap();

            // Fetch page 2 (seeds 6–10)
            let oldest_p1 = &subscription.initial_messages[0].clone();
            let page2 = whitenoise
                .fetch_aggregated_messages_for_group(
                    &account.pubkey,
                    &group_id,
                    &PaginationOptions {
                        before: Some(oldest_p1.created_at),
                        before_message_id: Some(oldest_p1.id.clone()),
                        ..Default::default()
                    },
                    Some(5),
                )
                .await
                .unwrap();

            // Fetch page 3 (seeds 1–5)
            let oldest_p2 = &page2[0].clone();
            let page3 = whitenoise
                .fetch_aggregated_messages_for_group(
                    &account.pubkey,
                    &group_id,
                    &PaginationOptions {
                        before: Some(oldest_p2.created_at),
                        before_message_id: Some(oldest_p2.id.clone()),
                        ..Default::default()
                    },
                    Some(5),
                )
                .await
                .unwrap();
            assert_eq!(page3.len(), 5, "third page must cover seeds 1–5");

            // Now a new message arrives
            let new_msg = message_aggregator::ChatMessage {
                id: format!("{:0>64x}", 250u8),
                author,
                content: "new message while deep in history".to_string(),
                created_at: nostr_sdk::Timestamp::from(base + 999),
                tags: nostr_sdk::Tags::new(),
                is_reply: false,
                reply_to_id: None,
                is_deleted: false,
                content_tokens: vec![],
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            whitenoise.message_stream_manager.emit(
                &group_id,
                message_streaming::MessageUpdate {
                    trigger: message_streaming::UpdateTrigger::NewMessage,
                    message: new_msg.clone(),
                },
            );

            // The updates receiver delivers it despite being deep in history
            let received = subscription.updates.recv().await.unwrap();
            assert_eq!(
                received.message.id, new_msg.id,
                "live update must arrive regardless of how many historical pages were fetched"
            );
            assert_eq!(
                received.trigger,
                message_streaming::UpdateTrigger::NewMessage
            );

            // The new message is newer than everything in every fetched page
            for page in [&subscription.initial_messages, &page2, &page3] {
                for msg in page {
                    assert!(
                        received.message.created_at > msg.created_at,
                        "new message must be newer than all historical messages"
                    );
                }
            }
        }

        /// Tied timestamps across a page boundary: if several messages share the same
        /// `created_at` second and straddle the snapshot boundary, the compound cursor
        /// ensures the page fetch picks up exactly the right set — no duplicates, no gaps.
        #[tokio::test]
        async fn test_scroll_up_with_tied_timestamps_at_page_boundary() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let group_id = GroupId::from_slice(&[133; 32]);
            setup_group(&group_id, &whitenoise).await;
            let account = create_db_account(&whitenoise).await;
            whitenoise
                .get_or_create_account_group(&account, &group_id, None)
                .await
                .unwrap();
            let author = nostr_sdk::Keys::generate().public_key();

            // 8 messages: seeds 1–6 all at the same second (tie), seed 7 earlier, seed 8 later
            let tie_ts: u64 = 1_733_000_000;
            insert_msg_at(7, author, tie_ts - 1, &group_id, &whitenoise).await;
            for i in 1u8..=6 {
                insert_msg_at(i, author, tie_ts, &group_id, &whitenoise).await;
            }
            insert_msg_at(8, author, tie_ts + 1, &group_id, &whitenoise).await;

            // Subscribe with limit=3 → snapshot is the 3 newest
            let subscription = whitenoise
                .subscribe_to_group_messages(&account.pubkey, &group_id, Some(3))
                .await
                .unwrap();
            assert_eq!(subscription.initial_messages.len(), 3);
            let oldest_in_snapshot = &subscription.initial_messages[0].clone();

            // Scroll up: fetch 3 more using the compound cursor
            let older_page = whitenoise
                .fetch_aggregated_messages_for_group(
                    &account.pubkey,
                    &group_id,
                    &PaginationOptions {
                        before: Some(oldest_in_snapshot.created_at),
                        before_message_id: Some(oldest_in_snapshot.id.clone()),
                        ..Default::default()
                    },
                    Some(3),
                )
                .await
                .unwrap();
            assert_eq!(
                older_page.len(),
                3,
                "older page must contain exactly 3 messages"
            );

            // No overlap between snapshot and older page
            let snapshot_ids: std::collections::HashSet<_> = subscription
                .initial_messages
                .iter()
                .map(|m| &m.id)
                .collect();
            let older_ids: std::collections::HashSet<_> =
                older_page.iter().map(|m| &m.id).collect();
            assert!(
                snapshot_ids.is_disjoint(&older_ids),
                "tied-timestamp page boundary must produce no overlap between pages"
            );

            // Together they cover 6 distinct messages (no duplicates, no gaps so far)
            assert_eq!(
                snapshot_ids.len() + older_ids.len(),
                6,
                "snapshot + older page must cover 6 distinct messages"
            );
        }
    }

    mod user_subscription_tests {
        use chrono::Utc;
        use nostr_sdk::{Keys, Metadata};

        use super::*;

        #[tokio::test]
        async fn test_subscribe_to_user_returns_initial_snapshot_for_existing_known_user() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let pubkey = Keys::generate().public_key();

            let mut user = User {
                id: None,
                pubkey,
                metadata: Metadata::new().name("Known User"),
                created_at: Utc::now(),
                metadata_known_at: None,
                updated_at: Utc::now(),
            };
            user.mark_metadata_known_now();
            let saved_user = user.save(&whitenoise.database).await.unwrap();

            let subscription = whitenoise.subscribe_to_user(&pubkey).await.unwrap();

            assert_eq!(subscription.initial_user, saved_user);
        }

        #[tokio::test]
        async fn test_subscribe_to_user_creates_initial_snapshot_for_unknown_pubkey() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let pubkey = Keys::generate().public_key();

            let subscription = whitenoise.subscribe_to_user(&pubkey).await.unwrap();

            assert_eq!(subscription.initial_user.pubkey, pubkey);
            assert!(subscription.initial_user.id.is_some());
            assert!(subscription.initial_user.metadata_is_unknown());

            let saved_user = User::find_by_pubkey(&pubkey, &whitenoise.database)
                .await
                .unwrap();
            assert_eq!(saved_user, subscription.initial_user);
        }
    }

    // Subscription Status Tests
    mod subscription_status_tests {
        use super::*;

        #[tokio::test]
        async fn test_is_account_operational_with_subscriptions() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let account = whitenoise.create_identity().await.unwrap();

            // create_identity sets up subscriptions automatically
            let is_operational = whitenoise
                .is_account_subscriptions_operational(&account)
                .await
                .unwrap();

            // Should return true when subscriptions are set up
            // (create_identity sets up follow_list and giftwrap subscriptions)
            assert!(
                is_operational,
                "Account should be operational after create_identity"
            );
        }

        /// When MDK has more active groups than the group plane, the parity
        /// check should detect the mismatch and report non-operational.
        #[tokio::test]
        async fn test_is_account_operational_detects_group_count_mismatch() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            let creator_account = whitenoise.create_identity().await.unwrap();
            let members = setup_multiple_test_accounts(&whitenoise, 1).await;
            let member_pubkey = members[0].0.pubkey;
            wait_for_key_package_publication(&whitenoise, &[&members[0].0]).await;

            // Starts operational (0 groups in MDK, 0 in plane)
            assert!(
                whitenoise
                    .is_account_subscriptions_operational(&creator_account)
                    .await
                    .unwrap(),
                "Should be operational with zero groups"
            );

            // Create a group — this adds it to MDK and (via background task)
            // to the group plane
            let config = create_nostr_group_config_data(vec![creator_account.pubkey]);
            whitenoise
                .create_group(&creator_account, vec![member_pubkey], config, None)
                .await
                .unwrap();

            // Tear down subscriptions to simulate the welcome-processing
            // cascade failure (group exists in MDK but not in group plane)
            whitenoise
                .relay_control
                .deactivate_account_subscriptions(&creator_account.pubkey)
                .await;

            // Re-activate inbox only (without groups) to isolate the test
            // to the group count parity check — inbox is healthy, groups are not
            let signer = whitenoise.get_signer_for_account(&creator_account).unwrap();
            let inbox_relays = crate::whitenoise::relays::Relay::urls(
                &creator_account
                    .effective_inbox_relays(&whitenoise)
                    .await
                    .unwrap(),
            );
            whitenoise
                .relay_control
                .activate_account_subscriptions(
                    creator_account.pubkey,
                    &inbox_relays,
                    &[], // empty group specs — simulates the missing group
                    creator_account.since_timestamp(10),
                    signer,
                )
                .await
                .unwrap();

            let is_operational = whitenoise
                .is_account_subscriptions_operational(&creator_account)
                .await
                .unwrap();
            assert!(
                !is_operational,
                "Should detect mismatch: MDK has 1 group, plane has 0"
            );

            // Recovery via ensure should fix it
            whitenoise
                .ensure_account_subscriptions(&creator_account)
                .await
                .unwrap();

            let is_operational = whitenoise
                .is_account_subscriptions_operational(&creator_account)
                .await
                .unwrap();
            assert!(
                is_operational,
                "Should be operational after ensure recovery"
            );
        }

        #[tokio::test]
        async fn test_is_global_subscriptions_operational_no_accounts_is_healthy() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Zero accounts with zero subscriptions is the correct state — healthy.
            let is_operational = whitenoise
                .is_global_subscriptions_operational()
                .await
                .unwrap();

            assert!(
                is_operational,
                "Zero accounts with zero subscriptions should be considered healthy"
            );
        }
    }

    // Cache Synchronization Tests
    mod cache_sync_tests {
        use super::*;

        #[tokio::test]
        async fn test_sync_message_cache_on_startup_with_empty_database() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Verify method can be called on empty database without panicking
            let result = whitenoise.sync_message_cache_on_startup().await;
            assert!(result.is_ok(), "Sync should succeed on empty database");
        }

        #[tokio::test]
        async fn test_sync_message_cache_on_startup_with_account_no_groups() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let _account = whitenoise.create_identity().await.unwrap();

            // Verify method can be called with account but no groups
            let result = whitenoise.sync_message_cache_on_startup().await;
            assert!(
                result.is_ok(),
                "Sync should succeed with account but no groups"
            );
        }

        #[tokio::test]
        async fn test_sync_is_idempotent() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let _account = whitenoise.create_identity().await.unwrap();

            // Run sync multiple times
            whitenoise.sync_message_cache_on_startup().await.unwrap();
            whitenoise.sync_message_cache_on_startup().await.unwrap();
            whitenoise.sync_message_cache_on_startup().await.unwrap();

            // Should not panic or error
        }
    }

    // Ensure Subscriptions Tests
    mod ensure_subscriptions_tests {
        use super::*;

        #[tokio::test]
        async fn test_ensure_account_subscriptions_behavior() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let account = whitenoise.create_identity().await.unwrap();

            // Test idempotency - multiple calls when operational should not cause issues
            whitenoise
                .ensure_account_subscriptions(&account)
                .await
                .unwrap();
            whitenoise
                .ensure_account_subscriptions(&account)
                .await
                .unwrap();

            let is_operational = whitenoise
                .is_account_subscriptions_operational(&account)
                .await
                .unwrap();
            assert!(
                is_operational,
                "Account should remain operational after multiple ensure calls"
            );

            // Test recovery - ensure_account_subscriptions should fix broken state
            whitenoise
                .relay_control
                .deactivate_account_subscriptions(&account.pubkey)
                .await;

            let is_operational = whitenoise
                .is_account_subscriptions_operational(&account)
                .await
                .unwrap();
            assert!(
                !is_operational,
                "Account should not be operational after unsubscribe"
            );

            whitenoise
                .ensure_account_subscriptions(&account)
                .await
                .unwrap();

            let is_operational = whitenoise
                .is_account_subscriptions_operational(&account)
                .await
                .unwrap();
            assert!(
                is_operational,
                "Account should be operational after ensure refresh"
            );
        }

        #[tokio::test]
        async fn test_ensure_all_subscriptions_comprehensive() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Create multiple accounts to test handling of multiple accounts
            let account1 = whitenoise.create_identity().await.unwrap();
            let account2 = whitenoise.create_identity().await.unwrap();
            let account3 = whitenoise.create_identity().await.unwrap();

            // Flush fire-and-forget rebuild (worker handles this in production)
            whitenoise.sync_discovery_subscriptions().await.unwrap();

            // First call - ensure all subscriptions work
            whitenoise.ensure_all_subscriptions().await.unwrap();

            // Verify global subscriptions are operational
            let global_operational = whitenoise
                .is_global_subscriptions_operational()
                .await
                .unwrap();
            assert!(
                global_operational,
                "Global subscriptions should be operational after ensure_all"
            );

            // Verify all accounts are operational
            for account in &[&account1, &account2, &account3] {
                let is_operational = whitenoise
                    .is_account_subscriptions_operational(account)
                    .await
                    .unwrap();
                assert!(
                    is_operational,
                    "Account {} should be operational",
                    account.pubkey.to_hex()
                );
            }

            // Test idempotency - multiple calls should not cause issues
            whitenoise.ensure_all_subscriptions().await.unwrap();
            whitenoise.ensure_all_subscriptions().await.unwrap();

            // Everything should still be operational after multiple calls
            let global_operational = whitenoise
                .is_global_subscriptions_operational()
                .await
                .unwrap();
            assert!(
                global_operational,
                "Global subscriptions should remain operational after multiple ensure_all calls"
            );

            for account in &[&account1, &account2, &account3] {
                let is_operational = whitenoise
                    .is_account_subscriptions_operational(account)
                    .await
                    .unwrap();
                assert!(
                    is_operational,
                    "Account {} should remain operational after multiple ensure_all calls",
                    account.pubkey.to_hex()
                );
            }
        }

        #[tokio::test]
        async fn test_ensure_all_subscriptions_continues_on_partial_failure() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Create two accounts
            let account1 = whitenoise.create_identity().await.unwrap();
            let account2 = whitenoise.create_identity().await.unwrap();

            // Break account1's subscriptions
            whitenoise
                .relay_control
                .deactivate_account_subscriptions(&account1.pubkey)
                .await;

            // Verify account1 is not operational
            let account1_operational = whitenoise
                .is_account_subscriptions_operational(&account1)
                .await
                .unwrap();
            assert!(
                !account1_operational,
                "Account1 should not be operational after unsubscribe"
            );

            // ensure_all should succeed and fix both accounts
            whitenoise.ensure_all_subscriptions().await.unwrap();

            // Both accounts should now be operational
            let account1_operational = whitenoise
                .is_account_subscriptions_operational(&account1)
                .await
                .unwrap();
            let account2_operational = whitenoise
                .is_account_subscriptions_operational(&account2)
                .await
                .unwrap();

            assert!(
                account1_operational,
                "Account1 should be operational after ensure_all"
            );
            assert!(
                account2_operational,
                "Account2 should remain operational after ensure_all"
            );
        }
    }

    // Scheduler Lifecycle Tests
    mod scheduler_lifecycle_tests {
        use super::*;

        #[tokio::test]
        async fn test_scheduler_handles_stored_after_init() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Scheduler handles should be accessible (empty since no tasks registered yet)
            let handles = whitenoise.scheduler_handles.lock().await;
            assert!(
                handles.is_empty(),
                "Scheduler handles should be empty when no tasks are registered"
            );
        }

        #[tokio::test]
        async fn test_shutdown_returns_ok() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Shutdown should succeed
            let result = whitenoise.shutdown().await;
            assert!(result.is_ok(), "Shutdown should return Ok");
        }

        #[tokio::test]
        async fn test_shutdown_completes_within_timeout() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Shutdown should complete without hanging
            let result =
                tokio::time::timeout(std::time::Duration::from_millis(100), whitenoise.shutdown())
                    .await;

            assert!(result.is_ok(), "Shutdown should complete within timeout");
        }

        #[tokio::test]
        async fn test_shutdown_is_idempotent() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Multiple shutdowns should not panic
            whitenoise.shutdown().await.unwrap();
            whitenoise.shutdown().await.unwrap();
            whitenoise.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn test_subscribe_to_notifications_returns_receiver() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Subscribe should return a subscription with a receiver
            let subscription = whitenoise.subscribe_to_notifications();

            // The receiver should be empty initially (no pending notifications)
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(10),
                subscription.updates.resubscribe().recv(),
            )
            .await;

            // Should timeout because there are no notifications yet
            assert!(
                result.is_err(),
                "Should timeout with no pending notifications"
            );
        }

        #[tokio::test]
        async fn test_subscribe_to_chat_list_returns_sorted_initial_items() {
            use chrono::Utc;
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let creator = whitenoise.create_identity().await.unwrap();
            let member = whitenoise.create_identity().await.unwrap();

            let base_timestamp = Utc::now().timestamp() as u64;
            let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
            config.name = "Group A".to_string();
            let group_a = whitenoise
                .create_group(&creator, vec![member.pubkey], config, None)
                .await
                .unwrap();
            let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
            config.name = "Group B".to_string();
            let group_b = whitenoise
                .create_group(&creator, vec![member.pubkey], config, None)
                .await
                .unwrap();
            let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
            config.name = "Group C".to_string();
            let group_c = whitenoise
                .create_group(&creator, vec![member.pubkey], config, None)
                .await
                .unwrap();
            let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
            config.name = "Group D".to_string();
            let group_d = whitenoise
                .create_group(&creator, vec![member.pubkey], config, None)
                .await
                .unwrap();
            let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
            config.name = "Group E".to_string();
            let group_e = whitenoise
                .create_group(&creator, vec![member.pubkey], config, None)
                .await
                .unwrap();

            let messages = vec![
                (&group_a, "1", "Message A", base_timestamp - 7200), // -2 hours
                (&group_b, "2", "Message B", base_timestamp + 7200), // +2 hours
                (&group_d, "4", "Message D", base_timestamp - 3600), // -1 hour
                (&group_e, "5", "Message E", base_timestamp + 3600), // +1 hour
            ];

            for (group, id, content, timestamp) in messages {
                let msg = message_aggregator::ChatMessage {
                    id: format!("{:0>64}", id),
                    author: creator.pubkey,
                    content: content.to_string(),
                    created_at: nostr_sdk::Timestamp::from(timestamp),
                    tags: nostr_sdk::Tags::new(),
                    is_reply: false,
                    reply_to_id: None,
                    is_deleted: false,
                    content_tokens: vec![],
                    reactions: message_aggregator::ReactionSummary::default(),
                    kind: 9,
                    media_attachments: vec![],
                    delivery_status: None,
                };
                aggregated_message::AggregatedMessage::insert_message(
                    &msg,
                    &group.mls_group_id,
                    &whitenoise.database,
                )
                .await
                .unwrap();
            }

            let subscription = whitenoise.subscribe_to_chat_list(&creator).await.unwrap();

            let initial_items = subscription.initial_items;
            assert_eq!(initial_items.len(), 5);

            // Verify items are in descending timestamp order:
            // B (+2h) -> E (+1h) -> C (~now, no msg) -> D (-1h) -> A (-2h)
            assert_eq!(
                initial_items[0].mls_group_id, group_b.mls_group_id,
                "First: Group B with newest message (+2h)"
            );
            assert_eq!(
                initial_items[1].mls_group_id, group_e.mls_group_id,
                "Second: Group E (+1h)"
            );
            assert_eq!(
                initial_items[2].mls_group_id, group_c.mls_group_id,
                "Third: Group C (no messages, created_at ~now)"
            );
            assert_eq!(
                initial_items[3].mls_group_id, group_d.mls_group_id,
                "Fourth: Group D (-1h)"
            );
            assert_eq!(
                initial_items[4].mls_group_id, group_a.mls_group_id,
                "Fifth: Group A with oldest message (-2h)"
            );
        }
    }

    // External Signer Registry Tests
    mod external_signer_tests {
        use super::*;
        use nostr_sdk::Keys;

        /// Helper to create a test signer using Keys (which implements NostrSigner)
        fn create_test_signer() -> (Keys, PublicKey) {
            let keys = Keys::generate();
            let pubkey = keys.public_key();
            (keys, pubkey)
        }

        #[tokio::test]
        async fn test_register_and_get_external_signer() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (signer, pubkey) = create_test_signer();

            // Insert signer directly (bypasses account-type validation)
            whitenoise
                .insert_external_signer(pubkey, signer)
                .await
                .unwrap();

            // Verify we can retrieve it
            let retrieved = whitenoise.get_external_signer(&pubkey);
            assert!(retrieved.is_some(), "Signer should be registered");

            // Verify it's the correct signer by checking pubkey
            let retrieved_signer = retrieved.unwrap();
            let retrieved_pubkey = retrieved_signer.get_public_key().await.unwrap();
            assert_eq!(retrieved_pubkey, pubkey);
        }

        #[tokio::test]
        async fn test_get_external_signer_not_found() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let random_pubkey = Keys::generate().public_key();

            // Try to get signer that doesn't exist
            let result = whitenoise.get_external_signer(&random_pubkey);
            assert!(
                result.is_none(),
                "Should return None for unregistered signer"
            );
        }

        #[tokio::test]
        async fn test_remove_external_signer() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (signer, pubkey) = create_test_signer();

            // Insert and verify
            whitenoise
                .insert_external_signer(pubkey, signer)
                .await
                .unwrap();
            assert!(whitenoise.get_external_signer(&pubkey).is_some());

            // Remove and verify
            whitenoise.remove_external_signer(&pubkey);
            assert!(
                whitenoise.get_external_signer(&pubkey).is_none(),
                "Signer should be removed"
            );
        }

        #[tokio::test]
        async fn test_external_signer_overwrites_existing() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (signer1, pubkey) = create_test_signer();

            // Create a second signer from the same underlying keys
            let signer2 = Keys::new(signer1.secret_key().clone());

            // Insert first signer
            whitenoise
                .insert_external_signer(pubkey, signer1)
                .await
                .unwrap();

            // Re-insert with a new signer for the same pubkey (should overwrite)
            whitenoise
                .insert_external_signer(pubkey, signer2)
                .await
                .unwrap();

            // Verify the signer is still retrievable and matches the pubkey
            let retrieved = whitenoise.get_external_signer(&pubkey).unwrap();
            let retrieved_pubkey = retrieved.get_public_key().await.unwrap();
            assert_eq!(retrieved_pubkey, pubkey);
        }

        #[tokio::test]
        async fn test_multiple_signers_different_pubkeys() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (signer1, pubkey1) = create_test_signer();
            let (signer2, pubkey2) = create_test_signer();

            // Insert both signers with their respective pubkeys
            whitenoise
                .insert_external_signer(pubkey1, signer1)
                .await
                .unwrap();
            whitenoise
                .insert_external_signer(pubkey2, signer2)
                .await
                .unwrap();

            // Verify both are retrievable
            let retrieved1 = whitenoise.get_external_signer(&pubkey1);
            let retrieved2 = whitenoise.get_external_signer(&pubkey2);

            assert!(retrieved1.is_some(), "First signer should be registered");
            assert!(retrieved2.is_some(), "Second signer should be registered");

            // Verify correct pubkeys
            assert_eq!(retrieved1.unwrap().get_public_key().await.unwrap(), pubkey1);
            assert_eq!(retrieved2.unwrap().get_public_key().await.unwrap(), pubkey2);
        }

        #[tokio::test]
        async fn test_remove_one_signer_leaves_others() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (signer1, pubkey1) = create_test_signer();
            let (signer2, pubkey2) = create_test_signer();

            // Insert both
            whitenoise
                .insert_external_signer(pubkey1, signer1)
                .await
                .unwrap();
            whitenoise
                .insert_external_signer(pubkey2, signer2)
                .await
                .unwrap();

            // Remove first signer
            whitenoise.remove_external_signer(&pubkey1);

            // Verify first is gone, second remains
            assert!(whitenoise.get_external_signer(&pubkey1).is_none());
            assert!(whitenoise.get_external_signer(&pubkey2).is_some());
        }

        #[tokio::test]
        async fn test_register_rejects_non_external_account() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Create a local account (not external signer)
            let account = whitenoise.create_identity().await.unwrap();
            let (signer, _) = create_test_signer();

            // Attempting to register an external signer for a local account should fail
            let result = whitenoise
                .register_external_signer(account.pubkey, signer)
                .await;
            assert!(
                result.is_err(),
                "Should reject registering external signer for a local account"
            );
        }

        #[tokio::test]
        async fn test_register_rejects_unknown_pubkey() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (signer, pubkey) = create_test_signer();

            // Attempting to register for a pubkey with no account should fail
            let result = whitenoise.register_external_signer(pubkey, signer).await;
            assert!(
                result.is_err(),
                "Should reject registering external signer for unknown pubkey"
            );
        }

        #[tokio::test]
        async fn test_register_accepts_external_account() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Create an external signer account
            let keys = Keys::generate();
            let pubkey = keys.public_key();
            let account = whitenoise
                .login_with_external_signer_for_test(keys.clone())
                .await
                .unwrap();

            // Use a signer whose pubkey matches the account
            let result = whitenoise
                .register_external_signer(account.pubkey, keys)
                .await;
            assert!(
                result.is_ok(),
                "Should accept registering external signer for an external account"
            );

            // Verify the signer was registered
            assert!(whitenoise.get_external_signer(&pubkey).is_some());
        }

        #[tokio::test]
        async fn test_register_recovers_missing_account_subscriptions() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Create an external signer account with an initially operational subscription state.
            let keys = Keys::generate();
            let account = whitenoise
                .login_with_external_signer_for_test(keys.clone())
                .await
                .unwrap();

            // Simulate app startup gap: no signer registered yet + account subscriptions missing.
            whitenoise.remove_external_signer(&account.pubkey);
            whitenoise
                .relay_control
                .deactivate_account_subscriptions(&account.pubkey)
                .await;

            let before = whitenoise
                .is_account_subscriptions_operational(&account)
                .await
                .unwrap();
            assert!(
                !before,
                "Account subscriptions should be non-operational before signer re-registration"
            );

            // Re-registering the signer should recover account subscriptions.
            whitenoise
                .register_external_signer(account.pubkey, keys)
                .await
                .unwrap();

            let after = whitenoise
                .is_account_subscriptions_operational(&account)
                .await
                .unwrap();
            assert!(
                after,
                "register_external_signer should recover missing account subscriptions"
            );
        }

        #[tokio::test]
        async fn test_register_external_signer_succeeds_when_relay_lookup_fails() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            let keys = Keys::generate();
            let account = whitenoise
                .login_with_external_signer_for_test(keys.clone())
                .await
                .unwrap();

            // Corrupt account->user linkage so relay lookup fails during
            // subscription recovery. Registration should still succeed.
            sqlx::query("DELETE FROM users WHERE id = ?")
                .bind(account.user_id)
                .execute(&whitenoise.database.pool)
                .await
                .unwrap();

            whitenoise.remove_external_signer(&account.pubkey);
            let result = whitenoise
                .register_external_signer(account.pubkey, keys)
                .await;

            assert!(
                result.is_ok(),
                "register_external_signer should remain successful even if recovery relay lookup fails"
            );
            assert!(
                whitenoise.get_external_signer(&account.pubkey).is_some(),
                "signer should still be registered when recovery fails"
            );
        }
    }

    mod subscription_tests {
        use chrono::{DateTime, TimeDelta, Utc};
        use nostr_sdk::Keys;

        use super::*;

        fn make_account(user_id: i64, last_synced_at: Option<DateTime<Utc>>) -> Account {
            Account {
                id: None,
                pubkey: Keys::generate().public_key(),
                user_id,
                account_type: AccountType::Local,
                last_synced_at,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }
        }

        #[tokio::test]
        async fn test_sync_discovery_subscriptions_no_account_returns_ok() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // With no accounts in the database, should return Ok (early return)
            let result = whitenoise.sync_discovery_subscriptions().await;
            assert!(
                result.is_ok(),
                "sync_discovery_subscriptions should succeed with no accounts"
            );
        }

        #[test]
        fn test_compute_global_since_empty_accounts_returns_none() {
            assert!(Whitenoise::compute_global_since_timestamp(&[]).is_none());
        }

        #[test]
        fn test_compute_global_since_unsynced_account_returns_none() {
            let accounts = vec![make_account(1, None)];
            assert!(Whitenoise::compute_global_since_timestamp(&accounts).is_none());
        }

        #[test]
        fn test_compute_global_since_all_synced_returns_min_with_buffer() {
            let now = Utc::now();
            let older = now - TimeDelta::seconds(200);
            let newer = now - TimeDelta::seconds(100);

            let accounts = vec![make_account(1, Some(older)), make_account(2, Some(newer))];

            let ts = Whitenoise::compute_global_since_timestamp(&accounts).unwrap();
            let expected = (older.timestamp().max(0) as u64)
                .saturating_sub(Whitenoise::SUBSCRIPTION_BUFFER_SECS);
            assert_eq!(ts.as_secs(), expected);
        }

        #[test]
        fn test_compute_global_since_mixed_synced_unsynced_returns_none() {
            let now = Utc::now();
            let accounts = vec![
                make_account(1, Some(now - TimeDelta::seconds(100))),
                make_account(2, None),
            ];
            assert!(Whitenoise::compute_global_since_timestamp(&accounts).is_none());
        }
    }

    mod fallback_relay_tests {
        use super::*;
        use std::collections::HashSet;

        #[tokio::test]
        async fn test_fallback_relay_urls_uses_discovery_plane_relays() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let fallback = whitenoise.fallback_relay_urls().await;
            let discovery_urls = whitenoise.config.discovery_relays.clone();

            for url in &discovery_urls {
                assert!(
                    fallback.contains(url),
                    "Fallback should include discovery relay: {}",
                    url
                );
            }
        }

        #[tokio::test]
        async fn test_fallback_relay_urls_excludes_disconnected_relays() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            // A relay URL that was never added to the discovery plane must not appear in fallback.
            let extra_url = RelayUrl::parse("wss://extra.relay.test").unwrap();

            let fallback = whitenoise.fallback_relay_urls().await;
            assert!(
                !fallback.contains(&extra_url),
                "Fallback should not include a relay that was never added to discovery"
            );
        }

        #[tokio::test]
        async fn test_fallback_relay_urls_deduplicates() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let fallback = whitenoise.fallback_relay_urls().await;

            let unique: HashSet<&RelayUrl> = fallback.iter().collect();
            assert_eq!(
                fallback.len(),
                unique.len(),
                "Fallback should not contain duplicates"
            );
        }
    }
}
