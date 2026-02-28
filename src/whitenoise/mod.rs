use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::Context;
use dashmap::DashMap;
use nostr_sdk::prelude::NostrSigner;
use nostr_sdk::{PublicKey, RelayUrl, ToBech32};
use tokio::sync::{
    Mutex, OnceCell, Semaphore, broadcast,
    mpsc::{self, Sender},
    watch,
};
use tokio::task::JoinHandle;

pub mod account_settings;
pub mod accounts;
pub mod accounts_groups;
pub mod aggregated_message;
pub mod app_settings;
pub(crate) mod cached_graph_user;
pub mod chat_list;
pub mod chat_list_streaming;
pub mod database;
pub mod drafts;
pub mod error;
mod event_processor;
pub mod event_tracker;
pub mod follows;
pub mod group_information;
pub mod groups;
pub mod key_packages;
pub mod media_files;
pub mod message_aggregator;
pub mod message_streaming;
pub mod messages;
pub mod notification_streaming;
pub mod relays;
pub mod scheduled_tasks;
pub mod secrets_store;
pub mod storage;
pub mod user_search;
pub mod users;
pub mod utils;

use mdk_core::prelude::MDK;
use mdk_sqlite_storage::MdkSqliteStorage;

use crate::init_tracing;
use crate::nostr_manager::NostrManager;

use crate::types::ProcessableEvent;

use accounts::*;
use app_settings::*;
use database::*;
use error::{Result, WhitenoiseError};
use event_tracker::WhitenoiseEventTracker;
use relays::*;
use secrets_store::SecretsStore;
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
        }
    }
}

pub struct Whitenoise {
    pub config: WhitenoiseConfig,
    database: Arc<Database>,
    nostr: NostrManager,
    secrets_store: SecretsStore,
    storage: storage::Storage,
    message_aggregator: message_aggregator::MessageAggregator,
    message_stream_manager: Arc<message_streaming::MessageStreamManager>,
    chat_list_stream_manager: chat_list_streaming::ChatListStreamManager,
    notification_stream_manager: notification_streaming::NotificationStreamManager,
    event_sender: Sender<ProcessableEvent>,
    shutdown_sender: Sender<()>,
    /// Per-account concurrency guards to prevent race conditions in contact list processing
    contact_list_guards: DashMap<PublicKey, Arc<Semaphore>>,
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
}

static GLOBAL_WHITENOISE: OnceCell<Whitenoise> = OnceCell::const_new();

impl std::fmt::Debug for Whitenoise {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Whitenoise")
            .field("config", &self.config)
            .field("database", &"<REDACTED>")
            .field("nostr", &"<REDACTED>")
            .field("secrets_store", &"<REDACTED>")
            .field("storage", &"<REDACTED>")
            .field("message_aggregator", &"<REDACTED>")
            .field("message_stream_manager", &"<REDACTED>")
            .field("chat_list_stream_manager", &"<REDACTED>")
            .field("notification_stream_manager", &"<REDACTED>")
            .field("event_sender", &"<REDACTED>")
            .field("shutdown_sender", &"<REDACTED>")
            .field("contact_list_guards", &"<REDACTED>")
            .field("scheduler_shutdown", &"<REDACTED>")
            .field("scheduler_handles", &"<REDACTED>")
            .finish()
    }
}

