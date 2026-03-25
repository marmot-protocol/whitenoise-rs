use std::collections::HashMap;

use nostr_sdk::PublicKey;
use tokio::sync::broadcast;

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::error::{Result, WhitenoiseError};
#[cfg(test)]
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
        group_id: &mdk_core::prelude::GroupId,
        limit: Option<u32>,
    ) -> Result<message_streaming::GroupMessageSubscription> {
        // 1. Subscribe FIRST to capture any concurrent updates that arrive during the fetch.
        let mut updates = self.message_stream_manager.subscribe(group_id);

        // 2. Fetch the most-recent `limit` messages using the paginated query so the initial
        //    snapshot honours the same page size the Flutter side will use for further loads.
        let fetched_messages =
            aggregated_message::AggregatedMessage::find_messages_by_group_paginated(
                group_id,
                &self.database,
                None, // no before-cursor → newest page
                None,
                limit,
            )
            .await
            .map_err(|e| {
                WhitenoiseError::from(anyhow::anyhow!("Failed to read cached messages: {}", e))
            })?;

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
                    if update.trigger == message_streaming::UpdateTrigger::NewMessage
                        || messages_map.contains_key(&update.message.id)
                    {
                        messages_map.insert(update.message.id.clone(), update.message);
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!(
                        target: "whitenoise",
                        "subscription drain lagged by {n} messages; rebuilding snapshot from storage"
                    );
                    // Some updates were dropped — rebuild the snapshot from the DB
                    // so the "initial snapshot + subsequent updates" guarantee holds.
                    // Stop draining after refetch so stale buffered deltas cannot
                    // overwrite fresher rows that are already in the rebuilt snapshot.
                    let refetched = aggregated_message::AggregatedMessage::find_messages_by_group(
                        group_id,
                        &self.database,
                    )
                    .await
                    .map_err(|e| {
                        WhitenoiseError::from(anyhow::anyhow!(
                            "Failed to read cached messages: {}",
                            e
                        ))
                    })?;
                    messages_map = refetched.into_iter().map(|m| (m.id.clone(), m)).collect();
                    break;
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    // Channel closed unexpectedly — unreachable while we hold a receiver.
                    return Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "Message stream closed unexpectedly during subscription"
                    )));
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
        let mut initial_user = self.resolve_user(pubkey).await?;

        loop {
            match updates.try_recv() {
                Ok(update) => {
                    initial_user = update.user;
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!(
                        target: "whitenoise",
                        "subscription drain lagged by {n} user updates; rebuilding snapshot from storage"
                    );
                    // Some updates were dropped — re-fetch from storage so the
                    // "initial snapshot + subsequent updates" guarantee holds.
                    initial_user = self.resolve_user(pubkey).await?;
                    continue;
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "User stream closed unexpectedly during subscription"
                    )));
                }
            }
        }

        Ok(user_streaming::UserSubscription {
            initial_user,
            updates,
        })
    }

    #[cfg(test)]
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
        self.subscribe_to_chat_list_internal(account, false).await
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
        self.subscribe_to_chat_list_internal(account, true).await
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

    async fn subscribe_to_chat_list_internal(
        &self,
        account: &Account,
        archived: bool,
    ) -> Result<chat_list_streaming::ChatListSubscription> {
        let mut updates = if archived {
            self.archived_chat_list_stream_manager
                .subscribe(&account.pubkey)
        } else {
            self.chat_list_stream_manager.subscribe(&account.pubkey)
        };

        let fetched_items = if archived {
            self.get_archived_chat_list(account).await?
        } else {
            self.get_chat_list(account).await?
        };

        let mut items_map: HashMap<mdk_core::prelude::GroupId, chat_list::ChatListItem> =
            fetched_items
                .into_iter()
                .map(|item| (item.mls_group_id.clone(), item))
                .collect();

        self.drain_chat_list_updates(account, &mut updates, &mut items_map, archived)
            .await?;

        let mut initial_items: Vec<chat_list::ChatListItem> = items_map.into_values().collect();
        chat_list::sort_chat_list(&mut initial_items);

        Ok(chat_list_streaming::ChatListSubscription {
            initial_items,
            updates,
        })
    }

    async fn drain_chat_list_updates(
        &self,
        account: &Account,
        updates: &mut broadcast::Receiver<chat_list_streaming::ChatListUpdate>,
        items_map: &mut HashMap<mdk_core::prelude::GroupId, chat_list::ChatListItem>,
        archived: bool,
    ) -> Result<()> {
        loop {
            match updates.try_recv() {
                Ok(update) => {
                    if update.item.archived_at.is_some() == archived {
                        items_map.insert(update.item.mls_group_id.clone(), update.item);
                    } else {
                        items_map.remove(&update.item.mls_group_id);
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    tracing::warn!(
                        target: "whitenoise",
                        "subscription drain lagged by {n} messages; rebuilding snapshot from storage"
                    );
                    let refetched = if archived {
                        self.get_archived_chat_list(account).await?
                    } else {
                        self.get_chat_list(account).await?
                    };
                    *items_map = refetched
                        .into_iter()
                        .map(|item| (item.mls_group_id.clone(), item))
                        .collect();
                    break;
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    let stream_name = if archived {
                        "Archived chat list"
                    } else {
                        "Chat list"
                    };
                    return Err(WhitenoiseError::Other(anyhow::anyhow!(
                        "{stream_name} stream closed unexpectedly during subscription"
                    )));
                }
            }
        }

        Ok(())
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
    use mdk_core::prelude::GroupId;
    use nostr_sdk::{Keys, Metadata};
    use tokio::sync::broadcast;

    use super::*;
    use crate::whitenoise::accounts::{Account, AccountType};
    use crate::whitenoise::chat_list::ChatListItem;
    use crate::whitenoise::chat_list_streaming::{ChatListUpdate, ChatListUpdateTrigger};
    use crate::whitenoise::group_information::GroupType;
    use crate::whitenoise::test_utils::create_mock_whitenoise;
    use crate::whitenoise::user_streaming;
    use crate::whitenoise::users::User;

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

    #[test]
    fn test_drain_user_updates_returns_error_when_channel_closed() {
        let initial_user = test_user("Initial");
        let (_sender, mut updates) = broadcast::channel(1);
        drop(_sender);

        let result = Whitenoise::drain_user_updates(initial_user, &mut updates);
        assert!(result.is_err());
    }

    #[test]
    fn test_drain_user_updates_handles_lagged_and_keeps_latest_message() {
        let initial_user = test_user("Initial");
        let latest_user = test_user("Latest");
        let (sender, mut updates) = broadcast::channel(1);
        let stale_user = test_user("Stale");

        sender
            .send(user_streaming::UserUpdate {
                trigger: user_streaming::UserUpdateTrigger::MetadataChanged,
                user: stale_user,
            })
            .expect("should queue stale update");
        sender
            .send(user_streaming::UserUpdate {
                trigger: user_streaming::UserUpdateTrigger::MetadataChanged,
                user: latest_user.clone(),
            })
            .expect("should queue latest update");

        let drained_user = Whitenoise::drain_user_updates(initial_user, &mut updates)
            .expect("lagged receiver should still return latest user");

        assert_eq!(
            drained_user.metadata.name.as_deref(),
            latest_user.metadata.name.as_deref()
        );
    }

    #[tokio::test]
    async fn test_drain_chat_list_updates_inserts_item_when_archive_status_matches() {
        let (whitenoise, _data_dir, _logs_dir) = create_mock_whitenoise().await;
        let account = test_account();
        let (sender, mut updates) = broadcast::channel(8);
        let mut items_map = HashMap::new();
        let item = test_chat_list_item(None);

        sender
            .send(ChatListUpdate {
                trigger: ChatListUpdateTrigger::NewGroup,
                item: item.clone(),
            })
            .expect("should queue chat list update");

        whitenoise
            .drain_chat_list_updates(&account, &mut updates, &mut items_map, false)
            .await
            .expect("drain should succeed");

        assert!(items_map.contains_key(&item.mls_group_id));
    }

    #[tokio::test]
    async fn test_drain_chat_list_updates_removes_item_when_archive_status_differs() {
        let (whitenoise, _data_dir, _logs_dir) = create_mock_whitenoise().await;
        let account = test_account();
        let (sender, mut updates) = broadcast::channel(8);
        let item = test_chat_list_item(Some(Utc::now()));
        let mut items_map = HashMap::from([(item.mls_group_id.clone(), item.clone())]);

        sender
            .send(ChatListUpdate {
                trigger: ChatListUpdateTrigger::ChatArchiveChanged,
                item: item.clone(),
            })
            .expect("should queue archive update");

        whitenoise
            .drain_chat_list_updates(&account, &mut updates, &mut items_map, false)
            .await
            .expect("drain should succeed");

        assert!(!items_map.contains_key(&item.mls_group_id));
    }

    #[tokio::test]
    async fn test_drain_chat_list_updates_returns_error_when_archived_stream_closed() {
        let (whitenoise, _data_dir, _logs_dir) = create_mock_whitenoise().await;
        let account = test_account();
        let (_sender, mut updates) = broadcast::channel(1);
        drop(_sender);
        let mut items_map = HashMap::new();

        let result = whitenoise
            .drain_chat_list_updates(&account, &mut updates, &mut items_map, true)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_drain_chat_list_updates_rebuilds_items_map_when_receiver_lagged() {
        let (whitenoise, _data_dir, _logs_dir) = create_mock_whitenoise().await;
        let account = whitenoise
            .create_identity()
            .await
            .expect("account should be created");
        let (sender, mut updates) = broadcast::channel(1);

        let stale_item = test_chat_list_item(None);
        let mut items_map = HashMap::from([(stale_item.mls_group_id.clone(), stale_item)]);

        sender
            .send(ChatListUpdate {
                trigger: ChatListUpdateTrigger::NewGroup,
                item: test_chat_list_item(None),
            })
            .expect("should queue first update");
        sender
            .send(ChatListUpdate {
                trigger: ChatListUpdateTrigger::NewGroup,
                item: test_chat_list_item(None),
            })
            .expect("should queue second update and force lag");

        whitenoise
            .drain_chat_list_updates(&account, &mut updates, &mut items_map, false)
            .await
            .expect("drain should succeed by rebuilding from storage");

        assert!(
            items_map.is_empty(),
            "lagged path should replace stale map with freshly fetched chat list"
        );
    }

    fn test_user(name: &str) -> User {
        let mut user = User {
            id: None,
            pubkey: Keys::generate().public_key(),
            metadata: Metadata::new().name(name),
            created_at: Utc::now(),
            metadata_known_at: None,
            updated_at: Utc::now(),
        };
        user.mark_metadata_known_now();
        user
    }

    fn test_account() -> Account {
        Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn test_chat_list_item(archived_at: Option<chrono::DateTime<Utc>>) -> ChatListItem {
        ChatListItem {
            mls_group_id: GroupId::from_slice(&[7; 32]),
            name: Some("test".to_string()),
            group_type: GroupType::Group,
            created_at: Utc::now(),
            group_image_path: None,
            group_image_url: None,
            last_message: None,
            self_removed: true,
            pending_confirmation: false,
            welcomer_pubkey: None,
            unread_count: 0,
            pin_order: None,
            dm_peer_pubkey: None,
            archived_at,
            removed_at: None,
            muted_until: None,
        }
    }
}
