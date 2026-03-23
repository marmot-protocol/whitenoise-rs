use std::collections::HashMap;

use nostr_sdk::PublicKey;
use tokio::sync::broadcast;

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::users::User;
use crate::whitenoise::{
    aggregated_message, chat_list, chat_list_streaming, media_files, message_aggregator,
    message_streaming, notification_streaming, user_streaming,
};

impl Whitenoise {
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
    #[perf_instrument("whitenoise")]
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
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!("subscription drain lagged by {n} messages");
                    continue;
                }
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

    /// Subscribe to updates for a single user.
    ///
    /// Returns both an initial snapshot AND a receiver for real-time updates.
    /// The design eliminates race conditions:
    /// - Subscription is established BEFORE fetching to capture concurrent updates
    /// - Any updates that arrived during fetch are merged into `initial_user`
    /// - The receiver only yields updates AFTER the initial snapshot
    #[perf_instrument("whitenoise")]
    pub async fn subscribe_to_user(
        &self,
        pubkey: &PublicKey,
    ) -> Result<user_streaming::UserSubscription> {
        let mut updates = self.user_stream_manager.subscribe(pubkey);
        let initial_user = self.resolve_user(pubkey).await?;
        let initial_user = Self::drain_user_updates(initial_user, &mut updates)?;

        Ok(user_streaming::UserSubscription {
            initial_user,
            updates,
        })
    }

    pub(crate) fn drain_user_updates(
        mut initial_user: User,
        updates: &mut broadcast::Receiver<user_streaming::UserUpdate>,
    ) -> Result<User> {
        loop {
            match updates.try_recv() {
                Ok(update) => {
                    initial_user = update.user;
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!(
                        target: "whitenoise",
                        "subscription drain lagged by {n} user updates"
                    );
                    continue;
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "User stream closed unexpectedly during subscription"
                    )));
                }
            }
        }

        Ok(initial_user)
    }

    /// Subscribe to chat list updates for a specific account.
    ///
    /// Returns both an initial snapshot AND a receiver for real-time updates.
    /// The design eliminates race conditions:
    /// - Subscription is established BEFORE fetching to capture concurrent updates
    /// - Any updates that arrived during fetch are merged into `initial_items`
    /// - The receiver only yields updates AFTER the initial snapshot
    #[perf_instrument("whitenoise")]
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

        // Drain updates that arrived during fetch. A ChatArchiveChanged update could
        // land here with archived_at set — filter it out so only active items remain.
        loop {
            match updates.try_recv() {
                Ok(update) => {
                    if update.item.archived_at.is_none() {
                        items_map.insert(update.item.mls_group_id.clone(), update.item);
                    } else {
                        items_map.remove(&update.item.mls_group_id);
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!("subscription drain lagged by {n} messages");
                    continue;
                }
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

    /// Subscribe to archived chat list updates for a specific account.
    ///
    /// Same race-condition-free design as `subscribe_to_chat_list`, but uses
    /// `get_archived_chat_list` and the archived stream manager.
    #[perf_instrument("whitenoise")]
    pub async fn subscribe_to_archived_chat_list(
        &self,
        account: &Account,
    ) -> Result<chat_list_streaming::ChatListSubscription> {
        let mut updates = self
            .archived_chat_list_stream_manager
            .subscribe(&account.pubkey);

        let fetched_items = self.get_archived_chat_list(account).await?;

        let mut items_map: HashMap<mdk_core::prelude::GroupId, chat_list::ChatListItem> =
            fetched_items
                .into_iter()
                .map(|item| (item.mls_group_id.clone(), item))
                .collect();

        // Drain updates that arrived during fetch. A ChatArchiveChanged update could
        // land here with archived_at cleared — filter it out so only archived items remain.
        loop {
            match updates.try_recv() {
                Ok(update) => {
                    if update.item.archived_at.is_some() {
                        items_map.insert(update.item.mls_group_id.clone(), update.item);
                    } else {
                        items_map.remove(&update.item.mls_group_id);
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!("subscription drain lagged by {n} messages");
                    continue;
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "Archived chat list stream closed unexpectedly during subscription"
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

}