impl Whitenoise {
    /// Initializes the keyring-core credential store.
    ///
    /// `mdk-sqlite-storage` and [`SecretsStore`] both use `keyring-core` to store
    /// secret key material.  Unlike the `keyring` crate (v3), `keyring-core` does
    /// **not** auto-detect a platform backend — callers must register one via
    /// `keyring_core::set_default_store()` before any `Entry` operations.
    ///
    /// In test and integration-test builds the mock (in-memory) store is used so
    /// that unit tests never touch the real platform keychain.
    ///
    /// This function is safe to call multiple times; only the first call has
    /// an effect.
    fn initialize_keyring_store() {
        static KEYRING_STORE_INIT: OnceLock<()> = OnceLock::new();
        KEYRING_STORE_INIT.get_or_init(|| {
            // In test / integration-test builds, always use the mock store so
            // tests never interact with the real platform keychain.
            #[cfg(any(test, feature = "integration-tests"))]
            {
                keyring_core::set_default_store(
                    keyring_core::mock::Store::new()
                        .expect("Failed to create mock credential store"),
                );
            }

            #[cfg(not(any(test, feature = "integration-tests")))]
            {
                #[cfg(target_os = "macos")]
                {
                    let store = apple_native_keyring_store::keychain::Store::new()
                        .expect("Failed to create macOS Keychain credential store");
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
    pub async fn initialize_whitenoise(config: WhitenoiseConfig) -> Result<()> {
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
        let (event_sender, event_receiver) = mpsc::channel(500);
        let (shutdown_sender, shutdown_receiver) = mpsc::channel(1);

        // Create scheduler shutdown channel
        let (scheduler_shutdown, scheduler_shutdown_rx) = watch::channel(false);

        let whitenoise_res: Result<&'static Whitenoise> = GLOBAL_WHITENOISE.get_or_try_init(|| async {
        let data_dir = &config.data_dir;
        let logs_dir = &config.logs_dir;

        // Setup directories
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("Failed to create data directory: {:?}", data_dir))
            .map_err(WhitenoiseError::from)?;
        std::fs::create_dir_all(logs_dir)
            .with_context(|| format!("Failed to create logs directory: {:?}", logs_dir))
            .map_err(WhitenoiseError::from)?;

        // Only initialize tracing once
        init_tracing(logs_dir);

        tracing::debug!(target: "whitenoise::initialize_whitenoise", "Logging initialized in directory: {:?}", logs_dir);

        let database = Arc::new(Database::new(data_dir.join("whitenoise.sqlite")).await?);

        // Create NostrManager with event_sender for direct event queuing
        let nostr =
            NostrManager::new(event_sender.clone(), Arc::new(WhitenoiseEventTracker::new(database.clone())), NostrManager::default_timeout())
                .await?;

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

        let whitenoise = Self {
            config,
            database,
            nostr,
            secrets_store,
            storage,
            message_aggregator,
            message_stream_manager: Arc::new(message_streaming::MessageStreamManager::default()),
            chat_list_stream_manager: chat_list_streaming::ChatListStreamManager::default(),
            notification_stream_manager: notification_streaming::NotificationStreamManager::default(),
            event_sender,
            shutdown_sender,
            contact_list_guards: DashMap::new(),
            scheduler_shutdown,
            scheduler_handles: Mutex::new(Vec::new()),
            external_signers: DashMap::new(),
            background_task_cancellation: DashMap::new(),
            pending_logins: DashMap::new(),
        };

        // Create default relays in the database if they don't exist
        // TODO: Make this batch fetch and insert all relays at once
        for relay in Relay::defaults() {
            let _ = whitenoise.find_or_create_relay_by_url(&relay.url).await?;
        }

        // Create default app settings in the database if they don't exist
        AppSettings::find_or_create_default(&whitenoise.database).await?;

        // Add default relays to the Nostr client if they aren't already added
        if whitenoise.nostr.client.relays().await.is_empty() {
            // First time starting the app
            for relay in Relay::defaults() {
                whitenoise.nostr.client.add_relay(relay.url).await?;
            }
        }

        // No need to wait for all the relays to be up
        tokio::spawn({
            let client = whitenoise.nostr.client.clone();
            async move {
                client.connect().await;
            }
        });
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
        ];
        let scheduler_handles = scheduled_tasks::start_scheduled_tasks(
            whitenoise_ref,
            scheduler_shutdown_rx,
            None,
            tasks,
        );
        *whitenoise_ref.scheduler_handles.lock().await = scheduler_handles;

        // Fetch events and setup subscriptions after event processing has started
        Self::setup_all_subscriptions(whitenoise_ref).await?;

        tracing::debug!(
            target: "whitenoise::initialize_whitenoise",
            "Completed initialization for all loaded accounts"
        );

        Ok(())
    }

    pub async fn setup_all_subscriptions(whitenoise_ref: &'static Whitenoise) -> Result<()> {
        Self::setup_global_users_subscriptions(whitenoise_ref).await?;
        Self::setup_accounts_subscriptions(whitenoise_ref).await?;
        Ok(())
    }

    async fn setup_global_users_subscriptions(whitenoise_ref: &Whitenoise) -> Result<()> {
        let users_with_relays = User::all_users_with_relay_urls(whitenoise_ref).await?;
        let fallback_relays = whitenoise_ref.fallback_relay_urls().await;

        let accounts = Account::all(&whitenoise_ref.database).await?;
        if accounts.is_empty() {
            tracing::info!(
                target: "whitenoise::setup_global_users_subscriptions",
                "No accounts found, skipping global user subscriptions"
            );
            return Ok(());
        }

        // Find the first account that has a usable signer. External-only
        // accounts may not have a signer registered yet (e.g. before the
        // Flutter side provides one), so we skip those gracefully.
        let signer = accounts.iter().find_map(|account| {
            match whitenoise_ref.get_signer_for_account(account) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::debug!(
                        target: "whitenoise::setup_global_users_subscriptions",
                        "Skipping account {} for global subscriptions: {}",
                        account.pubkey.to_hex(),
                        e,
                    );
                    None
                }
            }
        });

        let Some(signer) = signer else {
            tracing::info!(
                target: "whitenoise::setup_global_users_subscriptions",
                "No account has a usable signer, skipping global user subscriptions"
            );
            return Ok(());
        };

        // Compute shared since for global user subscriptions with 10s lookback buffer
        let since = Self::compute_global_since_timestamp(whitenoise_ref).await?;

        whitenoise_ref
            .nostr
            .setup_batched_relay_subscriptions_with_signer(
                users_with_relays,
                &fallback_relays,
                signer,
                since,
            )
            .await?;
        Ok(())
    }

    // Compute a shared since timestamp for global user subscriptions.
    // - Assumes at least one account exists (caller checked signer presence)
    // - If any account is unsynced (last_synced_at = None), return None
    // - Otherwise, use min(last_synced_at) minus a 10s buffer, floored at 0
    async fn compute_global_since_timestamp(
        whitenoise_ref: &Whitenoise,
    ) -> Result<Option<nostr_sdk::Timestamp>> {
        let accounts = Account::all(&whitenoise_ref.database).await?;
        if accounts.iter().any(|a| a.last_synced_at.is_none()) {
            let unsynced = accounts
                .iter()
                .filter(|a| a.last_synced_at.is_none())
                .count();
            tracing::info!(
                target: "whitenoise::setup_global_users_subscriptions",
                "Global subscriptions using since=None due to {} unsynced accounts",
                unsynced
            );
            return Ok(None);
        }

        const BUFFER_SECS: u64 = 10;
        let since = accounts
            .iter()
            .filter_map(|a| a.since_timestamp(BUFFER_SECS))
            .min_by_key(|t| t.as_secs());

        if let Some(ts) = since {
            tracing::info!(
                target: "whitenoise::setup_global_users_subscriptions",
                "Global subscriptions using since={} ({}s buffer)",
                ts.as_secs(), BUFFER_SECS
            );
        } else {
            tracing::warn!(
                target: "whitenoise::setup_global_users_subscriptions",
                "No minimum last_synced_at found; defaulting to since=None"
            );
        }
        Ok(since)
    }

    async fn setup_accounts_subscriptions(whitenoise_ref: &'static Whitenoise) -> Result<()> {
        let accounts = Account::all(&whitenoise_ref.database).await?;
        for account in accounts {
            let nip65_relays = account.nip65_relays(whitenoise_ref).await?;
            let inbox_relays = account.inbox_relays(whitenoise_ref).await?;
            // Setup subscriptions for this account
            match whitenoise_ref
                .setup_subscriptions(&account, &nip65_relays, &inbox_relays)
                .await
            {
                Ok(()) => {
                    tracing::debug!(
                        target: "whitenoise::initialize_whitenoise",
                        "Successfully set up subscriptions for account: {}",
                        account.pubkey.to_hex()
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::initialize_whitenoise",
                        "Failed to set up subscriptions for account {}: {}",
                        account.pubkey.to_hex(),
                        e
                    );
                    // Continue with other accounts instead of failing completely
                }
            }
        }
        Ok(())
    }

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
    pub async fn delete_all_data(&self) -> Result<()> {
        tracing::debug!(target: "whitenoise::delete_all_data", "Deleting all data");

        // Shutdown gracefully before deleting data
        self.shutdown().await?;

        // Remove nostr cache
        self.nostr.delete_all_data().await?;

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
    pub(crate) async fn scheduler_task_count(&self) -> usize {
        self.scheduler_handles.lock().await.len()
    }

    pub async fn export_account_nsec(&self, account: &Account) -> Result<String> {
        if account.uses_external_signer() {
            return Err(WhitenoiseError::ExternalSignerCannotExportNsec);
        }
        Ok(self
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)?
            .secret_key()
            .to_bech32()
            .unwrap())
    }

    pub async fn export_account_npub(&self, account: &Account) -> Result<String> {
        Ok(account.pubkey.to_bech32().unwrap())
    }

    /// Registers an external signer for an account.
    ///
    /// This is used for accounts that use external signers like Amber (NIP-55).
    /// The signer will be used for operations that require signing or decryption,
    /// such as processing giftwrap events.
    ///
    /// Returns an error if the account does not exist, is not an external
    /// signer account, or if the signer's pubkey does not match.
    pub async fn register_external_signer<S>(&self, pubkey: PublicKey, signer: S) -> Result<()>
    where
        S: NostrSigner + 'static,
    {
        let account = self.find_account_by_pubkey(&pubkey).await?;
        if !account.uses_external_signer() {
            return Err(WhitenoiseError::NotExternalSignerAccount);
        }
        self.insert_external_signer(pubkey, signer).await?;

        // On app restore, external accounts may exist before their signer is
        // re-registered. Startup subscription setup can fail in that gap.
        // Rebuild account subscriptions now that signing/decryption is available.
        match (
            account.nip65_relays(self).await,
            account.inbox_relays(self).await,
        ) {
            (Ok(nip65_relays), Ok(inbox_relays)) => {
                if let Err(e) = self
                    .setup_subscriptions(&account, &nip65_relays, &inbox_relays)
                    .await
                {
                    tracing::warn!(
                        target: "whitenoise::external_signer",
                        "Failed to recover account subscriptions for {} after signer registration: {}",
                        pubkey.to_hex(),
                        e
                    );
                }
            }
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!(
                    target: "whitenoise::external_signer",
                    "Failed to load relay lists for {} during subscription recovery: {}",
                    pubkey.to_hex(),
                    e
                );
            }
        }

        // Best-effort global recovery for cases where global subscriptions were
        // also skipped due to no available signer at startup.
        if let Err(e) = self.ensure_global_subscriptions().await {
            tracing::warn!(
                target: "whitenoise::external_signer",
                "Failed to recover global subscriptions after signer registration for {}: {}",
                pubkey.to_hex(),
                e
            );
        }

        Ok(())
    }

