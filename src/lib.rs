use std::sync::{Mutex, OnceLock};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{filter::EnvFilter, fmt::Layer, prelude::*, registry::Registry};

mod nostr_manager;
mod types;
pub mod whitenoise;

// Integration tests module - included when integration-tests feature is enabled
// This provides IDE support.
#[cfg(feature = "integration-tests")]
pub mod integration_tests;

// Re-export main types for library users

// Core types
pub use types::{ImageType, MessageWithTokens};
pub use whitenoise::{Whitenoise, WhitenoiseConfig};

// Error handling
pub use whitenoise::error::WhitenoiseError;

// Account and user management
pub use whitenoise::accounts::{Account, AccountType};
pub use whitenoise::users::{User, UserSyncMode};

// Settings and configuration
pub use whitenoise::account_settings::AccountSettings;
pub use whitenoise::app_settings::{AppSettings, Language, ThemeMode};

// Groups and relays
pub use whitenoise::accounts_groups::AccountGroup;
pub use whitenoise::group_information::{GroupInformation, GroupType};
pub use whitenoise::relays::{Relay, RelayType};

// Media files
pub use whitenoise::database::media_files::{FileMetadata, MediaFile};

// Messaging
pub use whitenoise::message_aggregator::{
    ChatMessage, EmojiReaction, ReactionSummary, UserReaction,
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
