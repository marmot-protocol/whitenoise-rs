use std::sync::{Mutex, OnceLock};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{filter::EnvFilter, fmt::Layer, prelude::*, registry::Registry};

#[cfg(feature = "benchmark-tests")]
use crate::integration_tests::benchmarks::PerfTracingLayer;

mod nostr_manager;
pub mod perf;
pub(crate) mod relay_control;
mod types;
pub mod whitenoise;

#[cfg(feature = "cli")]
pub mod cli;

// Integration tests module - included when integration-tests feature is enabled
// This provides IDE support.
#[cfg(feature = "integration-tests")]
pub mod integration_tests;

#[cfg(feature = "integration-tests")]
pub mod test_fixtures;

// Re-export main types for library users

// Core types
pub use types::{
    AccountInboxPlaneStateSnapshot, AccountInboxPlanesStateSnapshot, DiscoveryPlaneStateSnapshot,
    GroupPlaneGroupStateSnapshot, GroupPlaneStateSnapshot, ImageType, MessageWithTokens,
    RelayControlStateSnapshot, RelaySessionRelayStateSnapshot, RelaySessionStateSnapshot,
};
pub use whitenoise::{Whitenoise, WhitenoiseConfig};

// Error handling
pub use whitenoise::error::WhitenoiseError;

// Account and user management
pub use whitenoise::accounts::{Account, AccountType, LoginError, LoginResult, LoginStatus};
pub use whitenoise::users::{KeyPackageStatus, User, UserSyncMode};

// Settings and configuration
pub use whitenoise::account_settings::AccountSettings;
pub use whitenoise::app_settings::{AppSettings, Language, ThemeMode};

// Groups and relays
pub use whitenoise::accounts_groups::AccountGroup;
pub use whitenoise::group_information::{GroupInformation, GroupType};
pub use whitenoise::groups::{GroupWithInfoAndMembership, GroupWithMembership};
pub use whitenoise::relays::{Relay, RelayType};

// Drafts
pub use whitenoise::drafts::Draft;

// Media files
pub use whitenoise::database::media_files::{FileMetadata, MediaFile};

// Messaging
pub use whitenoise::message_aggregator::{
    ChatMessage, DeliveryStatus, EmojiReaction, ReactionSummary, UserReaction,
};

// Nostr integration
pub use nostr_manager::parser::SerializableToken;

// Group message streaming
pub use whitenoise::message_streaming::{GroupMessageSubscription, MessageUpdate, UpdateTrigger};

// Chat list streaming
pub use whitenoise::chat_list_streaming::{
    ChatListSubscription, ChatListUpdate, ChatListUpdateTrigger,
};

// Notification streaming
pub use whitenoise::notification_streaming::{
    NotificationSubscription, NotificationTrigger, NotificationUpdate, NotificationUser,
};

// User search
pub use whitenoise::user_search::{
    MatchQuality, MatchResult, MatchedField, SearchUpdateTrigger, UserSearchResult,
    UserSearchSubscription, UserSearchUpdate,
};

static TRACING_GUARDS: OnceLock<Mutex<Option<(WorkerGuard, WorkerGuard)>>> = OnceLock::new();
static TRACING_INIT: OnceLock<()> = OnceLock::new();

fn init_tracing(logs_dir: &std::path::Path) {
    init_tracing_base(logs_dir);
}

/// Initialise tracing and attach the given `PerfTracingLayer` to the subscriber
/// stack so that `perf_span!` markers are captured during benchmarks.
///
/// Only available when the `benchmark-tests` feature is enabled.
#[cfg(feature = "benchmark-tests")]
pub fn init_tracing_with_perf_layer(logs_dir: &std::path::Path, perf_layer: PerfTracingLayer) {
    use tracing_subscriber::filter::LevelFilter;

    TRACING_INIT.get_or_init(|| {
        let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("whitenoise")
            .filename_suffix("log")
            .build(logs_dir)
            .expect("Failed to create file appender");

        let (non_blocking_file, file_guard) = tracing_appender::non_blocking(file_appender);
        let (non_blocking_stdout, stdout_guard) = tracing_appender::non_blocking(std::io::stdout());

        TRACING_GUARDS
            .set(Mutex::new(Some((file_guard, stdout_guard))))
            .ok();

        let stdout_layer = Layer::new()
            .with_writer(non_blocking_stdout)
            .with_ansi(true)
            .with_target(true);

        let file_layer = Layer::new()
            .with_writer(non_blocking_file)
            .with_ansi(false)
            .with_target(true);

        // Per-layer filter for stdout/file: pass everything the env_filter
        // would pass, but suppress perf-only targets that should only reach
        // the dedicated PerfTracingLayer.
        let output_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("info,refinery_core=warn,refinery=warn,whitenoise::perf=off,sqlx::query=off")
        });

        let file_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("info,refinery_core=warn,refinery=warn,whitenoise::perf=off,sqlx::query=off")
        });

        // Add a dedicated INFO-level filter so the perf layer receives events
        // from both our manual perf_span! markers and sqlx's query logger.
        let perf_filter = tracing_subscriber::filter::Targets::new()
            .with_target("whitenoise::perf", LevelFilter::INFO)
            .with_target("sqlx::query", LevelFilter::INFO);

        Registry::default()
            .with(stdout_layer.with_filter(output_filter))
            .with(file_layer.with_filter(file_filter))
            .with(perf_layer.with_filter(perf_filter))
            .init();
    });
}

fn init_tracing_base(logs_dir: &std::path::Path) {
    TRACING_INIT.get_or_init(|| {
        let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("whitenoise")
            .filename_suffix("log")
            .build(logs_dir)
            .expect("Failed to create file appender");

        let (non_blocking_file, file_guard) = tracing_appender::non_blocking(file_appender);
        let (non_blocking_stdout, stdout_guard) = tracing_appender::non_blocking(std::io::stdout());

        TRACING_GUARDS
            .set(Mutex::new(Some((file_guard, stdout_guard))))
            .ok();

        let stdout_layer = Layer::new()
            .with_writer(non_blocking_stdout)
            .with_ansi(true)
            .with_target(true);

        let file_layer = Layer::new()
            .with_writer(non_blocking_file)
            .with_ansi(false)
            .with_target(true);

        Registry::default()
            .with(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("info,refinery_core=warn,refinery=warn")),
            )
            .with(stdout_layer)
            .with(file_layer)
            .init();
    });
}