    /// Inserts an external signer after validating the signer's pubkey matches.
    ///
    /// This is for internal use where the caller has already verified that the
    /// account uses an external signer (e.g., during login or test setup).
    /// The signer's pubkey is still validated to prevent mismatched signers.
    pub(crate) async fn insert_external_signer<S>(&self, pubkey: PublicKey, signer: S) -> Result<()>
    where
        S: NostrSigner + 'static,
    {
        self.validate_signer_pubkey(&pubkey, &signer).await?;
        tracing::info!(
            target: "whitenoise::external_signer",
            "Registering external signer for account: {}",
            pubkey.to_hex()
        );
        self.external_signers.insert(pubkey, Arc::new(signer));
        Ok(())
    }

    /// Gets the external signer for an account, if one is registered.
    pub fn get_external_signer(&self, pubkey: &PublicKey) -> Option<Arc<dyn NostrSigner>> {
        self.external_signers.get(pubkey).map(|r| r.clone())
    }

    /// Gets the appropriate signer for an account.
    ///
    /// For external accounts (Amber/NIP-55), returns the stored external signer.
    /// For local accounts, returns the keys from the secrets store.
    ///
    /// Returns an error if no signer is available for the account.
    pub(crate) fn get_signer_for_account(&self, account: &Account) -> Result<Arc<dyn NostrSigner>> {
        // First check for a registered external signer
        if let Some(external_signer) = self.get_external_signer(&account.pubkey) {
            tracing::debug!(
                target: "whitenoise::signer",
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
            target: "whitenoise::signer",
            "Using local keys for account {}",
            account.pubkey.to_hex()
        );
        Ok(Arc::new(keys))
    }

    /// Removes the external signer for an account.
    pub fn remove_external_signer(&self, pubkey: &PublicKey) {
        tracing::info!(
            target: "whitenoise::external_signer",
            "Removing external signer for account: {}",
            pubkey.to_hex()
        );
        self.external_signers.remove(pubkey);
    }

    /// Get a reference to the message aggregator for advanced usage
    /// This allows consumers to access the message aggregator directly for custom processing
    pub fn message_aggregator(&self) -> &message_aggregator::MessageAggregator {
        &self.message_aggregator
    }

    /// Subscribe to message updates for a specific group.
    ///
    /// Returns both an initial snapshot AND a receiver for real-time updates.
    /// The design eliminates race conditions:
    /// - Subscription is established BEFORE fetching to capture concurrent updates
    /// - Any updates that arrived during fetch are merged into `initial_messages`
    /// - The receiver only yields updates AFTER the initial snapshot
    ///
    /// # Arguments
    /// * `group_id` - The group to subscribe to
    ///
    /// # Returns
    /// A [`message_streaming::GroupMessageSubscription`] containing initial messages and a broadcast receiver
    pub async fn subscribe_to_group_messages(
        &self,
        group_id: &mdk_core::prelude::GroupId,
    ) -> Result<message_streaming::GroupMessageSubscription> {
        let mut updates = self.message_stream_manager.subscribe(group_id);

        let fetched_messages =
            aggregated_message::AggregatedMessage::find_messages_by_group(group_id, &self.database)
                .await
                .map_err(|e| {
                    WhitenoiseError::from(anyhow::anyhow!("Failed to read cached messages: {}", e))
                })?;

        let mut messages_map: HashMap<String, message_aggregator::ChatMessage> = fetched_messages
            .into_iter()
            .map(|m| (m.id.clone(), m))
            .collect();

        loop {
            match updates.try_recv() {
                Ok(update) => {
                    // Apply update: insert or replace by message ID
                    messages_map.insert(update.message.id.clone(), update.message);
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => {
                    // Channel closed unexpectedly - should be unreachable since we hold a receiver
                    return Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "Message stream closed unexpectedly during subscription"
                    )));
                }
            }
        }

