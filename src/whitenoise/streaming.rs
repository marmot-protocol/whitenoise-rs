use std::collections::{HashMap, HashSet};

use anyhow::anyhow;
use nostr_sdk::RelayUrl;
use tokio::sync::broadcast;

use super::accounts::Account;
use super::error::{Result, WhitenoiseError};
use super::relays::Relay;
use super::{
    Whitenoise, aggregated_message, chat_list, chat_list_streaming, media_files,
    message_aggregator, message_streaming, notification_streaming,
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
    pub async fn subscribe_to_group_messages(
        &self,
        group_id: &mdk_core::prelude::GroupId,
    ) -> Result<message_streaming::GroupMessageSubscription> {
        let mut updates = self.message_stream_manager.subscribe(group_id);

        let fetched_messages =
            aggregated_message::AggregatedMessage::find_messages_by_group(group_id, &self.database)
                .await
                .map_err(|e| {
                    WhitenoiseError::from(anyhow!("Failed to read cached messages: {}", e))
                })?;

        let mut messages_map: HashMap<String, message_aggregator::ChatMessage> = fetched_messages
            .into_iter()
            .map(|m| (m.id.clone(), m))
            .collect();

        loop {
            match updates.try_recv() {
                Ok(update) => {
                    messages_map.insert(update.message.id.clone(), update.message);
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(WhitenoiseError::Other(anyhow!(
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
                    return Err(WhitenoiseError::Other(anyhow!(
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
}
