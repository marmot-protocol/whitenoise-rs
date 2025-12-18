use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use dashmap::DashMap;
use nostr_sdk::{PublicKey, RelayUrl, ToBech32};
use tokio::sync::{
    Mutex, OnceCell, Semaphore, broadcast,
    mpsc::{self, Sender},
    watch,
};
use tokio::task::JoinHandle;

pub mod accounts;
pub mod aggregated_message;
pub mod app_settings;
pub mod chat_list;
pub mod database;
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
pub mod relays;
pub mod scheduled_tasks;
pub mod secrets_store;
pub mod storage;
pub mod users;
pub mod utils;
pub mod welcomes;

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
}

impl WhitenoiseConfig {
    pub fn new(data_dir: &Path, logs_dir: &Path) -> Self {
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
        }
    }

    /// Create a new configuration with custom message aggregator settings
    pub fn new_with_aggregator_config(
        data_dir: &Path,
        logs_dir: &Path,
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
    message_stream_manager: message_streaming::MessageStreamManager,
    event_sender: Sender<ProcessableEvent>,
    shutdown_sender: Sender<()>,
    /// Per-account concurrency guards to prevent race conditions in contact list processing
    contact_list_guards: DashMap<PublicKey, Arc<Semaphore>>,
    /// Shutdown signal for scheduled tasks
    scheduler_shutdown: watch::Sender<bool>,
    /// Handles for spawned scheduler tasks
    scheduler_handles: Mutex<Vec<JoinHandle<()>>>,
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
            .field("event_sender", &"<REDACTED>")
            .field("shutdown_sender", &"<REDACTED>")
            .field("contact_list_guards", &"<REDACTED>")
            .field("scheduler_shutdown", &"<REDACTED>")
            .field("scheduler_handles", &"<REDACTED>")
            .finish()
    }
}

impl Whitenoise {
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

        // Create SecretsStore
        let secrets_store = SecretsStore::new(data_dir);

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
            message_stream_manager: message_streaming::MessageStreamManager::default(),
            event_sender,
            shutdown_sender,
            contact_list_guards: DashMap::new(),
            scheduler_shutdown,
            scheduler_handles: Mutex::new(Vec::new()),
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

        tracing::debug!(
            target: "whitenoise::initialize_whitenoise",
            "Starting event processing loop for loaded accounts"
        );

        Self::start_event_processing_loop(whitenoise_ref, event_receiver, shutdown_receiver).await;

        // Register and start scheduled background tasks
        let tasks: Vec<Arc<dyn scheduled_tasks::Task>> =
            vec![Arc::new(scheduled_tasks::KeyPackageMaintenance)];
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
        let default_relays: Vec<RelayUrl> = Relay::urls(&Relay::defaults());

        let Some(signer_account) = Account::first(&whitenoise_ref.database).await? else {
            tracing::info!(
                target: "whitenoise::setup_global_users_subscriptions",
                "No signer account found, skipping global user subscriptions"
            );
            return Ok(());
        };

        let keys = whitenoise_ref
            .secrets_store
            .get_nostr_keys_for_pubkey(&signer_account.pubkey)?;

        // Compute shared since for global user subscriptions with 10s lookback buffer
        let since = Self::compute_global_since_timestamp(whitenoise_ref).await?;

        whitenoise_ref
            .nostr
            .setup_batched_relay_subscriptions_with_signer(
                users_with_relays,
                &default_relays,
                keys,
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
            .min_by_key(|t| t.as_u64());

        if let Some(ts) = since {
            tracing::info!(
                target: "whitenoise::setup_global_users_subscriptions",
                "Global subscriptions using since={} ({}s buffer)",
                ts.as_u64(), BUFFER_SECS
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
    pub(crate) async fn scheduler_task_count(&self) -> usize {
        self.scheduler_handles.lock().await.len()
    }

    pub async fn export_account_nsec(&self, account: &Account) -> Result<String> {
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

    /// Get a MediaFiles orchestrator for coordinating storage and database operations
    ///
    /// This provides high-level methods that coordinate between the storage layer
    /// (filesystem) and database layer (metadata) for media files.
    pub(crate) fn media_files(&self) -> media_files::MediaFiles<'_> {
        media_files::MediaFiles::new(&self.storage, &self.database)
    }

    pub(crate) async fn refresh_global_subscription_for_user(&self, user: &User) -> Result<()> {
        let users_with_relays = User::all_users_with_relay_urls(self).await?;
        let default_relays: Vec<RelayUrl> = Relay::urls(&Relay::defaults());

        let Some(signer_account) = Account::first(&self.database).await? else {
            tracing::info!(
                target: "whitenoise::users::refresh_global_subscription",
                "No signer account found, skipping global user subscriptions"
            );
            return Ok(());
        };

        let keys = self
            .secrets_store
            .get_nostr_keys_for_pubkey(&signer_account.pubkey)?;

        self.nostr
            .refresh_user_global_subscriptions_with_signer(
                user.pubkey,
                users_with_relays,
                &default_relays,
                keys,
            )
            .await?;
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
        let config = WhitenoiseConfig::new(data_temp_dir.path(), logs_temp_dir.path());
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
        let secrets_store = SecretsStore::new(&config.data_dir);

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
            message_stream_manager: message_streaming::MessageStreamManager::default(),
            event_sender,
            shutdown_sender,
            contact_list_guards: DashMap::new(),
            scheduler_shutdown,
            scheduler_handles: Mutex::new(Vec::new()),
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
            let (ekp, tags) = whitenoise
                .encoded_key_package(&account, &key_package_relays)
                .await
                .unwrap();

            let key_package_relays_urls =
                Relay::urls(&account.key_package_relays(whitenoise).await.unwrap());

            let _ = whitenoise
                .nostr
                .publish_key_package_with_signer(&ekp, &key_package_relays_urls, &tags, keys)
                .await
                .unwrap();
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
            let config = WhitenoiseConfig::new(data_dir, logs_dir);

            if cfg!(debug_assertions) {
                assert_eq!(config.data_dir, data_dir.join("dev"));
                assert_eq!(config.logs_dir, logs_dir.join("dev"));
            } else {
                assert_eq!(config.data_dir, data_dir.join("release"));
                assert_eq!(config.logs_dir, logs_dir.join("release"));
            }
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

            let debug_str = format!("{:?}", config);
            assert!(debug_str.contains("data_dir"));
            assert!(debug_str.contains("logs_dir"));
            assert!(debug_str.contains("message_aggregator_config"));
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
                custom_config.clone(),
            );

            assert!(config.message_aggregator_config.is_some());
            let aggregator_config = config.message_aggregator_config.unwrap();
            assert!(!aggregator_config.normalize_emoji);
            assert!(aggregator_config.enable_debug_logging);
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
    }
}