        let mut initial_messages: Vec<message_aggregator::ChatMessage> =
            messages_map.into_values().collect();
        initial_messages.sort_by_key(|m| m.created_at);

        Ok(message_streaming::GroupMessageSubscription {
            initial_messages,
            updates,
        })
    }

    /// Subscribe to chat list updates for a specific account.
    ///
    /// Returns both an initial snapshot AND a receiver for real-time updates.
    /// The design eliminates race conditions:
    /// - Subscription is established BEFORE fetching to capture concurrent updates
    /// - Any updates that arrived during fetch are merged into `initial_items`
    /// - The receiver only yields updates AFTER the initial snapshot
    pub async fn subscribe_to_chat_list(
        &self,
        account: &Account,
    ) -> Result<chat_list_streaming::ChatListSubscription> {
        let mut updates = self.chat_list_stream_manager.subscribe(&account.pubkey);

        let fetched_items = self.get_chat_list(account).await?;

        let mut items_map: HashMap<mdk_core::prelude::GroupId, chat_list::ChatListItem> =
            fetched_items
                .into_iter()
                .map(|item| (item.mls_group_id.clone(), item))
                .collect();

        loop {
            match updates.try_recv() {
                Ok(update) => {
                    items_map.insert(update.item.mls_group_id.clone(), update.item);
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "Chat list stream closed unexpectedly during subscription"
                    )));
                }
            }
        }

        let mut initial_items: Vec<chat_list::ChatListItem> = items_map.into_values().collect();
        chat_list::sort_chat_list(&mut initial_items);

        Ok(chat_list_streaming::ChatListSubscription {
            initial_items,
            updates,
        })
    }

    /// Subscribe to notification updates across all accounts.
    ///
    /// Unlike other subscription methods, this does NOT return initial items.
    /// Notifications are real-time only
    pub fn subscribe_to_notifications(&self) -> notification_streaming::NotificationSubscription {
        notification_streaming::NotificationSubscription {
            updates: self.notification_stream_manager.subscribe(),
        }
    }

    /// Get a MediaFiles orchestrator for coordinating storage and database operations
    ///
    /// This provides high-level methods that coordinate between the storage layer
    /// (filesystem) and database layer (metadata) for media files.
    pub(crate) fn media_files(&self) -> media_files::MediaFiles<'_> {
        media_files::MediaFiles::new(&self.storage, &self.database)
    }

    /// Returns the union of default relays and currently connected relays.
    ///
    /// Used as the fallback relay set when a user has no stored NIP-65 relays.
    /// Includes all relay types known to the client (NIP-65, inbox, group, etc.),
    /// which broadens discovery coverage at negligible cost.
    pub(crate) async fn fallback_relay_urls(&self) -> Vec<RelayUrl> {
        let mut urls: HashSet<RelayUrl> = Relay::defaults().into_iter().map(|r| r.url).collect();
        for (url, relay) in self.nostr.client.relays().await {
            if relay.is_connected() {
                urls.insert(url);
            }
        }
        urls.into_iter().collect()
    }

    pub(crate) async fn refresh_global_subscription_for_user(&self, user: &User) -> Result<()> {
        let users_with_relays = User::all_users_with_relay_urls(self).await?;
        let fallback_relays = self.fallback_relay_urls().await;

        let Some(signer_account) = Account::first(&self.database).await? else {
            tracing::info!(
                target: "whitenoise::users::refresh_global_subscription",
                "No signer account found, skipping global user subscriptions"
            );
            return Ok(());
        };

        let signer = match self.get_signer_for_account(&signer_account) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::users::refresh_global_subscription",
                    "Failed to get signer for account {} (user {}), skipping global subscription refresh: {e}",
                    signer_account.pubkey.to_hex(),
                    user.pubkey.to_hex(),
                );
                return Ok(());
            }
        };
        self.nostr
            .refresh_user_global_subscriptions_with_signer(
                user.pubkey,
                users_with_relays,
                &fallback_relays,
                signer,
            )
            .await?;
        Ok(())
    }

    /// Refreshes global subscriptions for ALL batches across ALL relays.
    ///
    /// Unlike `refresh_global_subscription_for_user` which only refreshes
    /// batches containing a specific user, this refreshes every batch on every
    /// relay. Use after bulk user discovery (e.g. contact list processing)
    /// where new users may be spread across many different relay batches.
    pub(crate) async fn refresh_all_global_subscriptions(&self) -> Result<()> {
        let users_with_relays = User::all_users_with_relay_urls(self).await?;
        let fallback_relays = self.fallback_relay_urls().await;

        let Some(signer_account) = Account::first(&self.database).await? else {
            tracing::info!(
                target: "whitenoise::refresh_all_global_subscriptions",
                "No signer account found, skipping global subscription refresh"
            );
            return Ok(());
        };

        if signer_account.uses_external_signer() {
            self.nostr
                .refresh_all_global_subscriptions(users_with_relays, &fallback_relays)
                .await?;
        } else {
            let keys = self
                .secrets_store
                .get_nostr_keys_for_pubkey(&signer_account.pubkey)?;

            self.nostr
                .refresh_all_global_subscriptions_with_signer(
                    users_with_relays,
                    &fallback_relays,
                    keys,
                )
                .await?;
        }
        Ok(())
    }

    pub async fn ensure_account_subscriptions(&self, account: &Account) -> Result<()> {
        let is_operational = self.is_account_subscriptions_operational(account).await?;

        if !is_operational {
            tracing::info!(
                target: "whitenoise::ensure_account_subscriptions",
                "Account subscriptions not operational for {}, refreshing...",
                account.pubkey.to_hex()
            );
            self.refresh_account_subscriptions(account).await?;
        }

        Ok(())
    }

    pub async fn ensure_global_subscriptions(&self) -> Result<()> {
        let is_operational = self.is_global_subscriptions_operational().await?;

        if !is_operational {
            tracing::info!(
                target: "whitenoise::ensure_global_subscriptions",
                "Global subscriptions not operational, refreshing..."
            );
            Self::setup_global_users_subscriptions(self).await?;
        }

        Ok(())
    }

    /// Ensures all subscriptions (global and all accounts) are operational.
    ///
    /// This method is designed for periodic background tasks that need to ensure
    /// the entire subscription system is functioning. It checks and refreshes
    /// global subscriptions first, then iterates through all accounts.
    ///
    /// Uses a best-effort strategy: if one subscription check fails, logs the error
    /// and continues with the remaining checks. This maximizes the number of working
    /// subscriptions even when some fail due to transient network issues.
    ///
    /// # Error Handling
    ///
    /// - **Subscription errors**: Logged and ignored, processing continues
    /// - **Database errors**: Propagated immediately (catastrophic failure)
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Completed all checks (some may have failed, check logs)
    /// - `Err(_)`: Only on catastrophic failures (e.g., database connection lost)
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use whitenoise::Whitenoise;
    /// # async fn background_task(whitenoise: &Whitenoise) -> Result<(), Box<dyn std::error::Error>> {
    /// // In a periodic background task (every 15 minutes)
    /// whitenoise.ensure_all_subscriptions().await?;
    ///
    /// // All subscriptions are now as operational as possible
    /// // Check logs for any failures
    /// # Ok(())
    /// # }
    /// ```
    pub async fn ensure_all_subscriptions(&self) -> Result<()> {
        // Best-effort: log and continue on error
        if let Err(e) = self.ensure_global_subscriptions().await {
            tracing::warn!(
                target: "whitenoise::ensure_all_subscriptions",
                "Failed to ensure global subscriptions: {}", e
            );
        }

        // Fail fast only on database errors (catastrophic)
        let accounts = Account::all(&self.database).await?;

        // Best-effort: log and continue for each account
        for account in &accounts {
            if let Err(e) = self.ensure_account_subscriptions(account).await {
                tracing::warn!(
                    target: "whitenoise::ensure_all_subscriptions",
                    "Failed to ensure subscriptions for account {}: {}",
                    account.pubkey.to_hex(),
                    e
                );
            }
        }

        Ok(())
    }

    /// Checks if account subscriptions are operational
    ///
    /// Returns true if at least one relay is connected or connecting AND
    /// expected subscriptions exist (minimum: follow_list and giftwrap).
    pub async fn is_account_subscriptions_operational(&self, account: &Account) -> Result<bool> {
        let sub_count = self
            .nostr
            .count_subscriptions_for_account(&account.pubkey)
            .await;

        if sub_count < 2 {
            return Ok(false); // Early exit if subscriptions missing
        }

        let user_relays: Vec<RelayUrl> = Relay::urls(&account.nip65_relays(self).await?);
        let inbox_relays: Vec<RelayUrl> = Relay::urls(&account.inbox_relays(self).await?);

        let (group_relays, _) = self.extract_groups_relays_and_ids(account).await?;

        let all_relays: Vec<RelayUrl> = user_relays
            .iter()
            .chain(inbox_relays.iter())
            .chain(group_relays.iter())
            .cloned()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        Ok(self.nostr.has_any_relay_connected(&all_relays).await)
    }

    /// Checks if global subscriptions are operational without refreshing.
    ///
    /// Returns true if at least one relay (from the client pool) is connected or connecting
    /// AND at least one global subscription exists.
    pub async fn is_global_subscriptions_operational(&self) -> Result<bool> {
        let all_relays: Vec<RelayUrl> = self.nostr.client.relays().await.into_keys().collect();

        if !self.nostr.has_any_relay_connected(&all_relays).await {
            return Ok(false);
        }

        let global_count = self.nostr.count_global_subscriptions().await;
        Ok(global_count > 0)
    }

    #[cfg(feature = "integration-tests")]
    pub async fn wipe_database(&self) -> Result<()> {
        self.database.delete_all_data().await?;
        Ok(())
    }

    #[cfg(feature = "integration-tests")]
    pub async fn reset_nostr_client(&self) -> Result<()> {
        self.nostr.client.reset().await;
        Ok(())
    }
}

