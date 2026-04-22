use std::sync::Arc;

use dashmap::DashMap;
use nostr_sdk::PublicKey;
use nostr_sdk::prelude::NostrSigner;
use tokio::sync::Mutex;

use crate::relay_control::RelayControlPlane;
use crate::whitenoise::chat_list_streaming::ChatListStreamManager;
use crate::whitenoise::database::Database;
use crate::whitenoise::discovery_sync_worker::DiscoverySyncWorker;
use crate::whitenoise::event_tracker::EventTracker;
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
pub(crate) struct SharedServices {
    pub(crate) database: Arc<Database>,
    pub(crate) relay_control: Arc<RelayControlPlane>,
    pub(crate) event_tracker: Arc<dyn EventTracker>,
    pub(crate) secrets_store: SecretsStore,
    pub(crate) storage: Storage,
    pub(crate) message_aggregator: MessageAggregator,
    pub(crate) message_stream_manager: Arc<MessageStreamManager>,
    pub(crate) user_stream_manager: UserStreamManager,
    pub(crate) chat_list_stream_manager: ChatListStreamManager,
    pub(crate) archived_chat_list_stream_manager: ChatListStreamManager,
    pub(crate) notification_stream_manager: NotificationStreamManager,
    pub(crate) user_resolution_guards: DashMap<PublicKey, Arc<Mutex<()>>>,
    pub(crate) external_signers: DashMap<PublicKey, Arc<dyn NostrSigner>>,
    pub(crate) discovery_sync_worker: DiscoverySyncWorker,
}
