use std::collections::HashMap;

use nostr_sdk::PublicKey;
use tokio::sync::broadcast;

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::database::aggregated_messages::PaginationOptions;
use crate::whitenoise::error::Result;
use crate::whitenoise::streaming_error::{StreamKind, StreamingError};
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
    /// * `limit`    - Maximum number of messages to include in the initial snapshot
    ///   (default 50, capped at 200).  Older messages can be fetched on demand via
    ///   [`Self::fetch_aggregated_messages_for_group`].  Pass `None` to use the default.
    ///
    /// # Returns
    /// A [`message_streaming::GroupMessageSubscription`] containing initial messages and a broadcast receiver
    #[perf_instrument("whitenoise")]
    pub async fn subscribe_to_group_messages(
        &self,
        account_pubkey: &nostr_sdk::PublicKey,
        group_id: &mdk_core::prelude::GroupId,
        limit: Option<u32>,
    ) -> Result<message_streaming::GroupMessageSubscription> {
        // 1. Subscribe FIRST to capture any concurrent updates that arrive during the fetch.
        let mut updates = self.message_stream_manager.subscribe(group_id);

        // 2. Fetch the most-recent `limit` messages using the paginated query so the initial
        //    snapshot honours the same page size the Flutter side will use for further loads.
        let cleared_at_ms =
            AccountGroup::chat_cleared_at_ms(account_pubkey, group_id, &self.database).await?;

        let fetched_messages =
            aggregated_message::AggregatedMessage::find_messages_by_group_paginated(
                account_pubkey,
                group_id,
                &self.database,
                &PaginationOptions::default(),
                limit,
                cleared_at_ms,
            )
            .await?;

        // 3. Build a map keyed by message ID for deduplication.
        let mut messages_map: HashMap<String, message_aggregator::ChatMessage> = fetched_messages
            .into_iter()
            .map(|m| (m.id.clone(), m))
            .collect();

        // 4. Drain any updates that landed between subscribe() and the DB fetch.
        //    These are messages that were persisted and broadcast while the query was in
        //    flight. Without this drain they would be missing from the snapshot even though
        //    they are already in the database. For NewMessage triggers, always insert: either
        //    the message is already in the map (update replaces stale DB row) or it arrived
        //    just after the query cut-off (new message that belongs in the snapshot). For
        //    other triggers, only apply the update if the message id already exists in the map
        //    to avoid pulling in old messages. The live `updates` receiver returned to the
        //    caller covers all future messages; this loop only closes the narrow race window
        //    on the initial snapshot.
        loop {
            match updates.try_recv() {
                Ok(update) => {
                    // Skip messages at or before the clear timestamp
                    if let Some(cleared) = cleared_at_ms {
                        let msg_ms = update.message.created_at.as_secs() as i64 * 1000;
                        if msg_ms <= cleared {
                            continue;
                        }
                    }
                    if update.trigger == message_streaming::UpdateTrigger::NewMessage
                        || messages_map.contains_key(&update.message.id)
                    {
                        messages_map.insert(update.message.id.clone(), update.message);
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::error!(
                        target: "whitenoise::mod",
                        "subscription drain lagged by {n} messages, snapshot may be incomplete"
                    );
                    return Err(StreamingError::Lagged {
                        stream: StreamKind::Message,
                        missed: n,
                    }
                    .into());
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    // Channel closed unexpectedly — unreachable while we hold a receiver.
                    return Err(StreamingError::Closed {
                        stream: StreamKind::Message,
                    }
                    .into());
                }
            }
        }

        // 5. Collect and sort chronologically (oldest first).  The map iteration order is
        //    arbitrary, so an explicit sort is always needed.
        let mut initial_messages: Vec<message_aggregator::ChatMessage> =
            messages_map.into_values().collect();
        initial_messages.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });

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

    fn drain_user_updates(
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
                    return Err(StreamingError::Closed {
                        stream: StreamKind::User,
                    }
                    .into());
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
                    return Err(StreamingError::Closed {
                        stream: StreamKind::ChatList,
                    }
                    .into());
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
                    return Err(StreamingError::Closed {
                        stream: StreamKind::ArchivedChatList,
                    }
                    .into());
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use nostr_sdk::{Keys, Metadata};
    use tokio::sync::broadcast;

    use super::*;
    use crate::whitenoise::user_streaming;

    #[test]
    fn test_drain_user_updates_uses_newest_queued_update() {
        let pubkey = Keys::generate().public_key();
        let mut initial_user = User {
            id: None,
            pubkey,
            metadata: Metadata::new().name("Old Name"),
            created_at: Utc::now(),
            metadata_known_at: None,
            updated_at: Utc::now(),
        };
        initial_user.mark_metadata_known_now();

        let mut newest_user = initial_user.clone();
        newest_user.metadata = Metadata::new().name("Newest Name");
        newest_user.updated_at = Utc::now();

        let (sender, mut updates) = broadcast::channel(10);
        sender
            .send(user_streaming::UserUpdate {
                trigger: user_streaming::UserUpdateTrigger::MetadataChanged,
                user: newest_user.clone(),
            })
            .expect("should queue update");

        let drained_user = Whitenoise::drain_user_updates(initial_user, &mut updates).unwrap();

        assert_eq!(drained_user, newest_user);
    }
}
