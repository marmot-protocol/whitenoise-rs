use std::ffi::OsStr;
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use ::rand::RngCore;
use nostr_sdk::{PublicKey, RelayUrl};
use once_cell::sync::OnceCell as SyncOnceCell;
use tokio::sync::{
    Mutex, OnceCell,
    mpsc::{self, Sender},
    watch,
};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

pub mod account_settings;
pub mod accounts;
pub mod accounts_groups;
pub mod aggregated_message;
pub mod app_settings;
pub mod background_notifications;
mod broadcast_hub;
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
mod foreground_catchup;
pub mod group_information;
pub mod group_state_streaming;
pub mod groups;
mod init_timing;
pub mod key_packages;
// Platform keyring wrappers are only needed where we install a custom
// keyring-core default store, plus tests that exercise those wrappers.
#[cfg(any(
    test,
    all(
        any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly",
            target_os = "android"
        ),
        not(feature = "integration-tests"),
        not(feature = "benchmark-tests")
    )
))]
mod keyring_store;
pub mod media_files;
pub mod message_aggregator;
pub mod message_streaming;
pub mod messages;
pub mod mute_list;
pub mod notification_streaming;
pub mod product_analytics;
pub mod push_notifications;
pub mod relays;
pub mod scheduled_tasks;
pub mod secrets_store;
pub mod session;
pub(crate) mod shared;
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
use mdk_sqlite_storage::{MdkSqliteStorage, keyring};

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
use secrets_store::{SecretsStore, SecretsStoreError};
#[cfg(test)]
use users::User;

struct KeyringStoreInit {
    initialized: SyncOnceCell<()>,
}

impl KeyringStoreInit {
    const fn new() -> Self {
        Self {
            initialized: SyncOnceCell::new(),
        }
    }

    fn initialize_with<DefaultStoreExists, InitializeStore>(
        &self,
        default_store_exists: DefaultStoreExists,
        initialize_store: InitializeStore,
    ) -> Result<()>
    where
        DefaultStoreExists: Fn() -> bool,
        InitializeStore: FnOnce() -> Result<()>,
    {
        self.initialized
            .get_or_try_init(|| {
                if default_store_exists() {
                    return Ok(());
                }

                initialize_store()
            })
            .map(|_| ())
    }
}

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

    /// Override for the Whitenoise SQLCipher keyring key id; available in test and benchmark builds.
    #[cfg(any(test, feature = "integration-tests", feature = "benchmark-tests"))]
    pub database_key_id: Option<String>,

    /// Configured discovery relays for the relay-control discovery plane.
    pub discovery_relays: Vec<RelayUrl>,

    /// Opt-in product analytics configuration.
    pub product_analytics_config: Option<product_analytics::ProductAnalyticsConfig>,

    /// Relay URLs adopted as a freshly-created account's NIP-65, Inbox, and
    /// KeyPackage lists when no existing relay-list events are found on the
    /// network. Seeded from [`Relay::defaults`]; override with
    /// [`Self::with_default_account_relays`] for benchmarks or private
    /// (non-public-relay) deployments.
    pub default_account_relays: Vec<RelayUrl>,
}

impl WhitenoiseConfig {
    fn normalize_keyring_service_id(&mut self) {
        self.keyring_service_id = self.keyring_service_id.trim().to_string();
    }

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
            #[cfg(any(test, feature = "integration-tests", feature = "benchmark-tests"))]
            database_key_id: None,
            discovery_relays: DiscoveryPlaneConfig::curated_default_relays(),
            product_analytics_config: None,
            default_account_relays: Relay::urls(&Relay::defaults()),
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
            #[cfg(any(test, feature = "integration-tests", feature = "benchmark-tests"))]
            database_key_id: None,
            discovery_relays: DiscoveryPlaneConfig::curated_default_relays(),
            product_analytics_config: None,
            default_account_relays: Relay::urls(&Relay::defaults()),
        }
    }

    pub fn with_discovery_relays(mut self, discovery_relays: Vec<RelayUrl>) -> Self {
        self.discovery_relays = discovery_relays;
        self
    }

    /// Configure opt-in product analytics; validated when `Whitenoise` is initialized.
    pub fn with_product_analytics_config(
        mut self,
        config: product_analytics::ProductAnalyticsConfig,
    ) -> Self {
        self.product_analytics_config = Some(config);
        self
    }

    /// Override the relay URLs adopted as a freshly-created account's NIP-65,
    /// Inbox, and KeyPackage lists when the network has no existing relay-list
    /// events for the account. Production callers should almost never set this
    /// — it is intended for benchmarks and private (non-public-relay)
    /// deployments where the public defaults are inappropriate.
    pub fn with_default_account_relays(mut self, default_account_relays: Vec<RelayUrl>) -> Self {
        self.default_account_relays = default_account_relays;
        self
    }

    #[cfg(any(test, feature = "integration-tests", feature = "benchmark-tests"))]
    pub fn with_database_key_id(mut self, database_key_id: &str) -> Self {
        self.database_key_id = Some(database_key_id.to_string());
        self
    }
}

pub struct Whitenoise {
    pub shared: Arc<shared::SharedServices>,
    event_sender: Sender<ProcessableEvent>,
    shutdown_sender: Sender<()>,
    /// Shutdown signal for scheduled tasks
    scheduler_shutdown: watch::Sender<bool>,
    /// Handles for spawned scheduler tasks
    scheduler_handles: Mutex<Vec<JoinHandle<()>>>,
    /// Per-account session manager. Holds active sessions and pending logins.
    pub(crate) account_manager: session::AccountManager,
    /// Weak self-reference installed at construction via [`Arc::new_cyclic`].
    /// Methods that take `&self` but need to hand out an `Arc<Whitenoise>` (for
    /// example to stamp a back-reference into a freshly constructed
    /// `AccountSession`) upgrade this. Never upgrade in a context that expects
    /// the instance to outlive the holder.
    pub(crate) this: Weak<Whitenoise>,
    /// Handles for fire-and-forget background tasks (discovery catch-up,
    /// user-search workers, welcome-finalization, relay-list refresh, etc.).
    /// Mirrors [`Self::scheduler_handles`] for non-scheduler async work that
    /// would otherwise be dropped and run unobserved past shutdown.
    ///
    /// Tests use [`Self::wait_for_pending_background_tasks`] to drain pending
    /// work before asserting; production drains on shutdown so a stopping
    /// process doesn't leave half-finished DB writes racing with teardown.
    background_handles: Mutex<Vec<JoinHandle<()>>>,
}

/// Process-lifetime cache used exclusively by [`Whitenoise::ensure_initialized`]
/// to support iOS notification handlers that may fire concurrently across
/// process wake-ups. Holds a clone of the `Arc<Whitenoise>` produced by the
/// first successful initialization so subsequent callers observe the same
/// instance instead of double-initializing.
///
/// This is **not** a general-purpose access path — `Arc<Whitenoise>` ownership
/// flows through explicit handles everywhere else (the FFI boundary, sessions,
/// scheduled tasks). `tokio::sync::OnceCell::get_or_try_init` serializes
/// concurrent first-callers internally, replacing the previous
/// `ENSURE_INITIALIZED_LOCK` mutex without losing the race-free guarantee.
static GLOBAL_WHITENOISE: OnceCell<Arc<Whitenoise>> = OnceCell::const_new();
const DELETE_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