#[cfg(test)]
pub mod test_utils {
    use super::*;
    use crate::whitenoise::relays::Relay;
    use mdk_core::prelude::*;
    use nostr_sdk::{Keys, PublicKey, RelayUrl};
    use tempfile::TempDir;

    // Test configuration and setup helpers
    pub(crate) fn create_test_config() -> (WhitenoiseConfig, TempDir, TempDir) {
        let data_temp_dir = TempDir::new().expect("Failed to create temp data dir");
        let logs_temp_dir = TempDir::new().expect("Failed to create temp logs dir");
        let config = WhitenoiseConfig::new(
            data_temp_dir.path(),
            logs_temp_dir.path(),
            "com.whitenoise.test",
        );
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
    /// - `(Whitenoise, TempDir, TempDir)`
    ///   - `Whitenoise`: The mock Whitenoise instance
    ///   - `TempDir`: The temporary directory for data storage
    ///   - `TempDir`: The temporary directory for log storage
    pub(crate) async fn create_mock_whitenoise() -> (Whitenoise, TempDir, TempDir) {
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
        let (event_sender, _event_receiver) = mpsc::channel(10);
        let (shutdown_sender, _shutdown_receiver) = mpsc::channel(1);
        let (scheduler_shutdown, _scheduler_shutdown_rx) = watch::channel(false);

        // Create NostrManager for testing - now with actual relay connections
        // to use the local development relays running in docker
        let nostr = NostrManager::new(
            event_sender.clone(),
            Arc::new(event_tracker::WhitenoiseEventTracker::new(database.clone())),
            NostrManager::default_timeout(),
        )
        .await
        .expect("Failed to create NostrManager");

        // connect to default relays
        let default_relays_urls: Vec<RelayUrl> = Relay::urls(&Relay::defaults());

        for relay in default_relays_urls {
            nostr.client.add_relay(relay).await.unwrap();
        }

        nostr.client.connect().await;

        // Create Storage
        let storage = storage::Storage::new(data_temp.path()).await.unwrap();

        // Create message aggregator for testing
        let message_aggregator = message_aggregator::MessageAggregator::new();

        let whitenoise = Whitenoise {
            config,
            database,
            nostr,
            secrets_store,
            storage,
            message_aggregator,
            message_stream_manager: Arc::new(message_streaming::MessageStreamManager::default()),
            chat_list_stream_manager: chat_list_streaming::ChatListStreamManager::default(),
            notification_stream_manager: notification_streaming::NotificationStreamManager::default(
            ),
            event_sender,
            shutdown_sender,
            contact_list_guards: DashMap::new(),
            scheduler_shutdown,
            scheduler_handles: Mutex::new(Vec::new()),
            external_signers: DashMap::new(),
            background_task_cancellation: DashMap::new(),
            pending_logins: DashMap::new(),
        };

        (whitenoise, data_temp, logs_temp)
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
        // Initialize whitenoise for this specific test
        let (config, _data_temp, _logs_temp) = create_test_config();
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
            // Generate keys first
            let keys = create_test_keys();
            // Use login to create and register the account properly
            let account = whitenoise
                .login(keys.secret_key().to_secret_hex())
                .await
                .unwrap();
            accounts.push((account.clone(), keys.clone()));
            // publish keypackage to relays
            let key_package_relays = account.key_package_relays(whitenoise).await.unwrap();
            let (ekp, tags, hash_ref) = whitenoise
                .encoded_key_package(&account, &key_package_relays)
                .await
                .unwrap();

            let key_package_relays_urls =
                Relay::urls(&account.key_package_relays(whitenoise).await.unwrap());

            let result = whitenoise
                .nostr
                .publish_key_package_with_signer(&ekp, &key_package_relays_urls, &tags, keys)
                .await
                .unwrap();

            // Track the published key package for lifecycle management in tests
            if !result.success.is_empty() {
                let _ = crate::whitenoise::database::published_key_packages::PublishedKeyPackage::create(
                    &account.pubkey,
                    &hash_ref,
                    &result.id().to_hex(),
                    &whitenoise.database,
                )
                .await;
            }
        }
        accounts
    }
}

