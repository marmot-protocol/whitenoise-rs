use std::sync::{Mutex, OnceLock};

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::{filter::EnvFilter, fmt::Layer, prelude::*, registry::Registry};

#[cfg(feature = "benchmark-tests")]
use tracing_subscriber::filter::LevelFilter;

#[cfg(feature = "benchmark-tests")]
use crate::integration_tests::benchmarks::PerfTracingLayer;

mod nostr_manager;
pub mod perf;
pub(crate) mod relay_control;

// Re-export proc macro for ergonomic `#[perf_instrument("prefix")]` usage.
// Crate-private because the macro expands to `crate::perf_span!(...)` which
// only resolves inside this crate.
pub(crate) use whitenoise_macros::perf_instrument;
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

/// Create non-blocking file and stdout writers, stash their guards in [`TRACING_GUARDS`],
/// and return the two writers so callers can attach them to `fmt::Layer`s.
///
/// Extracted to avoid duplicating the appender setup across `init_tracing` and
/// `init_tracing_with_perf_layer`.
fn build_writers(logs_dir: &std::path::Path) -> (NonBlocking, NonBlocking) {
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

    (non_blocking_stdout, non_blocking_file)
}

/// Build the `EnvFilter` applied to both the stdout and file layers.
///
/// Perf and sqlx targets are suppressed in human-readable output so they are
/// routed exclusively to the `PerfTracingLayer` when it is active.  Both layers
/// share the same suppression rules, so a single helper avoids duplicating the
/// construction logic.
#[cfg(feature = "benchmark-tests")]
fn build_io_filter() -> EnvFilter {
    match EnvFilter::try_from_default_env() {
        Ok(env_filter) => env_filter
            .add_directive("whitenoise::perf=off".parse().unwrap())
            .add_directive("sqlx::query=off".parse().unwrap()),
        Err(_) => EnvFilter::new(
            "info,refinery_core=warn,refinery=warn,whitenoise::perf=off,sqlx::query=off",
        ),
    }
}

fn init_tracing(logs_dir: &std::path::Path) {
    TRACING_INIT.get_or_init(|| {
        let (non_blocking_stdout, non_blocking_file) = build_writers(logs_dir);

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

/// Initialise tracing and attach the given `PerfTracingLayer` to the subscriber
/// stack so that `perf_span!` markers are captured during benchmarks.
///
/// Only available when the `benchmark-tests` feature is enabled.
#[cfg(feature = "benchmark-tests")]
pub fn init_tracing_with_perf_layer(logs_dir: &std::path::Path, perf_layer: PerfTracingLayer) {
    TRACING_INIT.get_or_init(|| {
        let (non_blocking_stdout, non_blocking_file) = build_writers(logs_dir);

        let stdout_layer = Layer::new()
            .with_writer(non_blocking_stdout)
            .with_ansi(true)
            .with_target(true);

        let file_layer = Layer::new()
            .with_writer(non_blocking_file)
            .with_ansi(false)
            .with_target(true);

        // Both stdout and file layers suppress perf/sqlx targets so those
        // events are routed exclusively to the dedicated PerfTracingLayer.
        let stdout_filter = build_io_filter();
        let file_filter = build_io_filter();

        // The perf layer receives only the targets it cares about.
        let perf_filter = tracing_subscriber::filter::Targets::new()
            .with_target("whitenoise::perf", LevelFilter::INFO)
            .with_target("sqlx::query", LevelFilter::INFO);

        Registry::default()
            .with(stdout_layer.with_filter(stdout_filter))
            .with(file_layer.with_filter(file_filter))
            .with(perf_layer.with_filter(perf_filter))
            .init();
    });
}
