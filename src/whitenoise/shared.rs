use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use nostr_sdk::prelude::NostrSigner;
use nostr_sdk::{PublicKey, RelayUrl};
use tokio::sync::Mutex;

#[cfg(test)]
use nostr_sdk::EventId;
#[cfg(test)]
use tokio::sync::Semaphore;

use crate::mdk::GroupId;
use crate::perf_instrument;
use crate::relay_control::RelayControlPlane;
use crate::whitenoise::WhitenoiseConfig;
use crate::whitenoise::chat_list_streaming::ChatListStreamManager;
use crate::whitenoise::database::Database;
use crate::whitenoise::discovery_sync_worker::DiscoverySyncWorker;
use crate::whitenoise::event_tracker::EventTracker;
use crate::whitenoise::group_state_streaming::GroupStateStreamManager;
use crate::whitenoise::message_aggregator::MessageAggregator;
use crate::whitenoise::message_streaming::MessageStreamManager;
use crate::whitenoise::notification_streaming::NotificationStreamManager;
use crate::whitenoise::secrets_store::SecretsStore;
use crate::whitenoise::storage::Storage;
use crate::whitenoise::user_streaming::UserStreamManager;

/// Long-lived services and registries shared across every account session.
///
/// Held behind `Arc<SharedServices>` on `Whitenoise` and on every
/// `AccountSession`. Lifecycle plumbing (event and shutdown channels, scheduler
/// handles, account registry) stays on `Whitenoise` itself.
pub struct SharedServices {
    pub(crate) config: Arc<WhitenoiseConfig>,
    pub database: Arc<Database>,
    pub(crate) relay_control: Arc<RelayControlPlane>,
    pub(crate) event_tracker: Arc<dyn EventTracker>,
    pub(crate) secrets_store: SecretsStore,
    pub(crate) storage: Storage,
    pub(crate) message_aggregator: MessageAggregator,
    pub(crate) message_stream_manager: MessageStreamManager,
    pub(crate) user_stream_manager: UserStreamManager,
    pub(crate) chat_list_stream_manager: ChatListStreamManager,
    pub(crate) archived_chat_list_stream_manager: ChatListStreamManager,
    pub(crate) group_state_stream_manager: GroupStateStreamManager,
    pub(crate) notification_stream_manager: NotificationStreamManager,
    pub(crate) user_resolution_guards: DashMap<PublicKey, Arc<Mutex<()>>>,
    pub(crate) external_signers: DashMap<PublicKey, Arc<dyn NostrSigner>>,
    pub(crate) discovery_sync_worker: DiscoverySyncWorker,
    /// Per-sender rate limiter for inbound MIP-05 token requests and removals.
    /// Maps `(account_pubkey, GroupId, sender_leaf_index, kind)` → last accepted
    /// `Instant`.  Requests and removals have independent cooldown buckets.
    pub(crate) token_request_timestamps: DashMap<
        (
            PublicKey,
            GroupId,
            u32,
            super::push_notifications::TokenRateKind,
        ),
        Instant,
    >,
    /// In-memory coordination for delayed MIP-05 token-list responses.
    /// Only used by deprecated `Whitenoise`-level test wrappers; the session
    /// layer has its own `AccountSession::pending_push_token_responses`.
    #[cfg(test)]
    pub(crate) pending_push_token_responses: Arc<DashMap<(PublicKey, GroupId, EventId), ()>>,
    /// Bounds the number of concurrently-active delayed MIP-05 token-list response tasks.
    /// Only used by deprecated `Whitenoise`-level test wrappers.
    #[cfg(test)]
    pub(crate) token_response_semaphore: Arc<Semaphore>,
}

impl SharedServices {
    /// Build a `SharedServices` from its externally-configured fields. Stream
    /// managers, registries, and the discovery worker use their defaults —
    /// callers never need to supply them.
    pub(crate) fn new(
        config: Arc<WhitenoiseConfig>,
        database: Arc<Database>,
        relay_control: Arc<RelayControlPlane>,
        event_tracker: Arc<dyn EventTracker>,
        secrets_store: SecretsStore,
        storage: Storage,
        message_aggregator: MessageAggregator,
    ) -> Self {
        Self {
            config,
            database,
            relay_control,
            event_tracker,
            secrets_store,
            storage,
            message_aggregator,
            message_stream_manager: MessageStreamManager::default(),
            user_stream_manager: UserStreamManager::default(),
            chat_list_stream_manager: ChatListStreamManager::default(),
            archived_chat_list_stream_manager: ChatListStreamManager::default(),
            group_state_stream_manager: GroupStateStreamManager::default(),
            notification_stream_manager: NotificationStreamManager::default(),
            user_resolution_guards: DashMap::new(),
            external_signers: DashMap::new(),
            discovery_sync_worker: DiscoverySyncWorker::new(),
            token_request_timestamps: DashMap::new(),
            #[cfg(test)]
            pending_push_token_responses: Arc::new(DashMap::new()),
            #[cfg(test)]
            token_response_semaphore: Arc::new(Semaphore::new(
                super::push_notifications::MAX_CONCURRENT_TOKEN_RESPONSE_TASKS,
            )),
        }
    }

    /// Returns the union of default relays and currently connected relays.
    ///
    /// Used as the fallback relay set when a user has no stored NIP-65 relays.
    /// Discovery fallback is owned by the discovery plane rather than whatever
    /// other relays happen to be connected for unrelated workloads.
    #[perf_instrument("whitenoise")]
    pub(crate) async fn fallback_relay_urls(&self) -> Vec<RelayUrl> {
        self.relay_control.discovery().relays().to_vec()
    }
}