#[cfg(test)]
mod tests {
    use super::test_utils::*;
    use super::*;

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
        }

        #[test]
        fn test_whitenoise_config_debug_and_clone() {
            let (config, _data_temp, _logs_temp) = create_test_config();
            let cloned_config = config.clone();

            assert_eq!(config.data_dir, cloned_config.data_dir);
            assert_eq!(config.logs_dir, cloned_config.logs_dir);
            assert_eq!(
                config.message_aggregator_config,
                cloned_config.message_aggregator_config
            );
            assert_eq!(config.keyring_service_id, cloned_config.keyring_service_id);

            let debug_str = format!("{:?}", config);
            assert!(debug_str.contains("data_dir"));
            assert!(debug_str.contains("logs_dir"));
            assert!(debug_str.contains("message_aggregator_config"));
            assert!(debug_str.contains("keyring_service_id"));
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
        }

        #[test]
        fn test_whitenoise_config_keyring_service_id_is_required() {
            let data_dir = std::path::Path::new("/test/data");
            let logs_dir = std::path::Path::new("/test/logs");
            let config = WhitenoiseConfig::new(data_dir, logs_dir, "com.myapp.custom");
            assert_eq!(config.keyring_service_id, "com.myapp.custom");
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

            // Fetching messages for a non-existent group should return empty list (no error)
            let result = whitenoise
                .fetch_aggregated_messages_for_group(&account.pubkey, &group_id)
                .await;

            assert!(result.is_ok(), "Should succeed with empty list");
            let messages = result.unwrap();
            assert_eq!(
                messages.len(),
                0,
                "Should return empty list for non-existent group"
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
                .subscribe_to_group_messages(&group_id)
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
                .subscribe_to_group_messages(&group_id)
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

        #[tokio::test]
        async fn test_is_global_subscriptions_operational_no_subscriptions() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // No global subscriptions set up in fresh instance
            let is_operational = whitenoise
                .is_global_subscriptions_operational()
                .await
                .unwrap();

            // Should return false when no global subscriptions exist
            assert!(
                !is_operational,
                "Global subscriptions should not be operational without setup"
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
                .nostr
                .unsubscribe_account_subscriptions(&account.pubkey)
                .await
                .unwrap();

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
                .nostr
                .unsubscribe_account_subscriptions(&account1.pubkey)
                .await
                .unwrap();

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
                .nostr
                .unsubscribe_account_subscriptions(&account.pubkey)
                .await
                .unwrap();

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
        use super::*;

        #[tokio::test]
        async fn test_refresh_all_global_subscriptions_no_account_returns_ok() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // With no accounts in the database, should return Ok (early return)
            let result = whitenoise.refresh_all_global_subscriptions().await;
            assert!(
                result.is_ok(),
                "refresh_all_global_subscriptions should succeed with no accounts"
            );
        }
    }

    mod fallback_relay_tests {
        use super::*;
        use std::collections::HashSet;

        #[tokio::test]
        async fn test_fallback_relay_urls_includes_defaults() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let fallback = whitenoise.fallback_relay_urls().await;
            let default_urls: Vec<RelayUrl> = Relay::urls(&Relay::defaults());

            for url in &default_urls {
                assert!(
                    fallback.contains(url),
                    "Fallback should include default relay: {}",
                    url
                );
            }
        }

        #[tokio::test]
        async fn test_fallback_relay_urls_excludes_disconnected_relays() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let extra_url = RelayUrl::parse("wss://extra.relay.test").unwrap();
            whitenoise
                .nostr
                .client
                .add_relay(extra_url.clone())
                .await
                .unwrap();

            let fallback = whitenoise.fallback_relay_urls().await;
            assert!(
                !fallback.contains(&extra_url),
                "Fallback should not include disconnected relay"
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