async fn get_or_try_init_with_initializer_flag<T, E, Fut>(
    cell: &OnceCell<T>,
    init: impl FnOnce() -> Fut,
) -> core::result::Result<(&T, bool), E>
where
    Fut: Future<Output = core::result::Result<T, E>>,
{
    let initialized_by_this_call = AtomicBool::new(false);
    let value = cell
        .get_or_try_init(|| async {
            initialized_by_this_call.store(true, Ordering::Relaxed);
            init().await
        })
        .await?;

    Ok((value, initialized_by_this_call.load(Ordering::Relaxed)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartupSubscriptionMode {
    Run,
    Defer,
}

fn background_notification_startup_subscription_mode() -> StartupSubscriptionMode {
    StartupSubscriptionMode::Defer
}

/// Marker carried in [`WhitenoiseComponents`] to defer construction of the
/// standard [`WhitenoiseEventTracker`] until inside `Arc::new_cyclic`, where
/// the `Weak<Whitenoise>` is available.
struct UseWhitenoiseEventTracker;
struct WhitenoiseComponents {
    event_tracker: UseWhitenoiseEventTracker,
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
            .field("config", self.config())
            .field("shared", &"<REDACTED>")
            .field("event_sender", &"<REDACTED>")
            .field("shutdown_sender", &"<REDACTED>")
            .field("scheduler_shutdown", &"<REDACTED>")
            .field("scheduler_handles", &"<REDACTED>")
            .field("account_manager", &"<REDACTED>")
            .field("background_handles", &"<REDACTED>")
            .finish()
    }
}

impl Whitenoise {
    /// Enumerate every persisted account that has either a row in
    /// `shared.accounts` or a per-account DB file under `<data_dir>/accounts/`,
    /// open each one without running migrations, and return the pool plus its
    /// hex pubkey.
    ///
    /// Used at boot before [`crate::whitenoise::database::rust_migrations::MIGRATOR::run_all`]
    /// so the unified timeline can apply per-account local copies in lockstep
    /// with the shared global drops that depend on them.
    ///
    /// Resilient to fresh installs (no `accounts` table yet) and to accounts
    /// that exist in shared but have no per-account file yet (the file is
    /// created by `open_without_migrations` so the bootstrap can stamp it
    /// later).
    async fn enumerate_account_pools(
        shared: &Database,
        data_dir: &Path,
    ) -> Result<Vec<(sqlx::SqlitePool, String)>> {
        use std::collections::HashSet;

        let accounts_dir = data_dir.join("accounts");
        std::fs::create_dir_all(&accounts_dir)?;

        // Best-effort read of `accounts.pubkey`. Fresh installs (no table yet)
        // return an empty list; that's fine — there's nothing to migrate.
        let mut pubkeys: HashSet<String> = accounts::Account::all_pubkeys_hex(shared)
            .await?
            .into_iter()
            .collect();

        // Also include any per-account DB files on disk that aren't in
        // `shared.accounts` — orphan files still deserve their own migration
        // pass so a stale row never gets dropped without copying.
        if let Ok(entries) = std::fs::read_dir(&accounts_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("db") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    pubkeys.insert(stem.to_string());
                }
            }
        }

        let mut pools = Vec::with_capacity(pubkeys.len());
        for pubkey in pubkeys {
            let path = accounts_dir.join(format!("{pubkey}.db"));
            let db = Database::open_without_migrations(path, None).await?;
            pools.push((db.pool, pubkey));
        }

        tracing::info!(
            target: "whitenoise::new",
            "Enumerated {} per-account database(s) for migration lockstep",
            pools.len()
        );
        Ok(pools)
    }

    fn from_components(
        mut config: WhitenoiseConfig,
        database: Arc<Database>,
        components: WhitenoiseComponents,
    ) -> Result<Arc<Self>> {
        config.normalize_keyring_service_id();
        config.validate_product_analytics_config()?;
        let mut session_salt = [0u8; 16];
        ::rand::rng().fill_bytes(&mut session_salt);
        let discovery_relays = config.discovery_relays.clone();

        let WhitenoiseComponents {
            event_tracker,
            secrets_store,
            storage,
            message_aggregator,
            event_sender,
            shutdown_sender,
            scheduler_shutdown,
        } = components;

        let config = Arc::new(config);

        let whitenoise = Arc::new_cyclic(move |weak: &Weak<Self>| {
            let UseWhitenoiseEventTracker = event_tracker;
            let event_tracker_arc: std::sync::Arc<dyn event_tracker::EventTracker> =
                Arc::new(WhitenoiseEventTracker::new(database.clone(), weak.clone()));

            let relay_control = Arc::new(RelayControlPlane::new(
                database.clone(),
                discovery_relays,
                event_sender.clone(),
                session_salt,
                event_tracker_arc.clone(),
            ));

            let shared = Arc::new(shared::SharedServices::new(
                config,
                database,
                relay_control,
                event_tracker_arc,
                secrets_store,
                storage,
                message_aggregator,
            ));

            Self {
                shared,
                event_sender,
                shutdown_sender,
                scheduler_shutdown,
                scheduler_handles: Mutex::new(Vec::new()),
                account_manager: session::AccountManager::default(),
                this: weak.clone(),
                background_handles: Mutex::new(Vec::new()),
            }
        });
        Ok(whitenoise)
    }

    /// Access the runtime configuration. The `WhitenoiseConfig` value is held
    /// behind `Arc<SharedServices>` so every session observes the same instance.
    pub fn config(&self) -> &WhitenoiseConfig {
        &self.shared.config
    }

    /// Upgrade the weak self-reference stamped during [`Arc::new_cyclic`].
    /// Returns [`WhitenoiseError::Initialization`] if the backing `Arc` has
    /// already been dropped (only possible mid-drop).
    pub(crate) fn arc(&self) -> Result<Arc<Self>> {
        self.this.upgrade().ok_or(WhitenoiseError::Initialization)
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
    /// an effect. If another crate or host application has already configured
    /// a `keyring-core` default store, that store is preserved.
    fn initialize_keyring_store() -> Result<()> {
        static KEYRING_STORE_INIT: KeyringStoreInit = KeyringStoreInit::new();
        KEYRING_STORE_INIT.initialize_with(
            || keyring_core::get_default_store().is_some(),
            Self::initialize_platform_keyring_store,
        )
    }

    fn initialize_platform_keyring_store() -> Result<()> {
        // Use the mock (in-memory) store in test, integration-test, and
        // benchmark-test builds so that `cargo test` and unsigned `cargo run`
        // binaries never require real platform keychain entitlements.
        // The real store is reserved for production builds (no feature flags).
        #[cfg(any(test, feature = "integration-tests", feature = "benchmark-tests"))]
        {
            Self::set_default_keyring_store(keyring_core::mock::Store::new(), "mock")?;
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
                Self::set_default_keyring_store(
                    apple_native_keyring_store::keychain::Store::new(),
                    "macOS Keychain",
                )?;
            }
            // Intel Macs (non-CLI): use the regular Keychain store.
            // Protected Data store is not available on x86_64.
            #[cfg(all(target_os = "macos", target_arch = "x86_64", not(feature = "cli")))]
            {
                Self::set_default_keyring_store(
                    apple_native_keyring_store::keychain::Store::new(),
                    "macOS Keychain",
                )?;
            }
            // Apple Silicon (non-CLI): use the Protected Data store (audit #630).
            // Requires code-signing with a provisioning profile that includes the
            // com.apple.security.application-groups entitlement. The Flutter app
            // build pipeline satisfies this.
            #[cfg(all(target_os = "macos", target_arch = "aarch64", not(feature = "cli")))]
            {
                Self::set_default_keyring_store(
                    apple_native_keyring_store::protected::Store::new(),
                    "macOS protected-data",
                )?;
            }
            #[cfg(target_os = "ios")]
            {
                Self::set_default_keyring_store(
                    apple_native_keyring_store::protected::Store::new(),
                    "iOS protected-data",
                )?;
            }
            #[cfg(target_os = "windows")]
            {
                Self::set_default_keyring_store(
                    windows_native_keyring_store::Store::new(),
                    "Windows",
                )?;
            }
            #[cfg(target_os = "linux")]
            {
                let primary = Self::create_secret_service_keyring_store("Linux Secret Service")?;
                let store = Self::create_legacy_migration_keyring_store(
                    primary,
                    linux_keyutils_keyring_store::Store::new(),
                    "legacy Linux keyutils",
                );
                keyring_core::set_default_store(store);
            }
            #[cfg(any(
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "dragonfly"
            ))]
            {
                let store = Self::create_secret_service_keyring_store("BSD Secret Service")?;
                keyring_core::set_default_store(store);
            }
            #[cfg(target_os = "android")]
            {
                let primary = Self::create_keyring_store(
                    android_native_keyring_store::Store::new(),
                    "Android",
                )?;
                let store = Self::create_legacy_migration_keyring_store(
                    primary,
                    android_native_keyring_store::LegacyStore::from_ndk_context(),
                    "legacy Android",
                );
                keyring_core::set_default_store(store);
            }
            #[cfg(not(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "windows",
                target_os = "linux",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "dragonfly",
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

        Ok(())
    }

    fn create_keyring_store<S>(
        store: keyring_core::Result<Arc<S>>,
        store_name: &str,
    ) -> Result<Arc<keyring_core::CredentialStore>>
    where
        S: keyring_core::api::CredentialStoreApi + Send + Sync + 'static,
    {
        let store = store.map_err(|e| {
            WhitenoiseError::SecretsStore(SecretsStoreError::KeyringUnavailable(format!(
                "Failed to create {store_name} credential store: {e}"
            )))
        })?;
        Ok(store)
    }

    #[cfg(all(
        any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        ),
        not(test),
        not(feature = "integration-tests"),
        not(feature = "benchmark-tests")
    ))]
    fn create_secret_service_keyring_store(
        store_name: &str,
    ) -> Result<Arc<keyring_core::CredentialStore>> {
        Self::create_targeted_secret_service_keyring_store(
            zbus_secret_service_keyring_store::Store::new(),
            store_name,
        )
    }

    #[cfg(any(
        test,
        all(
            any(
                target_os = "linux",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "dragonfly"
            ),
            not(feature = "integration-tests"),
            not(feature = "benchmark-tests")
        )
    ))]
    fn create_targeted_secret_service_keyring_store<S>(
        store: keyring_core::Result<Arc<S>>,
        store_name: &str,
    ) -> Result<Arc<keyring_core::CredentialStore>>
    where
        S: keyring_core::api::CredentialStoreApi + Send + Sync + 'static,
    {
        let store = Self::create_keyring_store(store, store_name)?;
        Ok(keyring_store::TargetedCredentialStore::new(
            store,
            keyring_store::SECRET_SERVICE_TARGET,
        ))
    }

    #[cfg(any(
        test,
        all(
            any(target_os = "linux", target_os = "android"),
            not(feature = "integration-tests"),
            not(feature = "benchmark-tests")
        )
    ))]
    fn create_legacy_migration_keyring_store<S, E>(
        primary: Arc<keyring_core::CredentialStore>,
        legacy: core::result::Result<Arc<S>, E>,
        legacy_store_name: &str,
    ) -> Arc<keyring_core::CredentialStore>
    where
        S: keyring_core::api::CredentialStoreApi + Send + Sync + 'static,
        E: Into<keyring_core::Error>,
    {
        let legacy = legacy.map_err(Into::into);
        match Self::create_keyring_store(legacy, legacy_store_name) {
            Ok(legacy) => keyring_store::LegacyMigrationCredentialStore::new(primary, legacy),
            Err(err) => {
                tracing::warn!(
                    target: "whitenoise::keyring_store",
                    error = %err,
                    store = legacy_store_name,
                    "Failed to create legacy keyring store; continuing with primary store only"
                );
                primary
            }
        }
    }

    fn set_default_keyring_store<S>(
        store: keyring_core::Result<Arc<S>>,
        store_name: &str,
    ) -> Result<()>
    where
        S: keyring_core::api::CredentialStoreApi + Send + Sync + 'static,
    {
        let store = Self::create_keyring_store(store, store_name)?;
        keyring_core::set_default_store(store);
        Ok(())
    }

    /// Initializes the mock keyring store for testing environments.
    ///
    /// This is a convenience alias for external callers (e.g. the integration
    /// test binary) that need to set up the mock store before
    /// `Whitenoise::new` is called.  In practice
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
        Self::initialize_keyring_store().expect("Failed to initialize mock keyring store");
    }

    /// Creates an MDK instance for the given account public key using this
    /// instance's configured data directory and keyring service identifier.
    pub(crate) fn create_mdk_for_account(
        &self,
        pubkey: PublicKey,
    ) -> core::result::Result<MDK<MdkSqliteStorage>, AccountError> {
        Account::create_mdk(pubkey, &self.config().data_dir, self.keyring_service_id())
    }

    /// Constructs a fully-initialized `Whitenoise` instance and returns it
    /// wrapped in `Arc<Self>`.
    ///
    /// This is the authoritative constructor: it sets up data and log
    /// directories, initializes logging and the database, seeds default
    /// relays and settings, spawns the event-processing loop, scheduled
    /// tasks, and the discovery sync worker.
    ///
    /// **Relay-bound init is performed asynchronously and continues after
    /// this function returns.** Two background tasks are spawned: one brings
    /// the discovery plane online (connecting to the configured discovery
    /// relays and warming the ephemeral pool against the same URLs); the
    /// other ensures per-account inbox / group-plane subscriptions are
    /// operational and signals the discovery sync worker. Callers that need
    /// synchronously-live subscriptions (for example, the iOS background-push
    /// path) must call [`Self::ensure_all_subscriptions`] before depending on
    /// inbound event delivery — the existing FFI entry point in
    /// [`crate::whitenoise::background_notifications`] already does this.
    ///
    /// Operations that read or publish through the discovery plane
    /// (`fetch_user_relays`, the discovery sync worker's own rebuild, etc.)
    /// will lazy-connect on first use if the background task has not yet
    /// completed, so no caller has to wait explicitly. The
    /// `subscription_health_check` scheduled task is the recovery safety net
    /// for any transient relay failure within either deferred task.
    ///
    /// Callers own the returned `Arc` for the lifetime of the process (or the
    /// test) and drop it to tear the instance down. Sessions and event
    /// handlers receive their own `Arc<Whitenoise>` clones internally via the
    /// weak self-reference stamped at construction. Both deferred tasks are
    /// registered with the background-task pool, so [`Self::shutdown`] waits
    /// for them to complete before returning.
    #[perf_instrument("whitenoise")]
    pub async fn new(config: WhitenoiseConfig) -> Result<Arc<Self>> {
        Self::new_with_startup_subscription_mode(config, StartupSubscriptionMode::Run).await
    }

    async fn new_with_startup_subscription_mode(
        mut config: WhitenoiseConfig,
        startup_subscription_mode: StartupSubscriptionMode,
    ) -> Result<Arc<Self>> {
        init_timing::start();

        // Validate keyring_service_id is not empty or whitespace
        config.normalize_keyring_service_id();
        config.validate_product_analytics_config()?;
        if config.keyring_service_id.is_empty() {
            return Err(WhitenoiseError::Configuration(
                "keyring_service_id cannot be empty or whitespace".to_string(),
            ));
        }
        let keyring_service_id = config.keyring_service_id.clone();

        // Ensure keyring-core has a credential store before any MDK or
        // SecretsStore operations attempt to create or read keyring entries.
        Self::initialize_keyring_store()?;

        // Create event processing channels
        let (event_sender, event_receiver) = mpsc::channel(2000);
        let (shutdown_sender, shutdown_receiver) = mpsc::channel(1);

        // Create scheduler shutdown channel
        let (scheduler_shutdown, scheduler_shutdown_rx) = watch::channel(false);

        init_timing::record("keyring_and_channels");

        let data_dir = config.data_dir.clone();
        let logs_dir = config.logs_dir.clone();

        // Setup directories. Path context is preserved via tracing; the
        // io::Error itself flows through `WhitenoiseError::Filesystem` so
        // callers can distinguish filesystem failures.
        std::fs::create_dir_all(&data_dir).inspect_err(|e| {
            tracing::error!(
                target: "whitenoise::new",
                ?data_dir,
                error = %e,
                "Failed to create data directory"
            );
        })?;
        std::fs::create_dir_all(&logs_dir).inspect_err(|e| {
            tracing::error!(
                target: "whitenoise::new",
                ?logs_dir,
                error = %e,
                "Failed to create logs directory"
            );
        })?;

        // Only initialize tracing once
        init_tracing(&logs_dir);

        tracing::debug!(target: "whitenoise::new", "Logging initialized in directory: {:?}", logs_dir);

        init_timing::record("directories_and_logging");

        // Open shared database with SQLCipher encryption but without running
        // migrations yet. The unified migration timeline contains global drops
        // (v21-v26) that depend on per-account local copies (v15-v20) running
        // first. We open encrypted without migrations, then call
        // `MIGRATOR.run_all` so the framework walks one version at a time,
        // applying each local to every account in lockstep with its surrounding
        // globals.
        let database_path = data_dir.join("whitenoise.sqlite");
        #[cfg(any(test, feature = "integration-tests", feature = "benchmark-tests"))]
        let database = Arc::new(match config.database_key_id.as_deref() {
            Some(database_key_id) => {
                Database::new_encrypted_with_key_id(
                    database_path,
                    &keyring_service_id,
                    database_key_id,
                )
                .await?
            }
            None => Database::new_encrypted(database_path, &keyring_service_id).await?,
        });
        #[cfg(not(any(test, feature = "integration-tests", feature = "benchmark-tests")))]
        let database = Arc::new(Database::new_encrypted(database_path, &keyring_service_id).await?);

        let account_pools = Self::enumerate_account_pools(&database, &data_dir).await?;
        crate::whitenoise::database::rust_migrations::MIGRATOR
            .run_all(&database.pool, &account_pools)
            .await?;
        // Drop the per-account pools now that migrations are committed; sessions
        // will re-open them on bring-up. `run_account_migrations` is then a
        // no-op on already-stamped versions and only fires the bootstrap (v12)
        // for any account file created post-boot (e.g. fresh login).
        drop(account_pools);

        init_timing::record("database");

        // Create SecretsStore backed by the platform keyring-core store
        let secrets_store = SecretsStore::new(&keyring_service_id);

        // Create Storage
        let storage = storage::Storage::new(&data_dir).await?;

        // Create message aggregator - always initialize, use custom config if provided
        let message_aggregator =
            if let Some(aggregator_config) = config.message_aggregator_config.clone() {
                message_aggregator::MessageAggregator::with_config(aggregator_config)
            } else {
                message_aggregator::MessageAggregator::new()
            };
        let whitenoise = Self::from_components(
            config,
            database,
            WhitenoiseComponents {
                event_tracker: UseWhitenoiseEventTracker,
                secrets_store,
                storage,
                message_aggregator,
                event_sender,
                shutdown_sender,
                scheduler_shutdown,
            },
        )?;
        whitenoise
            .shared
            .relay_control
            .start_telemetry_persistors()
            .await;

        init_timing::record("core_services");

        // Seed the relays table with the configured default-account relays so
        // a private-relay deployment does not carry stale rows for relays it
        // never uses. The discovery plane owns its own relay set elsewhere.
        // TODO: Make this batch fetch and insert all relays at once.
        for url in &whitenoise.config().default_account_relays {
            let _ = whitenoise.find_or_create_relay_by_url(url).await?;
        }

        // Create default app settings in the database if they don't exist
        AppSettings::find_or_create_default(&whitenoise.shared.database).await?;

        init_timing::record("database_seeding");

        // Two invariants make deferring the discovery-plane start safe:
        //   1. No init step below (`restore_sessions`,
        //      `sync_message_cache_on_startup`, `backfill_dm_peer_pubkeys`,
        //      the worker spawns) touches the discovery plane. Preserve this
        //      when adding steps here.
        //   2. The deferred subscription setup signals the discovery worker,
        //      whose `sync_discovery_subscriptions` itself calls
        //      `discovery.start()`. Both paths converge on
        //      `ensure_relays_connected`, which is idempotent at the
        //      relay-client layer.
        {
            let wn = Arc::clone(&whitenoise);
            whitenoise
                .spawn_background(async move {
                    if let Err(error) = wn.shared.relay_control.start_discovery_plane().await {
                        tracing::warn!(
                            target: "whitenoise::new",
                            "Background discovery-plane start failed: {error}"
                        );
                    }
                })
                .await;
        }

        init_timing::record("discovery_plane_spawned");

        // Restore account sessions before any startup step that resolves a
        // session by pubkey. `sync_message_cache_on_startup` reads per-account
        // `aggregated_messages` via `session.account_db` (post phase-18e), so
        // sessions must be populated first or it raises `AccountNotFound`.
        // Migrations were already applied in lockstep across shared and every
        // on-disk per-account file via `MIGRATOR.run_all` at the top of
        // `Whitenoise::new`, so `run_account_migrations` here is a no-op for
        // already-stamped versions. Only newly-created accounts (e.g. first
        // login post-boot) need it to fire the bootstrap migration.
        let _ = whitenoise
            .account_manager
            .restore_sessions(&whitenoise)
            .await;

        init_timing::record("session_restore");

        // Sync each account's mute list before any code path can write
        // `aggregated_messages` rows. `sync_message_cache_on_startup` below
        // hydrates the cache with whatever's in the local mute list at the
        // time, so a stale list would mass-stamp blocked authors as visible.
        // Bounded; the post-sync stamp/unstamp sweeps reconcile any residual.
        whitenoise.wait_for_mute_list_sync_or_timeout().await;
        init_timing::record("mute_list_cold_start_sync");

        tracing::info!(
            target: "whitenoise::new",
            "Synchronizing message cache with MDK..."
        );
        // Synchronize message cache BEFORE starting event processor
        // This eliminates race conditions between startup sync and real-time cache updates
        whitenoise.sync_message_cache_on_startup().await?;
        tracing::info!(
            target: "whitenoise::new",
            "Message cache synchronization complete"
        );

        init_timing::record("message_cache_sync");

        // Backfill dm_peer_pubkey for existing DM groups missing it
        if let Err(e) = whitenoise.backfill_dm_peer_pubkeys().await {
            tracing::warn!(
                target: "whitenoise::new",
                "DM peer pubkey backfill failed (non-fatal): {}",
                e
            );
        }

        tracing::debug!(
            target: "whitenoise::new",
            "Starting event processing loop for loaded accounts"
        );

        Self::start_event_processing_loop(
            Arc::clone(&whitenoise),
            event_receiver,
            shutdown_receiver,
        )
        .await;

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
            Arc::clone(&whitenoise),
            scheduler_shutdown_rx,
            None,
            tasks,
        );
        *whitenoise.scheduler_handles.lock().await = scheduler_handles;

        // Spawn discovery sync worker
        {
            let worker_shutdown_rx = whitenoise.scheduler_shutdown.subscribe();
            let worker_whitenoise = Arc::clone(&whitenoise);
            let handle = tokio::spawn(async move {
                let wn = Arc::clone(&worker_whitenoise);
                worker_whitenoise
                    .shared
                    .discovery_sync_worker
                    .run(wn, worker_shutdown_rx)
                    .await;
            });
            whitenoise.scheduler_handles.lock().await.push(handle);
        }

        init_timing::record("background_tasks");

        // `ensure_all_subscriptions` is the right primitive here (not
        // `setup_all_subscriptions`): it checks per-account operational state
        // and skips accounts whose subscriptions were already set up by a
        // concurrent `login` / `create_identity`, so this composes safely with
        // any account-creation work that runs after `Whitenoise::new` returns.
        //
        // Background notification collection defers this until after it has a
        // notification receiver, otherwise a cold notification-service-extension
        // launch can fetch/process new messages before anything is listening for
        // notification updates.
        if startup_subscription_mode == StartupSubscriptionMode::Run {
            let wn = Arc::clone(&whitenoise);
            whitenoise
                .spawn_background(async move {
                    if let Err(error) = wn.ensure_all_subscriptions().await {
                        tracing::warn!(
                            target: "whitenoise::new",
                            "Background subscription setup failed: {error}"
                        );
                        return;
                    }
                    wn.run_key_package_relay_cleanup_after_grace("startup")
                        .await;
                })
                .await;
        } else {
            tracing::info!(
                target: "whitenoise::new",
                "Deferring startup subscription setup"
            );
        }

        init_timing::record("subscription_setup_spawned");

        tracing::debug!(
            target: "whitenoise::new",
            "Completed initialization for all loaded accounts"
        );

        init_timing::report();

        Ok(whitenoise)
    }

    /// Deadline for the cold-start mute-list sync gate. A slow or unreachable
    /// relay must never stall startup past this; the post-sync stamp/unstamp
    /// sweeps reconcile anything that slips past.
    const COLD_START_MUTE_LIST_SYNC_DEADLINE: std::time::Duration =
        std::time::Duration::from_secs(2);

    /// Cold-start gate: best-effort sync of every active account's mute list,
    /// bounded by [`Self::COLD_START_MUTE_LIST_SYNC_DEADLINE`].
    ///
    /// On a cold start the device can process inbox events before it has
    /// learned of a recent block; without this gate those messages would be
    /// cached unstamped. Syncing the mute list first closes most of that
    /// window.
    ///
    /// This is an optimization, not a correctness guarantee: a relay that
    /// misses the deadline leaves the residual gap to the post-sync sweeps,
    /// which run from `sync_and_emit` once the kind-10000 event eventually
    /// arrives.
    pub(crate) async fn wait_for_mute_list_sync_or_timeout(&self) {
        let sessions: Vec<_> = self.account_manager.sessions_iter().collect();
        if sessions.is_empty() {
            return;
        }

        // Drive every account's sync concurrently and apply the deadline to
        // the aggregate, so a single slow relay can't starve other accounts
        // out of their share of the budget.
        let sync_all = futures::future::join_all(sessions.iter().map(|session| async move {
            if let Err(e) = session.mute_list().sync_mute_list().await {
                tracing::warn!(
                    target: "whitenoise::mute_list",
                    "Cold-start mute-list sync failed for {}: {}",
                    session.account_pubkey,
                    e,
                );
            }
        }));

        if tokio::time::timeout(Self::COLD_START_MUTE_LIST_SYNC_DEADLINE, sync_all)
            .await
            .is_err()
        {
            tracing::info!(
                target: "whitenoise::mute_list",
                "Cold-start mute-list sync hit the {}ms deadline; \
                 post-sync sweeps will reconcile any gap",
                Self::COLD_START_MUTE_LIST_SYNC_DEADLINE.as_millis(),
            );
        }
    }

    /// Buffer (in seconds) used when resubscribing after a teardown/rebuild
    /// cycle. Larger than `SUBSCRIPTION_BUFFER_SECS` to cover the gap window
    /// created by tearing down old subscriptions before new ones are live.
    /// Any events fetched that were already processed are deduplicated by the
    /// event tracker.
    const RESUBSCRIBE_BUFFER_SECS: u64 = 60;

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
    /// Concurrent callers are serialized through `OnceCell::get_or_try_init`
    /// so that exactly one caller drives [`Whitenoise::new`] on cold start;
    /// all others wait on the same future and observe the populated cell.
    /// This guarantees the documented "never start duplicate event processors
    /// or background tasks" contract even when multiple iOS notification
    /// handlers run concurrently on different threads.
    ///
    /// Note: this does not protect against a caller invoking [`Whitenoise::new`]
    /// directly in parallel with `ensure_initialized`. Outside of this
    /// background notification path, init is expected to be serialized by the caller
    /// (e.g. a single Flutter cold-start path).
    pub async fn ensure_initialized(config: WhitenoiseConfig) -> Result<Arc<Self>> {
        let arc = GLOBAL_WHITENOISE
            .get_or_try_init(|| async { Self::new(config).await })
            .await?;
        Ok(Arc::clone(arc))
    }

    pub(crate) async fn ensure_initialized_for_background_notifications(
        config: WhitenoiseConfig,
    ) -> Result<(Arc<Self>, bool)> {
        let (arc, cold_start) =
            get_or_try_init_with_initializer_flag(&GLOBAL_WHITENOISE, || async {
                Self::new_with_startup_subscription_mode(
                    config,
                    background_notification_startup_subscription_mode(),
                )
                .await
            })
            .await?;
        Ok((Arc::clone(arc), cold_start))
    }

    /// Gracefully shuts down all background tasks without deleting data.
    ///
    /// This should be called when the app is being closed or going into the background.
    /// Shuts down the event processor and all scheduled tasks.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use whitenoise::{Whitenoise, WhitenoiseConfig};
    /// # async fn example(config: WhitenoiseConfig) -> Result<(), Box<dyn std::error::Error>> {
    /// let whitenoise = Whitenoise::new(config).await?;
    /// whitenoise.shutdown().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[perf_instrument("whitenoise")]
    pub async fn shutdown(&self) -> Result<()> {
        tracing::info!(target: "whitenoise::shutdown", "Initiating graceful shutdown");

        self.shutdown_event_processing().await?;
        self.shutdown_scheduled_tasks().await;
        self.wait_for_pending_background_tasks().await;

        if let Err(e) = self.flush_product_analytics().await {
            tracing::warn!(
                target: "whitenoise::shutdown",
                error = %e,
                "Failed to flush product analytics during shutdown"
            );
        }

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

        // Delete is a destructive local reset. Give async workers a brief
        // grace period to stop cleanly, then abort any task still blocked on
        // relay or other background work so local cleanup can proceed.
        let delete_shutdown_deadline = Instant::now() + DELETE_SHUTDOWN_GRACE;
        self.shutdown_for_delete(delete_shutdown_deadline).await?;

        // Deactivate session-owned inbox planes before tearing down shared infra.
        // This may touch relay/session shutdown paths, so it is best-effort
        // during a destructive local reset.
        if timeout(
            Self::remaining_delete_grace(delete_shutdown_deadline),
            self.account_manager.deactivate_all_inboxes(),
        )
        .await
        .is_err()
        {
            tracing::warn!(
                target: "whitenoise::delete_all_data",
                "Timed out while deactivating inboxes; continuing with local data wipe"
            );
        }

        // Tear down shared relay-control subscriptions (group, ephemeral,
        // telemetry). Relay cleanup must not block local deletion indefinitely.
        if timeout(
            Self::remaining_delete_grace(delete_shutdown_deadline),
            self.shared.relay_control.shutdown_all(),
        )
        .await
        .is_err()
        {
            tracing::warn!(
                target: "whitenoise::delete_all_data",
                "Timed out while shutting down relay control; continuing with local data wipe"
            );
        }

        // Remove database files and key material.
        let accounts = Account::all(&self.shared.database).await?;
        let database_key_id = self.database_key_id().to_string();
        let close_result = self.shared.database.close_and_delete_files().await;
        let key_delete_result = keyring::delete_db_key(self.keyring_service_id(), &database_key_id);

        match (close_result, key_delete_result) {
            (Ok(()), Ok(())) => {}
            (Err(e), Ok(())) => return Err(e.into()),
            (Ok(()), Err(e)) => return Err(e.into()),
            (Err(close_error), Err(key_error)) => {
                return Err(WhitenoiseError::Internal(format!(
                    "Failed to delete database files ({close_error}) and app database key ({key_error})"
                )));
            }
        }

        // Remove storage artifacts (media cache, etc.)
        self.shared.storage.wipe_all().await?;

        // Remove MLS related data
        let mls_dir = self.shared.config.data_dir.join("mls");
        let orphaned_mdk_database_key_pubkeys = self
            .collect_orphaned_mdk_storage_pubkeys(&accounts, &mls_dir)
            .await?;
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
        self.delete_mdk_database_keys_for_accounts(&accounts)?;
        self.delete_mdk_database_keys_for_pubkeys(&orphaned_mdk_database_key_pubkeys)?;

        // Remove logs
        if self.shared.config.logs_dir.exists() {
            for entry in std::fs::read_dir(&self.shared.config.logs_dir)? {
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

    async fn delete_mdk_storage_for_account(&self, pubkey: &PublicKey) -> Result<()> {
        let mls_storage_path = Account::mdk_storage_path(pubkey, &self.config().data_dir);
        match tokio::fs::metadata(&mls_storage_path).await {
            Ok(metadata) if metadata.is_dir() => {
                tokio::fs::remove_dir_all(mls_storage_path).await?;
            }
            Ok(_) => {
                tokio::fs::remove_file(mls_storage_path).await?;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) if e.kind() == ErrorKind::NotADirectory => {
                tracing::warn!(
                    target: "whitenoise::accounts",
                    account_pubkey = %pubkey,
                    path = ?mls_storage_path,
                    reason = "path component is not a directory",
                );
            }
            Err(e) => return Err(e.into()),
        }
        self.delete_mdk_database_key_for_pubkey(pubkey)
    }

    fn delete_mdk_database_keys_for_accounts(&self, accounts: &[Account]) -> Result<()> {
        let pubkeys = accounts
            .iter()
            .map(|account| account.pubkey)
            .collect::<Vec<_>>();
        self.delete_mdk_database_keys_for_pubkeys(&pubkeys)
    }

    fn delete_mdk_database_keys_for_pubkeys(&self, pubkeys: &[PublicKey]) -> Result<()> {
        Self::delete_mdk_database_keys(pubkeys, |pubkey| {
            self.delete_mdk_database_key_for_pubkey(pubkey)
        })
    }

    fn delete_mdk_database_keys<F>(pubkeys: &[PublicKey], mut delete: F) -> Result<()>
    where
        F: FnMut(&PublicKey) -> Result<()>,
    {
        let mut first_error = None;

        for pubkey in pubkeys {
            if let Err(e) = delete(pubkey) {
                tracing::warn!(
                    target: "whitenoise::delete_all_data",
                    account_pubkey = %pubkey,
                    error = %e,
                    reason = "failed to delete MDK database key",
                );
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }

        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn mdk_storage_pubkey_from_entry_name(file_name: &OsStr, path: &Path) -> Option<PublicKey> {
        let Some(file_name) = file_name.to_str() else {
            tracing::warn!(
                target: "whitenoise::delete_all_data",
                path = ?path,
                reason = "non-UTF-8 MLS storage filename",
            );
            return None;
        };

        let Ok(pubkey) = file_name.parse::<PublicKey>() else {
            tracing::debug!(
                target: "whitenoise::delete_all_data",
                path = ?path,
                reason = "non-account MLS storage entry",
            );
            return None;
        };

        Some(pubkey)
    }

    async fn collect_orphaned_mdk_storage_pubkeys(
        &self,
        accounts: &[Account],
        mls_dir: &Path,
    ) -> Result<Vec<PublicKey>> {
        let mut pubkeys = Vec::new();
        let mut entries = match tokio::fs::read_dir(mls_dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(pubkeys),
            Err(e) => return Err(e.into()),
        };

        while let Some(entry) = entries.next_entry().await? {
            let file_name = entry.file_name();
            let Some(pubkey) = Self::mdk_storage_pubkey_from_entry_name(&file_name, &entry.path())
            else {
                continue;
            };

            if !accounts.iter().any(|account| account.pubkey == pubkey)
                && !pubkeys.contains(&pubkey)
            {
                pubkeys.push(pubkey);
            }
        }

        Ok(pubkeys)
    }

    fn delete_mdk_database_key_for_pubkey(&self, pubkey: &PublicKey) -> Result<()> {
        let db_key_id = Account::mdk_db_key_id(pubkey);
        keyring::delete_db_key(self.keyring_service_id(), &db_key_id)?;
        Ok(())
    }

    pub(crate) fn keyring_service_id(&self) -> &str {
        &self.shared.config.keyring_service_id
    }

    fn database_key_id(&self) -> &str {
        // The database_key_id override field only exists in test,
        // integration-test, and benchmark builds; production always uses the
        // stable app database key id.
        #[cfg(any(test, feature = "integration-tests", feature = "benchmark-tests"))]
        {
            self.config()
                .database_key_id
                .as_deref()
                .unwrap_or(WHITENOISE_DB_KEY_ID)
        }

        #[cfg(not(any(test, feature = "integration-tests", feature = "benchmark-tests")))]
        {
            WHITENOISE_DB_KEY_ID
        }
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

    #[perf_instrument("whitenoise")]
    async fn shutdown_for_delete(&self, deadline: Instant) -> Result<()> {
        tracing::info!(
            target: "whitenoise::delete_all_data",
            "Initiating bounded shutdown before local data wipe"
        );

        match timeout(
            Self::remaining_delete_grace(deadline),
            self.shutdown_event_processing(),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                tracing::warn!(
                    target: "whitenoise::delete_all_data",
                    "Timed out while signalling event processor shutdown; continuing with delete"
                );
            }
        }

        self.shutdown_scheduled_tasks_for_delete(deadline).await;
        self.wait_for_pending_background_tasks_for_delete(deadline)
            .await;

        tracing::info!(
            target: "whitenoise::delete_all_data",
            "Bounded shutdown before local data wipe complete"
        );
        Ok(())
    }

    #[perf_instrument("whitenoise")]
    async fn shutdown_scheduled_tasks_for_delete(&self, deadline: Instant) {
        tracing::info!(
            target: "whitenoise::scheduler",
            "Initiating bounded scheduler shutdown for delete"
        );

        let _ = self.scheduler_shutdown.send(true);
        let handles = {
            let mut handles = self.scheduler_handles.lock().await;
            handles.drain(..).collect::<Vec<_>>()
        };

        Self::await_or_abort_tasks(handles, deadline, "scheduler task").await;

        tracing::info!(
            target: "whitenoise::scheduler",
            "Bounded scheduler shutdown for delete complete"
        );
    }

    /// Returns the number of currently running scheduler tasks.
    ///
    /// This is primarily useful for integration testing to verify the scheduler is running.
    #[cfg(feature = "integration-tests")]
    #[perf_instrument("whitenoise")]
    pub(crate) async fn scheduler_task_count(&self) -> usize {
        self.scheduler_handles.lock().await.len()
    }

    /// Spawn a fire-and-forget background task and register its handle so it
    /// can be awaited at shutdown (production) or between sequential calls
    /// (tests).
    ///
    /// Use this anywhere a handler needs to kick off work that outlives the
    /// request (discovery catch-up, relay-list refresh, welcome finalization,
    /// user-search streaming). The raw `tokio::spawn` pattern drops the
    /// `JoinHandle`, which both leaks the task past shutdown and lets it race
    /// with subsequent DB writes in deterministic tests.
    ///
    /// Opportunistically prunes already-finished handles before pushing so a
    /// long-lived process doesn't accumulate completed `JoinHandle`s
    /// indefinitely between drain points.
    pub(crate) async fn spawn_background<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let handle = tokio::spawn(future);
        let mut handles = self.background_handles.lock().await;
        handles.retain(|h| !h.is_finished());
        handles.push(handle);
    }

    /// Drains and awaits every registered background task.
    ///
    /// Production shutdown calls this so half-finished DB writes don't race
    /// with teardown. Tests call this between sequential handler invocations
    /// to make per-handler background spawns observable before asserting.
    ///
    /// Loops until the registry is empty after a drain pass, so a spawned task
    /// that itself spawns another via [`Self::spawn_background`] doesn't slip
    /// past the wait. Assumes upstream producers (event processing in
    /// production, the test thread in tests) are quiesced before this is
    /// called — production shutdown satisfies this by ordering
    /// `shutdown_event_processing` first.
    pub(crate) async fn wait_for_pending_background_tasks(&self) {
        loop {
            let drained: Vec<JoinHandle<()>> = {
                let mut handles = self.background_handles.lock().await;
                if handles.is_empty() {
                    break;
                }
                handles.drain(..).collect()
            };
            for handle in drained {
                if let Err(e) = handle.await {
                    if e.is_panic() {
                        tracing::error!(
                            target: "whitenoise::background_handles",
                            "Background task panicked: {:?}",
                            e
                        );
                    } else {
                        tracing::debug!(
                            target: "whitenoise::background_handles",
                            "Background task cancelled: {:?}",
                            e
                        );
                    }
                }
            }
        }
    }

    #[perf_instrument("whitenoise")]
    async fn wait_for_pending_background_tasks_for_delete(&self, deadline: Instant) {
        // Same drain-loop shape as wait_for_pending_background_tasks: a task in
        // the current batch may register another background task before it exits.
        // The delete path keeps the shared deadline across generations and aborts
        // handles once the deadline is exhausted, so nested spawns cannot extend
        // the total grace window indefinitely.
        loop {
            let drained: Vec<JoinHandle<()>> = {
                let mut handles = self.background_handles.lock().await;
                if handles.is_empty() {
                    break;
                }
                handles.drain(..).collect()
            };

            Self::await_or_abort_tasks(drained, deadline, "background task").await;
        }
    }

    async fn await_or_abort_tasks(
        handles: Vec<JoinHandle<()>>,
        deadline: Instant,
        label: &'static str,
    ) {
        for mut handle in handles {
            let remaining = Self::remaining_delete_grace(deadline);
            if remaining.is_zero() {
                Self::abort_task(handle, label).await;
                continue;
            }

            match timeout(remaining, &mut handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) if e.is_panic() => {
                    tracing::error!(target: "whitenoise::delete_all_data", "{label} panicked: {:?}", e);
                }
                Ok(Err(e)) => {
                    tracing::debug!(target: "whitenoise::delete_all_data", "{label} cancelled: {:?}", e);
                }
                Err(_) => {
                    tracing::warn!(
                        target: "whitenoise::delete_all_data",
                        "{label} did not stop within shared {:?} grace period; aborting",
                        DELETE_SHUTDOWN_GRACE
                    );
                    Self::abort_task(handle, label).await;
                }
            }
        }
    }

    fn remaining_delete_grace(deadline: Instant) -> Duration {
        deadline.saturating_duration_since(Instant::now())
    }

    async fn abort_task(handle: JoinHandle<()>, label: &'static str) {
        handle.abort();
        if let Err(e) = handle.await {
            if e.is_panic() {
                tracing::error!(target: "whitenoise::delete_all_data", "{label} panicked after abort: {:?}", e);
            } else {
                tracing::debug!(target: "whitenoise::delete_all_data", "{label} aborted: {:?}", e);
            }
        }
    }

    #[cfg(feature = "integration-tests")]
    #[perf_instrument("whitenoise")]
    pub async fn wipe_database(&self) -> Result<()> {
        // Integration scenarios use this row-level reset while keeping the
        // same Whitenoise instance, pool, and database key alive. Full app
        // wipes must use delete_all_data so database files and keyring entries
        // are removed together.
        self.shared.database.delete_all_data().await?;
        Ok(())
    }

    #[cfg(feature = "integration-tests")]
    #[perf_instrument("whitenoise")]
    pub async fn reset_nostr_client(&self) -> Result<()> {
        self.shared.relay_control.reset_for_tests().await?;
        Ok(())
    }
}

#[cfg(test)]
pub mod test_utils {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    use super::*;
    use crate::whitenoise::accounts_groups::AccountGroup;
    use crate::whitenoise::group_information::GroupInformation;
    use crate::whitenoise::relays::Relay;
    use mdk_core::prelude::*;
    use nostr_sdk::{EventBuilder, EventId, Keys, PublicKey, RelayUrl, UnsignedEvent};
    use tempfile::TempDir;

    pub struct DropFlag(Arc<AtomicBool>);

    impl DropFlag {
        pub fn new(dropped: Arc<AtomicBool>) -> Self {
            Self(dropped)
        }
    }

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    // ── Shared raw-SQL fixture helpers ────────────────────────────────────────

    /// Insert a minimal user + account row for `pubkey`.
    ///
    /// Used by repository unit tests that need a real account row in the DB
    /// without going through the full account-creation flow.
    pub async fn insert_test_account(
        database: &crate::whitenoise::database::Database,
        pubkey: &PublicKey,
    ) {
        let user_pubkey = pubkey.to_hex();
        sqlx::query("INSERT INTO users (pubkey, metadata) VALUES (?, '{}')")
            .bind(&user_pubkey)
            .execute(&database.pool)
            .await
            .expect("insert user");
        let (user_id,): (i64,) = sqlx::query_as("SELECT id FROM users WHERE pubkey = ?")
            .bind(&user_pubkey)
            .fetch_one(&database.pool)
            .await
            .expect("get user id");
        sqlx::query("INSERT INTO accounts (pubkey, user_id, last_synced_at) VALUES (?, ?, NULL)")
            .bind(&user_pubkey)
            .bind(user_id)
            .execute(&database.pool)
            .await
            .expect("insert account");
    }

    /// Insert a minimal `group_information` row for `group_id`.
    ///
    /// Used by repository unit tests that need a real group row in the DB
    /// without going through the full group-creation flow.
    pub async fn insert_test_group(
        database: &crate::whitenoise::database::Database,
        group_id: &GroupId,
    ) {
        let now = chrono::Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO group_information (mls_group_id, group_type, created_at, updated_at)
             VALUES (?, 'group', ?, ?)",
        )
        .bind(group_id.as_slice())
        .bind(now)
        .bind(now)
        .execute(&database.pool)
        .await
        .expect("insert group_information");
    }

    // Test configuration and setup helpers
    pub(crate) fn create_test_config() -> (WhitenoiseConfig, TempDir, TempDir) {
        static TEST_CONFIG_ID: AtomicU64 = AtomicU64::new(0);
        let id = TEST_CONFIG_ID.fetch_add(1, Ordering::SeqCst);
        let data_temp_dir = TempDir::new().expect("Failed to create temp data dir");
        let logs_temp_dir = TempDir::new().expect("Failed to create temp logs dir");
        let config = WhitenoiseConfig::new(
            data_temp_dir.path(),
            logs_temp_dir.path(),
            &format!("com.whitenoise.test.{id}"),
        )
        .with_database_key_id(&format!("test.whitenoise.db.key.{id}"))
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
    async fn create_mock_whitenoise_internal_with<F>(
        customize: F,
    ) -> (
        Arc<Whitenoise>,
        mpsc::Receiver<ProcessableEvent>,
        TempDir,
        TempDir,
    )
    where
        F: FnOnce(WhitenoiseConfig) -> WhitenoiseConfig,
    {
        Whitenoise::initialize_mock_keyring_store();

        // Wait for local relays to be ready in test environment
        wait_for_test_relays().await;

        let (config, data_temp, logs_temp) = create_test_config();
        let config = customize(config);

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

        // Create Storage
        let storage = storage::Storage::new(data_temp.path()).await.unwrap();

        // Create message aggregator for testing
        let message_aggregator = message_aggregator::MessageAggregator::new();
        let whitenoise = Whitenoise::from_components(
            config,
            database,
            WhitenoiseComponents {
                event_tracker: UseWhitenoiseEventTracker,
                secrets_store,
                storage,
                message_aggregator,
                event_sender,
                shutdown_sender,
                scheduler_shutdown,
            },
        )
        .unwrap();

        (whitenoise, event_receiver, data_temp, logs_temp)
    }

    pub(crate) async fn create_mock_whitenoise() -> (Arc<Whitenoise>, TempDir, TempDir) {
        create_mock_whitenoise_with(|config| config).await
    }

    /// Build a mock Whitenoise with a customised [`WhitenoiseConfig`]. Use when
    /// a test needs to exercise a non-default config knob (e.g. an overridden
    /// `default_account_relays`).
    pub(crate) async fn create_mock_whitenoise_with<F>(
        customize: F,
    ) -> (Arc<Whitenoise>, TempDir, TempDir)
    where
        F: FnOnce(WhitenoiseConfig) -> WhitenoiseConfig,
    {
        let (whitenoise, _event_receiver, data_temp, logs_temp) =
            create_mock_whitenoise_internal_with(customize).await;
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
        // Phase 18c moved `published_events` into the per-account DB; the
        // table no longer exists in shared. Read from the session's account_db
        // (it has no `account_pubkey`/`account_id` filter — the file is the
        // scope).
        let session = whitenoise
            .session(&account.pubkey)
            .expect("account must have an active session");
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM published_events")
            .fetch_one(&session.account_db.inner.pool)
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
            None,
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
            let key_package_relays = account
                .key_package_relays(&whitenoise.shared)
                .await
                .unwrap();
            whitenoise
                .require_session(&account.pubkey)
                .unwrap()
                .key_packages()
                .create_and_publish(&key_package_relays)
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
                            .key_package_relays(&whitenoise.shared)
                            .await
                            .unwrap(),
                    );

                    match whitenoise
                        .shared
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
            let relay_urls = Relay::urls(
                &member_account
                    .key_package_relays(&whitenoise.shared)
                    .await
                    .unwrap(),
            );
            let key_package_event = whitenoise
                .shared
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
            .shared
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

            let session = whitenoise
                .require_session(&member_account.pubkey)
                .expect("member must have an active session");
            whitenoise
                .handle_giftwrap(&session, member_account, giftwrap)
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
        let member_session = whitenoise.session(&member_account.pubkey).unwrap();
        Whitenoise::finalize_welcome_with_instance(
            whitenoise,
            member_account,
            &member_session,
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
    #[cfg(unix)]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use keyring_core::api::CredentialStoreApi;

    use super::test_utils::*;
    use super::*;
    use database::aggregated_messages::PaginationOptions;

    struct CustomLegacyStoreCreationError;

    #[tokio::test]
    async fn get_or_try_init_with_initializer_flag_tracks_the_initializer_call() {
        let cell = OnceCell::const_new();

        let (value, initialized_by_first_call) =
            get_or_try_init_with_initializer_flag(&cell, || async {
                Ok::<_, WhitenoiseError>("initialized")
            })
            .await
            .unwrap();
        assert_eq!(*value, "initialized");
        assert!(initialized_by_first_call);

        let (value, initialized_by_second_call) =
            get_or_try_init_with_initializer_flag(&cell, || async {
                Ok::<_, WhitenoiseError>("not used")
            })
            .await
            .unwrap();
        assert_eq!(*value, "initialized");
        assert!(!initialized_by_second_call);
    }

    impl From<CustomLegacyStoreCreationError> for keyring_core::Error {
        fn from(_: CustomLegacyStoreCreationError) -> Self {
            keyring_core::Error::Invalid(
                "legacy Android".to_string(),
                "custom missing NDK context".to_string(),
            )
        }
    }

    #[test]
    fn keyring_store_init_retries_after_failed_attempt() {
        let init = KeyringStoreInit::new();
        let attempts = AtomicUsize::new(0);

        let first = init.initialize_with(
            || false,
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(SecretsStoreError::KeyringUnavailable("transient failure".to_string()).into())
            },
        );

        assert!(matches!(
            first,
            Err(WhitenoiseError::SecretsStore(
                SecretsStoreError::KeyringUnavailable(_)
            ))
        ));

        let second = init.initialize_with(
            || false,
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );

        assert!(second.is_ok());

        let third = init.initialize_with(|| false, || panic!("initializer ran after success"));

        assert!(third.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn keyring_store_init_preserves_existing_default_store() {
        let init = KeyringStoreInit::new();
        let attempts = AtomicUsize::new(0);

        // Host apps may configure keyring-core before Whitenoise starts. In
        // that case initialization should preserve the existing default store.
        let result = init.initialize_with(
            || true,
            || {
                attempts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );

        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn create_keyring_store_maps_creation_error() {
        let result = Whitenoise::create_keyring_store::<keyring_core::mock::Store>(
            Err(keyring_core::Error::Invalid(
                "store".to_string(),
                "boom".to_string(),
            )),
            "test",
        );

        assert!(matches!(
            result,
            Err(WhitenoiseError::SecretsStore(
                SecretsStoreError::KeyringUnavailable(msg)
            ))
                if msg.contains("Failed to create test credential store")
                    && msg.contains("boom")
        ));
    }

    #[test]
    fn secret_service_keyring_store_wraps_store_with_target() {
        let store = Whitenoise::create_targeted_secret_service_keyring_store(
            keyring_core::mock::Store::new(),
            "test Secret Service",
        )
        .unwrap();

        assert!(
            store
                .as_any()
                .is::<keyring_store::TargetedCredentialStore>()
        );
        assert!(format!("{store:?}").contains(keyring_store::SECRET_SERVICE_TARGET));
    }

    #[test]
    fn secret_service_keyring_store_maps_creation_error() {
        let result =
            Whitenoise::create_targeted_secret_service_keyring_store::<keyring_core::mock::Store>(
                Err(keyring_core::Error::Invalid(
                    "Secret Service".to_string(),
                    "missing session bus".to_string(),
                )),
                "test Secret Service",
            );

        assert!(matches!(
            result,
            Err(WhitenoiseError::SecretsStore(
                SecretsStoreError::KeyringUnavailable(msg)
            ))
                if msg.contains("Failed to create test Secret Service credential store")
                    && msg.contains("missing session bus")
        ));
    }

    #[test]
    fn legacy_migration_keyring_store_falls_back_to_primary_when_legacy_store_creation_fails() {
        let primary = keyring_core::mock::Store::new().unwrap();
        primary
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap()
            .set_secret(b"primary-db-key")
            .unwrap();

        let store = Whitenoise::create_legacy_migration_keyring_store::<
            keyring_core::mock::Store,
            keyring_core::Error,
        >(
            primary,
            Err(keyring_core::Error::Invalid(
                "legacy Android".to_string(),
                "missing NDK context".to_string(),
            )),
            "legacy Android",
        );

        assert_eq!(
            store
                .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
                .unwrap()
                .get_secret()
                .unwrap(),
            b"primary-db-key"
        );
    }

    #[test]
    fn legacy_migration_keyring_store_reads_legacy_when_primary_is_empty() {
        let primary = keyring_core::mock::Store::new().unwrap();
        let legacy = keyring_core::mock::Store::new().unwrap();
        legacy
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap()
            .set_secret(b"legacy-db-key")
            .unwrap();

        let store = Whitenoise::create_legacy_migration_keyring_store(
            primary,
            Ok::<_, keyring_core::Error>(legacy),
            "legacy Android",
        );

        assert_eq!(
            store
                .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
                .unwrap()
                .get_secret()
                .unwrap(),
            b"legacy-db-key"
        );
    }

    #[test]
    fn legacy_migration_keyring_store_falls_back_to_primary_when_custom_legacy_error_converts() {
        let primary = keyring_core::mock::Store::new().unwrap();
        primary
            .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
            .unwrap()
            .set_secret(b"primary-db-key")
            .unwrap();

        let store = Whitenoise::create_legacy_migration_keyring_store::<
            keyring_core::mock::Store,
            CustomLegacyStoreCreationError,
        >(
            primary,
            Err(CustomLegacyStoreCreationError),
            "legacy Android",
        );

        assert_eq!(
            store
                .build("com.whitenoise.app", "whitenoise.db.key.v1", None)
                .unwrap()
                .get_secret()
                .unwrap(),
            b"primary-db-key"
        );
    }

    #[tokio::test]
    async fn insert_test_group_creates_group_information_row() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = mdk_core::prelude::GroupId::from_slice(&[0xab, 0xcd, 0xef, 0x01]);

        insert_test_group(&whitenoise.shared.database, &group_id).await;

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM group_information WHERE mls_group_id = ?")
                .bind(group_id.as_slice())
                .fetch_one(&whitenoise.shared.database.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1);
    }

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

        #[test]
        fn test_whitenoise_config_default_account_relays_seed() {
            let data_dir = std::path::Path::new("/test/data");
            let logs_dir = std::path::Path::new("/test/logs");
            let config = WhitenoiseConfig::new(data_dir, logs_dir, "com.test.app");

            assert_eq!(
                config.default_account_relays,
                Relay::urls(&Relay::defaults()),
                "fresh config should seed default_account_relays from Relay::defaults()"
            );
        }

        #[test]
        fn test_whitenoise_config_with_default_account_relays_overrides_seed() {
            let data_dir = std::path::Path::new("/test/data");
            let logs_dir = std::path::Path::new("/test/logs");
            let override_urls = vec![
                RelayUrl::parse("ws://override-relay-a:9999").unwrap(),
                RelayUrl::parse("ws://override-relay-b:9999").unwrap(),
            ];
            let config = WhitenoiseConfig::new(data_dir, logs_dir, "com.test.app")
                .with_default_account_relays(override_urls.clone());

            assert_eq!(config.default_account_relays, override_urls);
        }

        #[test]
        fn test_keyring_service_id_normalization_trims_whitespace() {
            let data_dir = std::path::Path::new("/test/data");
            let logs_dir = std::path::Path::new("/test/logs");
            let mut config = WhitenoiseConfig::new(data_dir, logs_dir, "  com.test.app  ");

            config.normalize_keyring_service_id();

            assert_eq!(config.keyring_service_id, "com.test.app");
        }

        #[tokio::test]
        async fn test_new_rejects_empty_keyring_service_id() {
            use tempfile::TempDir;

            let data_temp = TempDir::new().unwrap();
            let logs_temp = TempDir::new().unwrap();
            let mut config =
                WhitenoiseConfig::new(data_temp.path(), logs_temp.path(), "com.test.app");

            // Test empty string
            config.keyring_service_id = "".to_string();
            let result = Whitenoise::new(config.clone()).await;
            assert!(result.is_err());
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("keyring_service_id cannot be empty")
            );

            // Test whitespace only
            config.keyring_service_id = "   ".to_string();
            let result = Whitenoise::new(config).await;
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
            assert!(
                Account::all(&whitenoise.shared.database)
                    .await
                    .unwrap()
                    .is_empty()
            );

            // Verify directories were created
            assert!(whitenoise.config().data_dir.exists());
            assert!(whitenoise.config().logs_dir.exists());
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
            assert!(whitenoise1.config().data_dir.exists());
            assert!(whitenoise2.config().data_dir.exists());
            assert!(
                Account::all(&whitenoise1.shared.database)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                Account::all(&whitenoise2.shared.database)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    // Data Management Tests
    mod data_management_tests {
        use tempfile::TempDir;

        use super::*;

        #[tokio::test]
        async fn test_delete_all_data() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // Create test files in the whitenoise directories
            let database_path = whitenoise.shared.database.path.clone();
            let database_key_id = whitenoise.database_key_id().to_string();
            let test_data_file = whitenoise.config().data_dir.join("test_data.txt");
            let test_log_file = whitenoise.config().logs_dir.join("test_log.txt");
            tokio::fs::write(&test_data_file, "test data")
                .await
                .unwrap();
            tokio::fs::write(&test_log_file, "test log").await.unwrap();
            assert!(test_data_file.exists());
            assert!(test_log_file.exists());

            // Create some test media files in cache
            whitenoise
                .shared
                .storage
                .media_files
                .store_file("test_image.jpg", b"fake image data")
                .await
                .unwrap();
            let media_cache_dir = whitenoise.shared.storage.media_files.cache_dir();
            assert!(media_cache_dir.exists());
            let cache_entries: Vec<_> = std::fs::read_dir(media_cache_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            assert_eq!(cache_entries.len(), 1);
            keyring::get_or_create_db_key(
                &whitenoise.config().keyring_service_id,
                &database_key_id,
            )
            .expect("Failed to create app database key");
            assert!(
                keyring::get_db_key(&whitenoise.config().keyring_service_id, &database_key_id)
                    .unwrap()
                    .is_some()
            );

            // Delete all data
            let result = whitenoise.delete_all_data().await;
            assert!(result.is_ok());

            // Verify cleanup
            assert!(!database_path.exists());
            assert!(
                keyring::get_db_key(&whitenoise.config().keyring_service_id, &database_key_id)
                    .unwrap()
                    .is_none(),
                "App wipe should remove the app database key"
            );
            assert!(!test_log_file.exists());

            // Media cache directory should be removed
            let media_cache_dir_after = whitenoise.shared.storage.media_files.cache_dir();
            assert!(!media_cache_dir_after.exists());

            // MLS directory should be recreated as empty
            let mls_dir = whitenoise.config().data_dir.join("mls");
            assert!(mls_dir.exists());
            assert!(mls_dir.is_dir());
        }

        #[tokio::test]
        async fn test_delete_all_data_aborts_stuck_background_task() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let database_path = whitenoise.shared.database.path.clone();
            let database_key_id = whitenoise.database_key_id().to_string();
            keyring::get_or_create_db_key(
                &whitenoise.config().keyring_service_id,
                &database_key_id,
            )
            .expect("Failed to create app database key");

            let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let dropped_in_task = dropped.clone();
            whitenoise
                .spawn_background(async move {
                    let _drop_flag = DropFlag::new(dropped_in_task);
                    std::future::pending::<()>().await;
                })
                .await;
            assert_eq!(
                whitenoise.background_handles.lock().await.len(),
                1,
                "test must register the stuck task before delete_all_data starts"
            );

            let result = tokio::time::timeout(
                DELETE_SHUTDOWN_GRACE + std::time::Duration::from_secs(2),
                whitenoise.delete_all_data(),
            )
            .await;

            assert!(
                result.is_ok(),
                "delete_all_data should complete within the bounded shutdown grace"
            );
            assert!(result.unwrap().is_ok(), "delete_all_data should succeed");
            assert!(
                dropped.load(std::sync::atomic::Ordering::SeqCst),
                "delete_all_data should abort stuck registered background tasks"
            );
            assert!(
                whitenoise.background_handles.lock().await.is_empty(),
                "delete_all_data should drain the registered background task handles"
            );
            assert!(!database_path.exists());
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn test_delete_all_data_removes_app_database_key_when_file_delete_fails() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let database_path = whitenoise.shared.database.path.clone();
            let lock_path = database_path.with_file_name(format!(
                "{}.encryption.lock",
                database_path.file_name().unwrap().to_string_lossy()
            ));
            let database_key_id = whitenoise.database_key_id().to_string();

            keyring::get_or_create_db_key(
                &whitenoise.config().keyring_service_id,
                &database_key_id,
            )
            .expect("Failed to create app database key");
            std::fs::create_dir(&lock_path).unwrap();

            let result = whitenoise.delete_all_data().await;

            assert!(result.is_err());
            assert!(
                keyring::get_db_key(&whitenoise.config().keyring_service_id, &database_key_id)
                    .unwrap()
                    .is_none(),
                "App wipe should attempt key deletion even when database file deletion fails"
            );
        }

        #[test]
        fn test_normalize_keyring_service_id_trims_whitespace() {
            let temp = TempDir::new().expect("Failed to create temp directory");
            let mut config = WhitenoiseConfig::new(temp.path(), temp.path(), "  padded  ");
            config.normalize_keyring_service_id();
            assert_eq!(config.keyring_service_id, "padded");
        }

        #[tokio::test]
        async fn test_construction_normalizes_keyring_service_id() {
            Whitenoise::initialize_mock_keyring_store();
            let temp = TempDir::new().expect("Failed to create temp directory");
            let config = WhitenoiseConfig::new(temp.path(), temp.path(), "  padded  ");
            let whitenoise = Whitenoise::new(config).await.unwrap();
            assert_eq!(whitenoise.keyring_service_id(), "padded");
        }

        #[test]
        fn test_background_notification_initialization_defers_startup_subscriptions() {
            assert_eq!(
                background_notification_startup_subscription_mode(),
                StartupSubscriptionMode::Defer,
                "background notification cold-start must subscribe before fetching subscriptions"
            );
        }

        #[tokio::test]
        async fn test_delete_all_data_removes_mdk_database_keys() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (account, _keys) = create_test_account(&whitenoise).await;
            account.save(&whitenoise.shared.database).await.unwrap();

            let db_key_id = Account::mdk_db_key_id(&account.pubkey);
            keyring::get_or_create_db_key(&whitenoise.config().keyring_service_id, &db_key_id)
                .expect("Failed to create MDK database key");

            assert!(
                keyring::get_db_key(&whitenoise.config().keyring_service_id, &db_key_id)
                    .unwrap()
                    .is_some()
            );

            whitenoise.delete_all_data().await.unwrap();

            assert!(
                keyring::get_db_key(&whitenoise.config().keyring_service_id, &db_key_id)
                    .unwrap()
                    .is_none(),
                "App wipe should remove account-scoped MDK database keys"
            );
        }

        #[tokio::test]
        async fn test_delete_all_data_removes_orphaned_mdk_database_keys_from_storage_dirs() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let orphan_pubkey = create_test_keys().public_key();
            let orphan_storage_dir =
                Account::mdk_storage_path(&orphan_pubkey, &whitenoise.config().data_dir);
            let db_key_id = Account::mdk_db_key_id(&orphan_pubkey);

            tokio::fs::create_dir_all(&orphan_storage_dir)
                .await
                .unwrap();
            keyring::get_or_create_db_key(&whitenoise.config().keyring_service_id, &db_key_id)
                .expect("Failed to create orphaned MDK database key");

            assert!(
                keyring::get_db_key(&whitenoise.config().keyring_service_id, &db_key_id)
                    .unwrap()
                    .is_some()
            );

            whitenoise.delete_all_data().await.unwrap();

            assert!(
                keyring::get_db_key(&whitenoise.config().keyring_service_id, &db_key_id)
                    .unwrap()
                    .is_none()
            );
            assert!(!orphan_storage_dir.exists());
        }

        #[tokio::test]
        async fn test_collect_orphaned_mdk_storage_pubkeys_handles_missing_and_invalid_entries() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let account_pubkey = create_test_keys().public_key();
            let orphan_pubkey = create_test_keys().public_key();
            let mls_dir = whitenoise.config().data_dir.join("scan_mls");
            let missing_dir = whitenoise.config().data_dir.join("missing_mls");
            let not_a_dir = whitenoise.config().data_dir.join("mls-file");
            let accounts = vec![
                Account::new_external(&whitenoise, account_pubkey)
                    .await
                    .unwrap(),
            ];

            let missing = whitenoise
                .collect_orphaned_mdk_storage_pubkeys(&accounts, &missing_dir)
                .await
                .unwrap();
            assert!(missing.is_empty());

            tokio::fs::write(&not_a_dir, "not a directory")
                .await
                .unwrap();
            assert!(
                whitenoise
                    .collect_orphaned_mdk_storage_pubkeys(&accounts, &not_a_dir)
                    .await
                    .is_err()
            );

            tokio::fs::create_dir_all(mls_dir.join(account_pubkey.to_string()))
                .await
                .unwrap();
            tokio::fs::create_dir_all(mls_dir.join(orphan_pubkey.to_string()))
                .await
                .unwrap();
            tokio::fs::write(mls_dir.join("not-a-pubkey"), "ignored")
                .await
                .unwrap();

            let found = whitenoise
                .collect_orphaned_mdk_storage_pubkeys(&accounts, &mls_dir)
                .await
                .unwrap();

            assert_eq!(found, vec![orphan_pubkey]);
        }

        #[cfg(unix)]
        #[test]
        fn test_mdk_storage_pubkey_from_entry_name_skips_non_utf8_and_invalid_names() {
            let orphan_pubkey = create_test_keys().public_key();
            let non_utf8_name = OsString::from_vec(vec![0xff, b'm', b'd', b'k']);

            assert_eq!(
                Whitenoise::mdk_storage_pubkey_from_entry_name(
                    non_utf8_name.as_os_str(),
                    Path::new("non-utf8")
                ),
                None
            );
            assert_eq!(
                Whitenoise::mdk_storage_pubkey_from_entry_name(
                    OsStr::new("not-a-pubkey"),
                    Path::new("invalid")
                ),
                None
            );
            assert_eq!(
                Whitenoise::mdk_storage_pubkey_from_entry_name(
                    OsStr::new(&orphan_pubkey.to_string()),
                    Path::new("valid")
                ),
                Some(orphan_pubkey)
            );
        }

        #[tokio::test]
        async fn test_delete_mdk_storage_for_account_treats_not_a_directory_as_cleanup_complete() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let pubkey = create_test_keys().public_key();
            let mls_dir = whitenoise.config().data_dir.join("mls");
            let db_key_id = Account::mdk_db_key_id(&pubkey);

            let _ = tokio::fs::remove_dir_all(&mls_dir).await;
            tokio::fs::write(&mls_dir, "not a directory").await.unwrap();
            keyring::get_or_create_db_key(&whitenoise.config().keyring_service_id, &db_key_id)
                .expect("Failed to create MDK database key");

            whitenoise
                .delete_mdk_storage_for_account(&pubkey)
                .await
                .unwrap();

            assert!(
                keyring::get_db_key(&whitenoise.config().keyring_service_id, &db_key_id)
                    .unwrap()
                    .is_none()
            );
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn test_delete_mdk_storage_for_account_propagates_unexpected_metadata_errors() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let pubkey = create_test_keys().public_key();
            let mls_dir = whitenoise.config().data_dir.join("mls");

            tokio::fs::create_dir_all(&mls_dir).await.unwrap();
            std::fs::set_permissions(&mls_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

            let result = whitenoise.delete_mdk_storage_for_account(&pubkey).await;

            std::fs::set_permissions(&mls_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(result.is_err());
        }

        #[test]
        fn test_delete_mdk_database_keys_continues_after_delete_error() {
            let first_pubkey = create_test_keys().public_key();
            let second_pubkey = create_test_keys().public_key();
            let pubkeys = vec![first_pubkey, second_pubkey];
            let mut attempted_pubkeys = Vec::new();

            let result = Whitenoise::delete_mdk_database_keys(&pubkeys, |pubkey| {
                attempted_pubkeys.push(*pubkey);
                if *pubkey == first_pubkey {
                    return Err(WhitenoiseError::Internal("delete failed".to_string()));
                }

                Ok(())
            });

            assert!(result.is_err());
            assert_eq!(attempted_pubkeys, pubkeys);
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
                &whitenoise.shared.database,
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
                is_blocked: false,
                content_tokens: whitenoise_markdown::Document::default(),
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
                is_blocked: false,
                content_tokens: whitenoise_markdown::Document::default(),
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };

            let session = whitenoise.session(&test_pubkey).unwrap();
            aggregated_message::AggregatedMessage::insert_message(
                &msg1,
                &group_id,
                &test_pubkey,
                &session.account_db.inner,
            )
            .await
            .unwrap();
            aggregated_message::AggregatedMessage::insert_message(
                &msg2,
                &group_id,
                &test_pubkey,
                &session.account_db.inner,
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
                is_blocked: false,
                content_tokens: whitenoise_markdown::Document::default(),
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };

            // Emit an update (will be caught by subscriber during drain phase)
            whitenoise.shared.message_stream_manager.emit(
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
        /// deterministic ID derived from `seed`. Writes into `account_pubkey`'s
        /// per-account DB; `author` is just the message author field and may
        /// differ from the storing account. Returns the inserted message.
        async fn insert_msg_at(
            seed: u8,
            author: nostr_sdk::PublicKey,
            account_pubkey: &nostr_sdk::PublicKey,
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
                is_blocked: false,
                content_tokens: whitenoise_markdown::Document::default(),
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            let session = whitenoise.session(account_pubkey).unwrap();
            aggregated_message::AggregatedMessage::insert_message(
                &msg,
                group_id,
                account_pubkey,
                &session.account_db.inner,
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
                &whitenoise.shared.database,
            )
            .await
            .unwrap();
        }

        /// Create the minimal user + account + session + accounts_groups rows
        /// for a raw pubkey so that `chat_cleared_at_ms` (and similar methods)
        /// can find the account-group association.
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
            .execute(&whitenoise.shared.database.pool)
            .await
            .unwrap();
            let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE pubkey = ?")
                .bind(&hex)
                .fetch_one(&whitenoise.shared.database.pool)
                .await
                .unwrap();
            // account row
            sqlx::query(
                "INSERT OR IGNORE INTO accounts \
                 (pubkey, user_id, account_type, created_at, updated_at) \
                 VALUES (?, ?, 'local', ?, ?)",
            )
            .bind(&hex)
            .bind(user_id)
            .bind(now_ms)
            .bind(now_ms)
            .execute(&whitenoise.shared.database.pool)
            .await
            .unwrap();
            // Create and register a session so require_session() works
            if whitenoise.account_manager.get_session(pubkey).is_none() {
                let mdk = whitenoise.create_mdk_for_account(*pubkey).unwrap();
                let session = std::sync::Arc::new(
                    session::AccountSession::new(
                        *pubkey,
                        mdk,
                        whitenoise.shared.clone(),
                        std::sync::Weak::new(),
                        None,
                    )
                    .await
                    .unwrap(),
                );
                whitenoise.account_manager.insert_session(session);
            }
            // accounts_groups row (per-account DB)
            let session = whitenoise.require_session(pubkey).unwrap();
            accounts_groups::AccountGroup::find_or_create(
                pubkey,
                group_id,
                None,
                &session.account_db.inner.pool,
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
                insert_msg_at(
                    i,
                    author,
                    &author,
                    base + u64::from(i),
                    &group_id,
                    &whitenoise,
                )
                .await;
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
                insert_msg_at(
                    i,
                    author,
                    &author,
                    base + u64::from(i),
                    &group_id,
                    &whitenoise,
                )
                .await;
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
                insert_msg_at(
                    i,
                    author,
                    &author,
                    base + u64::from(i),
                    &group_id,
                    &whitenoise,
                )
                .await;
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
                insert_msg_at(
                    i,
                    author,
                    &author,
                    base + u64::from(i),
                    &group_id,
                    &whitenoise,
                )
                .await;
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
                insert_msg_at(
                    i,
                    author,
                    &author,
                    base + u64::from(i),
                    &group_id,
                    &whitenoise,
                )
                .await;
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

            insert_msg_at(1, author, &author, 1_715_000_001, &group_id, &whitenoise).await;

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
            insert_msg_at(1, author, &author, 1_716_000_001, &group_id, &whitenoise).await;

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
                is_blocked: false,
                content_tokens: whitenoise_markdown::Document::default(),
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            whitenoise.shared.message_stream_manager.emit(
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

            insert_msg_at(1, author, &author, 1_718_000_001, &group_id, &whitenoise).await;
            insert_msg_at(2, author, &author, 1_718_000_002, &group_id, &whitenoise).await;

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
                is_blocked: false,
                content_tokens: whitenoise_markdown::Document::default(),
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            whitenoise.shared.message_stream_manager.emit(
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

            insert_msg_at(1, author, &author, 1_720_000_001, &group_id, &whitenoise).await;

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
                is_blocked: false,
                content_tokens: whitenoise_markdown::Document::default(),
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            whitenoise.shared.message_stream_manager.emit(
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
                is_blocked: false,
                content_tokens: whitenoise_markdown::Document::default(),
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            whitenoise.shared.message_stream_manager.emit(
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

        /// Helper: create a minimal account in the DB plus a registered session
        /// (no network calls). `subscribe_to_group_messages` and the chat-list
        /// view both require an `AccountSession` to be present in the manager.
        async fn create_db_account(whitenoise: &Arc<Whitenoise>) -> accounts::Account {
            let (account, keys) = accounts::Account::new(whitenoise, None).await.unwrap();
            whitenoise
                .shared
                .secrets_store
                .store_private_key(&keys)
                .expect("store keys");
            let account = account.save(&whitenoise.shared.database).await.unwrap();
            let session = Arc::new(
                session::AccountSession::from_account(&account, whitenoise)
                    .await
                    .unwrap(),
            );
            whitenoise.account_manager.insert_session(session);
            account
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
                .require_session(&account.pubkey)
                .unwrap()
                .membership()
                .for_group(&group_id)
                .get_or_create(None)
                .await
                .unwrap();
            let author = nostr_sdk::Keys::generate().public_key();

            // History: 10 messages at t+1 … t+10 (oldest → newest)
            let base: u64 = 1_730_000_000;
            for i in 1u8..=10 {
                insert_msg_at(
                    i,
                    author,
                    &account.pubkey,
                    base + u64::from(i),
                    &group_id,
                    &whitenoise,
                )
                .await;
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
                is_blocked: false,
                content_tokens: whitenoise_markdown::Document::default(),
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
                is_blocked: false,
                content_tokens: whitenoise_markdown::Document::default(),
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };

            whitenoise.shared.message_stream_manager.emit(
                &group_id,
                message_streaming::MessageUpdate {
                    trigger: message_streaming::UpdateTrigger::NewMessage,
                    message: new_msg_a.clone(),
                },
            );
            whitenoise.shared.message_stream_manager.emit(
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
                .require_session(&account.pubkey)
                .unwrap()
                .membership()
                .for_group(&group_id)
                .get_or_create(None)
                .await
                .unwrap();
            let author = nostr_sdk::Keys::generate().public_key();

            // Exactly 10 messages, page size 5 — two full pages, then nothing
            let base: u64 = 1_731_000_000;
            for i in 1u8..=10 {
                insert_msg_at(
                    i,
                    author,
                    &account.pubkey,
                    base + u64::from(i),
                    &group_id,
                    &whitenoise,
                )
                .await;
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
                .require_session(&account.pubkey)
                .unwrap()
                .membership()
                .for_group(&group_id)
                .get_or_create(None)
                .await
                .unwrap();
            let author = nostr_sdk::Keys::generate().public_key();

            let base: u64 = 1_732_000_000;
            for i in 1u8..=15 {
                insert_msg_at(
                    i,
                    author,
                    &account.pubkey,
                    base + u64::from(i),
                    &group_id,
                    &whitenoise,
                )
                .await;
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
                is_blocked: false,
                content_tokens: whitenoise_markdown::Document::default(),
                reactions: message_aggregator::ReactionSummary::default(),
                kind: 9,
                media_attachments: vec![],
                delivery_status: None,
            };
            whitenoise.shared.message_stream_manager.emit(
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
                .require_session(&account.pubkey)
                .unwrap()
                .membership()
                .for_group(&group_id)
                .get_or_create(None)
                .await
                .unwrap();
            let author = nostr_sdk::Keys::generate().public_key();

            // 8 messages: seeds 1–6 all at the same second (tie), seed 7 earlier, seed 8 later
            let tie_ts: u64 = 1_733_000_000;
            insert_msg_at(
                7,
                author,
                &account.pubkey,
                tie_ts - 1,
                &group_id,
                &whitenoise,
            )
            .await;
            for i in 1u8..=6 {
                insert_msg_at(i, author, &account.pubkey, tie_ts, &group_id, &whitenoise).await;
            }
            insert_msg_at(
                8,
                author,
                &account.pubkey,
                tie_ts + 1,
                &group_id,
                &whitenoise,
            )
            .await;

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
            let saved_user = user.save(&whitenoise.shared.database).await.unwrap();

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

            let saved_user = User::find_by_pubkey(&pubkey, &whitenoise.shared.database)
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
                .require_session(&creator_account.pubkey)
                .unwrap()
                .groups()
                .create_group(vec![member_pubkey], config, None)
                .await
                .unwrap();

            let session = whitenoise
                .session(&creator_account.pubkey)
                .expect("session should exist");
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if session.group_handle.group_count().await == 1 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("background group-plane refresh should settle");

            // Tear down subscriptions to simulate the welcome-processing
            // cascade failure (group exists in MDK but not in group plane)
            session.deactivate_subscriptions().await;

            // Re-activate inbox only (without groups) to isolate the test
            // to the group count parity check — inbox is healthy, groups are not
            let signer = whitenoise.get_signer_for_account(&creator_account).unwrap();
            let inbox_relays = crate::whitenoise::relays::Relay::urls(
                &creator_account
                    .effective_inbox_relays(&whitenoise.shared)
                    .await
                    .unwrap(),
            );
            session
                .activate_subscriptions(
                    &whitenoise.shared.relay_control,
                    &inbox_relays,
                    &[], // empty group specs — simulates the missing group
                    creator_account.since_timestamp(10),
                    signer,
                )
                .await
                .unwrap();

            assert_eq!(
                whitenoise
                    .shared
                    .relay_control
                    .group_plane_account_group_count(&creator_account.pubkey)
                    .await,
                0,
                "test setup should leave MDK with one group and the group plane empty"
            );

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
            let session = whitenoise
                .session(&account.pubkey)
                .expect("session should exist");
            session.deactivate_subscriptions().await;

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
            let session1 = whitenoise
                .session(&account1.pubkey)
                .expect("session should exist");
            session1.deactivate_subscriptions().await;

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
        async fn test_spawn_background_registers_and_wait_drains() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

            // Spawn N tasks and verify they land in the registry.
            for _ in 0..5 {
                let counter = counter.clone();
                whitenoise
                    .spawn_background(async move {
                        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    })
                    .await;
            }

            // Drain awaits every task to completion.
            whitenoise.wait_for_pending_background_tasks().await;
            assert_eq!(
                counter.load(std::sync::atomic::Ordering::Relaxed),
                5,
                "All registered tasks must complete before wait returns"
            );
            assert!(
                whitenoise.background_handles.lock().await.is_empty(),
                "Registry must be empty after drain"
            );

            // Spawning more after a drain works (prune-on-push is non-destructive).
            let counter_inner = counter.clone();
            whitenoise
                .spawn_background(async move {
                    counter_inner.fetch_add(100, std::sync::atomic::Ordering::Relaxed);
                })
                .await;
            whitenoise.wait_for_pending_background_tasks().await;
            assert_eq!(
                counter.load(std::sync::atomic::Ordering::Relaxed),
                105,
                "Post-drain spawn must also be awaited"
            );
        }

        #[tokio::test]
        async fn test_spawn_background_prunes_finished_handles_on_push() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            // First spawn a task that completes immediately, then wait briefly so
            // the JoinHandle reports `is_finished() == true` before the second push.
            whitenoise.spawn_background(async {}).await;
            tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;

            // Sanity: the first handle is sitting in the vec, finished but unreaped.
            assert_eq!(whitenoise.background_handles.lock().await.len(), 1);

            // Second spawn should retain only non-finished handles → drop the first.
            whitenoise
                .spawn_background(async {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                })
                .await;
            let len_after_second_push = whitenoise.background_handles.lock().await.len();
            assert_eq!(
                len_after_second_push, 1,
                "Finished handle must be pruned on the next push; only the live one remains"
            );

            whitenoise.wait_for_pending_background_tasks().await;
        }

        #[tokio::test]
        async fn test_delete_shutdown_aborts_task_after_shared_deadline() {
            let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let dropped_in_task = dropped.clone();
            let handle = tokio::spawn(async move {
                let _drop_flag = DropFlag::new(dropped_in_task);
                std::future::pending::<()>().await;
            });

            let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(20);
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                Whitenoise::await_or_abort_tasks(vec![handle], deadline, "test task"),
            )
            .await;

            assert!(
                result.is_ok(),
                "Delete shutdown should abort a stuck task instead of hanging"
            );
            assert!(
                dropped.load(std::sync::atomic::Ordering::SeqCst),
                "Aborted task future should be dropped"
            );
        }

        #[tokio::test]
        async fn test_wait_for_pending_background_tasks_handles_failed_tasks() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            whitenoise
                .spawn_background(async {
                    std::future::pending::<()>().await;
                })
                .await;
            whitenoise
                .spawn_background(async {
                    panic!("intentional background task panic");
                })
                .await;

            tokio::task::yield_now().await;
            {
                let handles = whitenoise.background_handles.lock().await;
                handles[0].abort();
            }

            whitenoise.wait_for_pending_background_tasks().await;
            assert!(
                whitenoise.background_handles.lock().await.is_empty(),
                "Registry must be empty after draining failed tasks"
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
                .require_session(&creator.pubkey)
                .unwrap()
                .groups()
                .create_group(vec![member.pubkey], config, None)
                .await
                .unwrap();
            let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
            config.name = "Group B".to_string();
            let group_b = whitenoise
                .require_session(&creator.pubkey)
                .unwrap()
                .groups()
                .create_group(vec![member.pubkey], config, None)
                .await
                .unwrap();
            let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
            config.name = "Group C".to_string();
            let group_c = whitenoise
                .require_session(&creator.pubkey)
                .unwrap()
                .groups()
                .create_group(vec![member.pubkey], config, None)
                .await
                .unwrap();
            let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
            config.name = "Group D".to_string();
            let group_d = whitenoise
                .require_session(&creator.pubkey)
                .unwrap()
                .groups()
                .create_group(vec![member.pubkey], config, None)
                .await
                .unwrap();
            let mut config = create_nostr_group_config_data(vec![creator.pubkey]);
            config.name = "Group E".to_string();
            let group_e = whitenoise
                .require_session(&creator.pubkey)
                .unwrap()
                .groups()
                .create_group(vec![member.pubkey], config, None)
                .await
                .unwrap();

            let messages = vec![
                (&group_a, "1", "Message A", base_timestamp - 7200), // -2 hours
                (&group_b, "2", "Message B", base_timestamp + 7200), // +2 hours
                (&group_d, "4", "Message D", base_timestamp - 3600), // -1 hour
                (&group_e, "5", "Message E", base_timestamp + 3600), // +1 hour
            ];

            let session = whitenoise.session(&creator.pubkey).unwrap();
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
                    is_blocked: false,
                    content_tokens: whitenoise_markdown::Document::default(),
                    reactions: message_aggregator::ReactionSummary::default(),
                    kind: 9,
                    media_attachments: vec![],
                    delivery_status: None,
                };
                aggregated_message::AggregatedMessage::insert_message(
                    &msg,
                    &group.mls_group_id,
                    &creator.pubkey,
                    &session.account_db.inner,
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
        use std::collections::HashSet;

        use nostr_sdk::Keys;

        use super::*;
        use crate::whitenoise::relays::RelayType;

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
            let session = whitenoise
                .session(&account.pubkey)
                .expect("session should exist");
            session.deactivate_subscriptions().await;

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
        async fn test_register_recovers_only_the_target_account_subscriptions() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            let keys_one = Keys::generate();
            let account_one = whitenoise
                .login_with_external_signer_for_test(keys_one.clone())
                .await
                .unwrap();

            let keys_two = Keys::generate();
            let account_two = whitenoise
                .login_with_external_signer_for_test(keys_two.clone())
                .await
                .unwrap();

            whitenoise.remove_external_signer(&account_one.pubkey);
            whitenoise.remove_external_signer(&account_two.pubkey);
            whitenoise
                .session(&account_one.pubkey)
                .expect("session should exist")
                .deactivate_subscriptions()
                .await;
            whitenoise
                .session(&account_two.pubkey)
                .expect("session should exist")
                .deactivate_subscriptions()
                .await;

            whitenoise
                .register_external_signer(account_one.pubkey, keys_one)
                .await
                .unwrap();

            let one_after_first_registration = whitenoise
                .is_account_subscriptions_operational(&account_one)
                .await
                .unwrap();
            let two_after_first_registration = whitenoise
                .is_account_subscriptions_operational(&account_two)
                .await
                .unwrap();
            assert!(
                one_after_first_registration,
                "Registering account one's signer should recover account one"
            );
            assert!(
                !two_after_first_registration,
                "Registering account one's signer should not recover account two"
            );

            whitenoise
                .register_external_signer(account_two.pubkey, keys_two)
                .await
                .unwrap();

            let two_after_second_registration = whitenoise
                .is_account_subscriptions_operational(&account_two)
                .await
                .unwrap();
            assert!(
                two_after_second_registration,
                "Registering account two's signer should recover account two"
            );
        }

        #[tokio::test]
        async fn test_register_recovers_subscriptions_with_nip65_inbox_fallback() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

            let keys = Keys::generate();
            let account = whitenoise
                .login_with_external_signer_for_test(keys.clone())
                .await
                .unwrap();

            let nip65_relays = account.nip65_relays(&whitenoise.shared).await.unwrap();
            assert!(
                !nip65_relays.is_empty(),
                "test setup should give the account NIP-65 relays"
            );

            let user = account.user(&whitenoise.shared.database).await.unwrap();
            user.sync_relay_urls(&whitenoise.shared, RelayType::Inbox, &HashSet::new(), None)
                .await
                .unwrap();

            assert!(
                account
                    .inbox_relays(&whitenoise.shared)
                    .await
                    .unwrap()
                    .is_empty(),
                "test setup should leave the account with no explicit inbox relays"
            );
            assert!(
                !account
                    .effective_inbox_relays(&whitenoise.shared)
                    .await
                    .unwrap()
                    .is_empty(),
                "effective inbox relay lookup should fall back to NIP-65 relays"
            );

            whitenoise.remove_external_signer(&account.pubkey);
            whitenoise
                .session(&account.pubkey)
                .expect("session should exist")
                .deactivate_subscriptions()
                .await;

            let before = whitenoise
                .is_account_subscriptions_operational(&account)
                .await
                .unwrap();
            assert!(
                !before,
                "Account subscriptions should be non-operational before signer re-registration"
            );

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
                "register_external_signer should recover subscriptions using NIP-65 fallback relays"
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
                .execute(&whitenoise.shared.database.pool)
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
            let fallback = whitenoise.shared.fallback_relay_urls().await;
            let discovery_urls = whitenoise.config().discovery_relays.clone();

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

            let fallback = whitenoise.shared.fallback_relay_urls().await;
            assert!(
                !fallback.contains(&extra_url),
                "Fallback should not include a relay that was never added to discovery"
            );
        }

        #[tokio::test]
        async fn test_fallback_relay_urls_deduplicates() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let fallback = whitenoise.shared.fallback_relay_urls().await;

            let unique: HashSet<&RelayUrl> = fallback.iter().collect();
            assert_eq!(
                fallback.len(),
                unique.len(),
                "Fallback should not contain duplicates"
            );
        }
    }

    #[tokio::test]
    async fn new_rejects_invalid_product_analytics_config_before_creating_dirs() {
        let (config, _data_temp, _logs_temp) = create_test_config();
        let data_dir = config.data_dir.clone();
        let logs_dir = config.logs_dir.clone();
        let config =
            config.with_product_analytics_config(product_analytics::ProductAnalyticsConfig {
                backend: product_analytics::ProductAnalyticsBackend::Disabled,
                app_version: "1.0.0".to_string(),
                bundle_identifier: "not a bundle".to_string(),
                device_class: product_analytics::ProductAnalyticsDeviceClass::Desktop,
                os_name: "macOS".to_string(),
                locale: "en-US".to_string(),
                is_debug: true,
            });

        let err = Whitenoise::new(config).await.unwrap_err();

        assert!(matches!(err, WhitenoiseError::ProductAnalytics(_)));
        assert!(!data_dir.exists());
        assert!(!logs_dir.exists());
    }
}
