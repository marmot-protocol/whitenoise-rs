use std::sync::Arc;
use std::time::Duration;

use crate::{
    nostr_manager::NostrManager,
    types::MessageWithTokens,
    whitenoise::{
        Whitenoise,
        accounts::Account,
        aggregated_message::AggregatedMessage,
        database::Database,
        error::{Result, WhitenoiseError},
        media_files::MediaFile,
        message_aggregator::{ChatMessage, DeliveryStatus},
        message_streaming::{MessageStreamManager, MessageUpdate, UpdateTrigger},
    },
};
use mdk_core::prelude::{message_types::Message, *};
use nostr_sdk::prelude::*;

impl Whitenoise {
    /// Sends a message to a specific group and returns the message with parsed tokens.
    ///
    /// This method creates and sends a message to a group using the Nostr MLS protocol.
    /// It handles the complete message lifecycle including event creation, MLS message
    /// generation, publishing to relays, and token parsing. The message content is
    /// automatically parsed for tokens (e.g., mentions, hashtags) before returning.
    ///
    /// # Arguments
    ///
    /// * `sender_pubkey` - The public key of the user sending the message. This is used
    ///   to identify the sender and fetch their account for message creation.
    /// * `group_id` - The unique identifier of the target group where the message will be sent.
    /// * `message` - The content of the message to be sent as a string.
    /// * `kind` - The Nostr event kind as a u16. This determines the type of event being created
    ///   (e.g., text note, reaction, etc.).
    /// * `tags` - Optional vector of Nostr tags to include with the message. If None, an empty
    ///   tag list will be used.
    pub async fn send_message_to_group(
        &self,
        account: &Account,
        group_id: &GroupId,
        message: String,
        kind: u16,
        tags: Option<Vec<Tag>>,
    ) -> Result<MessageWithTokens> {
        let (inner_event, event_id) =
            self.create_unsigned_nostr_event(&account.pubkey, &message, kind, tags)?;

        let mdk = self.create_mdk_for_account(account.pubkey)?;

        // Guard: fail immediately if no relays configured (before creating MLS message
        // to avoid persisting a message in MDK storage that can never be delivered)
        let group_relays: Vec<RelayUrl> = mdk.get_relays(group_id)?.into_iter().collect();
        if group_relays.is_empty() {
            return Err(WhitenoiseError::GroupMissingRelays);
        }

        let message_event = mdk.create_message(group_id, inner_event)?;
        let mdk_message =
            mdk.get_message(group_id, &event_id)?
                .ok_or(WhitenoiseError::MdkCoreError(
                    mdk_core::error::Error::MessageNotFound,
                ))?;

        let tokens = self.nostr.parse(&mdk_message.content);

        // Proactive caching + delivery tracking only for chat messages (kind 9).
        // Reactions (kind 7) and deletions (kind 5) don't need delivery status UI.
        if kind == 9 {
            let chat_message = self
                .message_aggregator
                .process_single_message(
                    &mdk_message,
                    &self.nostr,
                    MediaFile::find_by_group(&self.database, group_id).await?,
                )
                .await
                .map(|mut msg| {
                    msg.delivery_status = Some(DeliveryStatus::Sending);
                    msg
                })
                .map_err(|e| {
                    WhitenoiseError::from(anyhow::anyhow!("Failed to process message: {}", e))
                })?;

            AggregatedMessage::insert_message(&chat_message, group_id, &self.database)
                .await
                .map_err(|e| {
                    WhitenoiseError::from(anyhow::anyhow!("Failed to cache message: {}", e))
                })?;

            // Emit NewMessage so the UI shows it immediately (optimistic)
            self.message_stream_manager.emit(
                group_id,
                MessageUpdate {
                    trigger: UpdateTrigger::NewMessage,
                    message: chat_message,
                },
            );
        }

        // Spawn background task: retry publish + emit delivery status update
        let nostr = self.nostr.clone();
        let database = self.database.clone();
        let stream_manager = self.message_stream_manager.clone();
        let event_id_str = event_id.to_string();
        let group_id_clone = group_id.clone();
        let account_pubkey = account.pubkey;
        let is_chat_message = kind == 9;

        tokio::spawn(async move {
            if is_chat_message {
                Self::publish_with_retries(
                    &nostr,
                    message_event,
                    &account_pubkey,
                    &group_relays,
                    &event_id_str,
                    &group_id_clone,
                    &database,
                    &stream_manager,
                )
                .await;
            } else {
                // Reactions/deletions: best-effort publish, no delivery tracking
                if let Err(e) = nostr
                    .publish_event_to(message_event, &account_pubkey, &group_relays)
                    .await
                {
                    tracing::warn!(
                        target: "whitenoise::messages::delivery",
                        "Failed to publish kind {} event: {e}",
                        kind,
                    );
                }
            }
        });

        Ok(MessageWithTokens::new(mdk_message, tokens))
    }

    /// Update delivery status in the DB cache and emit a stream update.
    async fn update_and_emit_delivery_status(
        event_id: &str,
        group_id: &GroupId,
        status: &DeliveryStatus,
        database: &Database,
        stream_manager: &MessageStreamManager,
    ) {
        if let Err(e) =
            AggregatedMessage::update_delivery_status(event_id, group_id, status, database).await
        {
            tracing::warn!(
                target: "whitenoise::messages::delivery",
                "Failed to update delivery status in cache: {}",
                e
            );
            return;
        }

        // Re-fetch the full message to emit with updated status
        match AggregatedMessage::find_by_id(event_id, group_id, database).await {
            Ok(Some(updated_msg)) => {
                stream_manager.emit(
                    group_id,
                    MessageUpdate {
                        trigger: UpdateTrigger::DeliveryStatusChanged,
                        message: updated_msg,
                    },
                );
            }
            Ok(None) => {
                tracing::warn!(
                    target: "whitenoise::messages::delivery",
                    "Message {} not found in cache after delivery status update",
                    event_id
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::messages::delivery",
                    "Failed to fetch updated message for delivery status emission: {}",
                    e
                );
            }
        }
    }

    /// Publish an event with exponential backoff retries.
    ///
    /// Attempts up to 3 publishes with delays of 2s and 4s between retries.
    /// On success, updates the delivery status to `Sent(relay_count)`.
    /// On failure (all retries exhausted), updates to `Failed(reason)`.
    async fn publish_with_retries(
        nostr: &NostrManager,
        event: Event,
        account_pubkey: &PublicKey,
        relays: &[RelayUrl],
        event_id: &str,
        group_id: &GroupId,
        database: &Database,
        stream_manager: &Arc<MessageStreamManager>,
    ) {
        const MAX_ATTEMPTS: u32 = 3;
        let mut last_error = None;

        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                let delay = Duration::from_secs(1 << attempt);
                tracing::debug!(
                    target: "whitenoise::messages::delivery",
                    "Retrying message publish \
                     (attempt {}/{MAX_ATTEMPTS}), \
                     backing off {delay:?}",
                    attempt + 1,
                );
                tokio::time::sleep(delay).await;
            }

            match nostr
                .publish_event_to(event.clone(), account_pubkey, relays)
                .await
            {
                Ok(output) if !output.success.is_empty() => {
                    let status = DeliveryStatus::Sent(output.success.len());
                    Self::update_and_emit_delivery_status(
                        event_id,
                        group_id,
                        &status,
                        database,
                        stream_manager,
                    )
                    .await;
                    return;
                }
                Ok(output) => {
                    tracing::warn!(
                        target: "whitenoise::messages::delivery",
                        "Publish attempt {}/{MAX_ATTEMPTS}: \
                         no relay accepted (failed: {:?})",
                        attempt + 1,
                        output.failed.keys().collect::<Vec<_>>(),
                    );
                    last_error = Some("No relay accepted the message".to_string());
                }
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::messages::delivery",
                        "Publish attempt {}/{MAX_ATTEMPTS} \
                         failed: {e}",
                        attempt + 1,
                    );
                    last_error = Some(e.to_string());
                }
            }
        }

        // All retries exhausted — mark as failed
        let reason = last_error.unwrap_or_else(|| "Unknown error".to_string());
        let status = DeliveryStatus::Failed(reason);
        Self::update_and_emit_delivery_status(
            event_id,
            group_id,
            &status,
            database,
            stream_manager,
        )
        .await;
    }

    /// Retry publishing a failed message.
    ///
    /// Re-creates the MLS message event from the original message in the MDK store,
    /// updates the delivery status to `Sending`, and spawns a new background publish task.
    /// The retried message is repositioned to the current time in the chat history
    /// via `UpdateTrigger::MessageRetried`.
    pub async fn retry_message_publish(
        &self,
        account: &Account,
        group_id: &GroupId,
        event_id: &EventId,
    ) -> Result<()> {
        // Guard: only retry messages that are in Failed state to prevent duplicate publishes
        if let Ok(Some(cached)) =
            AggregatedMessage::find_by_id(&event_id.to_string(), group_id, &self.database).await
            && !matches!(cached.delivery_status, Some(DeliveryStatus::Failed(_)))
        {
            return Err(WhitenoiseError::from(anyhow::anyhow!(
                "Can only retry messages with Failed delivery status, got {:?}",
                cached.delivery_status
            )));
        }

        let mdk = self.create_mdk_for_account(account.pubkey)?;

        // Guard: fail immediately if no relays configured (before re-creating MLS message)
        let group_relays: Vec<RelayUrl> = mdk.get_relays(group_id)?.into_iter().collect();
        if group_relays.is_empty() {
            return Err(WhitenoiseError::GroupMissingRelays);
        }

        // Retrieve the original message from MDK
        let mdk_message =
            mdk.get_message(group_id, event_id)?
                .ok_or(WhitenoiseError::MdkCoreError(
                    mdk_core::error::Error::MessageNotFound,
                ))?;

        // Re-create the MLS message event for publishing
        let inner_event = UnsignedEvent::new(
            mdk_message.pubkey,
            mdk_message.created_at,
            mdk_message.kind,
            mdk_message.tags.clone(),
            &mdk_message.content,
        );
        let message_event = mdk.create_message(group_id, inner_event)?;

        // Update status to Sending and emit MessageRetried (includes reposition)
        let sending_status = DeliveryStatus::Sending;
        AggregatedMessage::update_delivery_status(
            &event_id.to_string(),
            group_id,
            &sending_status,
            &self.database,
        )
        .await
        .map_err(|e| {
            WhitenoiseError::from(anyhow::anyhow!("Failed to update delivery status: {}", e))
        })?;

        // Emit MessageRetried so the UI repositions the message
        if let Ok(Some(msg)) =
            AggregatedMessage::find_by_id(&event_id.to_string(), group_id, &self.database).await
        {
            self.message_stream_manager.emit(
                group_id,
                MessageUpdate {
                    trigger: UpdateTrigger::MessageRetried,
                    message: msg,
                },
            );
        }

        // Spawn background publish with retry
        let nostr = self.nostr.clone();
        let database = self.database.clone();
        let stream_manager = self.message_stream_manager.clone();
        let event_id_str = event_id.to_string();
        let group_id_clone = group_id.clone();
        let account_pubkey = account.pubkey;

        tokio::spawn(async move {
            Self::publish_with_retries(
                &nostr,
                message_event,
                &account_pubkey,
                &group_relays,
                &event_id_str,
                &group_id_clone,
                &database,
                &stream_manager,
            )
            .await;
        });

        Ok(())
    }

    /// Fetches all messages for a specific group with parsed tokens.
    ///
    /// This method retrieves all messages that have been sent to a particular group,
    /// parsing the content of each message to extract tokens (e.g., mentions, hashtags).
    /// The messages are returned with both the original message data and the parsed tokens.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The public key of the user requesting the messages. This is used to
    ///   fetch the appropriate account and verify access permissions.
    /// * `group_id` - The unique identifier of the group whose messages should be retrieved.
    pub async fn fetch_messages_for_group(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Vec<MessageWithTokens>> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let messages = mdk.get_messages(group_id, None)?;
        let messages_with_tokens = messages
            .iter()
            .map(|message| MessageWithTokens {
                message: message.clone(),
                tokens: self.nostr.parse(&message.content),
            })
            .collect::<Vec<MessageWithTokens>>();
        Ok(messages_with_tokens)
    }

    /// Fetch and aggregate messages for a group - Main consumer API
    ///
    /// Returns pre-aggregated messages from the cache. The cache is kept up-to-date by:
    /// - Event processor: Caches messages as they arrive (real-time updates)
    /// - Startup sync: Populates cache with existing messages on initialization
    ///
    /// # Arguments
    /// * `pubkey` - The public key of the user requesting messages
    /// * `group_id` - The group to fetch messages for
    pub async fn fetch_aggregated_messages_for_group(
        &self,
        pubkey: &PublicKey,
        group_id: &GroupId,
    ) -> Result<Vec<ChatMessage>> {
        Account::find_by_pubkey(pubkey, &self.database).await?; // Verify account exists (security check)

        AggregatedMessage::find_messages_by_group(group_id, &self.database)
            .await
            .map_err(|e| {
                WhitenoiseError::from(anyhow::anyhow!("Failed to read cached messages: {}", e))
            })
    }

    /// Creates an unsigned nostr event with the given parameters
    fn create_unsigned_nostr_event(
        &self,
        pubkey: &PublicKey,
        message: &String,
        kind: u16,
        tags: Option<Vec<Tag>>,
    ) -> Result<(UnsignedEvent, EventId)> {
        let final_tags = tags.unwrap_or_default();

        let mut inner_event =
            UnsignedEvent::new(*pubkey, Timestamp::now(), kind.into(), final_tags, message);

        inner_event.ensure_id();

        let event_id = inner_event.id.unwrap(); // This is guaranteed to be Some by ensure_id

        Ok((inner_event, event_id))
    }

    /// Synchronize message cache with MDK on startup
    ///
    /// MUST be called BEFORE event processor starts to avoid race conditions.
    /// Uses simple count comparison to detect sync needs, then incrementally syncs missing events.
    pub(crate) async fn sync_message_cache_on_startup(&self) -> Result<()> {
        tracing::info!(
            target: "whitenoise::cache",
            "Starting message cache synchronization..."
        );

        let mut total_synced = 0;
        let mut total_groups_checked = 0;

        let accounts = Account::all(&self.database).await?;

        for account in accounts {
            let mdk = self.create_mdk_for_account(account.pubkey)?;
            let groups = mdk.get_groups()?;

            for group_info in groups {
                total_groups_checked += 1;

                let mdk_messages = mdk.get_messages(&group_info.mls_group_id, None)?;

                if self
                    .cache_needs_sync(&group_info.mls_group_id, &mdk_messages)
                    .await?
                {
                    tracing::info!(
                        target: "whitenoise::cache",
                        "Syncing cache for group {} (account {}): {} events",
                        hex::encode(group_info.mls_group_id.as_slice()),
                        account.pubkey.to_hex(),
                        mdk_messages.len()
                    );

                    self.sync_cache_for_group(
                        &account.pubkey,
                        &group_info.mls_group_id,
                        mdk_messages,
                    )
                    .await?;

                    total_synced += 1;
                }
            }
        }

        tracing::info!(
            target: "whitenoise::cache",
            "Message cache synchronization complete: synced {}/{} groups",
            total_synced,
            total_groups_checked
        );

        Ok(())
    }

    async fn cache_needs_sync(&self, group_id: &GroupId, mdk_messages: &[Message]) -> Result<bool> {
        if mdk_messages.is_empty() {
            return Ok(false);
        }

        let cached_count = AggregatedMessage::count_by_group(group_id, &self.database)
            .await
            .map_err(|e| {
                WhitenoiseError::from(anyhow::anyhow!("Failed to count cached events: {}", e))
            })?;

        if mdk_messages.len() != cached_count {
            tracing::debug!(
                target: "whitenoise::cache",
                "Cache count mismatch for group {}: MDK={}, Cache={}",
                hex::encode(group_id.as_slice()),
                mdk_messages.len(),
                cached_count
            );
            return Ok(true);
        }

        Ok(false)
    }

    /// Synchronize cache for a specific group
    ///
    /// Filters out events already in cache, then processes and saves only new events.
    async fn sync_cache_for_group(
        &self,
        pubkey: &PublicKey,
        group_id: &GroupId,
        mdk_messages: Vec<Message>,
    ) -> Result<()> {
        if mdk_messages.is_empty() {
            return Ok(());
        }

        let cached_ids = AggregatedMessage::get_all_event_ids_by_group(group_id, &self.database)
            .await
            .map_err(|e| {
                WhitenoiseError::from(anyhow::anyhow!("Failed to get cached event IDs: {}", e))
            })?;

        let new_events: Vec<Message> = mdk_messages
            .into_iter()
            .filter(|msg| !cached_ids.contains(&msg.id.to_string()))
            .collect();

        if new_events.is_empty() {
            tracing::debug!(
                target: "whitenoise::cache",
                "No new events to sync for group {}",
                hex::encode(group_id.as_slice())
            );
            return Ok(());
        }

        let num_new_events = new_events.len();

        tracing::info!(
            target: "whitenoise::cache",
            "Found {} new events to cache for group {}",
            num_new_events,
            hex::encode(group_id.as_slice())
        );

        let media_files = MediaFile::find_by_group(&self.database, group_id).await?;

        let processed_messages = self
            .message_aggregator
            .aggregate_messages_for_group(
                pubkey,
                group_id,
                new_events.clone(),
                &self.nostr,
                media_files,
            )
            .await
            .map_err(|e| {
                WhitenoiseError::from(anyhow::anyhow!("Message aggregation failed: {}", e))
            })?;

        AggregatedMessage::save_events(new_events, processed_messages, group_id, &self.database)
            .await
            .map_err(|e| {
                WhitenoiseError::from(anyhow::anyhow!("Failed to save events to cache: {}", e))
            })?;

        tracing::debug!(
            target: "whitenoise::cache",
            "Successfully synced {} new events for group {}",
            num_new_events,
            hex::encode(group_id.as_slice())
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::*;
    use std::time::Duration;

    /// Test successful message sending with various scenarios:
    /// - Default tags (None)
    /// - Custom tags (e.g., reply tags)
    /// - Token parsing (URLs and other special content)
    #[tokio::test]
    async fn test_send_message_to_group_success() {
        // Arrange: Setup whitenoise and create a group
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator_account, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Test 1: Basic message with no tags
        let basic_message = "Hello, world!".to_string();
        let result = whitenoise
            .send_message_to_group(
                &creator_account,
                &group.mls_group_id,
                basic_message.clone(),
                9,
                None,
            )
            .await;
        assert!(
            result.is_ok(),
            "Failed to send basic message: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().message.content, basic_message);

        // Test 2: Message with custom tags (reply scenario)
        let reply_message = "This is a reply".to_string();
        let tags = Some(vec![
            Tag::parse(vec!["e", &format!("{:0>64}", "abc123")]).unwrap(),
        ]);
        let result = whitenoise
            .send_message_to_group(
                &creator_account,
                &group.mls_group_id,
                reply_message.clone(),
                9,
                tags,
            )
            .await;
        assert!(
            result.is_ok(),
            "Failed to send message with tags: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().message.content, reply_message);

        // Test 3: Message with URL (token parsing)
        let url_message = "Check out https://example.com for info".to_string();
        let result = whitenoise
            .send_message_to_group(
                &creator_account,
                &group.mls_group_id,
                url_message.clone(),
                9,
                None,
            )
            .await;
        assert!(result.is_ok(), "Failed to send message with URL");
        let message_with_tokens = result.unwrap();
        assert_eq!(message_with_tokens.message.content, url_message);
        assert!(
            !message_with_tokens.tokens.is_empty(),
            "Expected URL to be parsed as token"
        );
    }

    /// Test error handling when sending to non-existent group
    #[tokio::test]
    async fn test_send_message_to_group_error_handling() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();

        // Try to send message to a non-existent group
        let fake_group_id = GroupId::from_slice(&[255u8; 32]);
        let result = whitenoise
            .send_message_to_group(
                &creator_account,
                &fake_group_id,
                "Test".to_string(),
                9,
                None,
            )
            .await;

        assert!(result.is_err(), "Expected error for non-existent group");
    }

    /// Test edge cases: empty content, long content, different event kinds
    #[tokio::test]
    async fn test_send_message_to_group_edge_cases() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator_account, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Test empty content
        let empty_result = whitenoise
            .send_message_to_group(
                &creator_account,
                &group.mls_group_id,
                String::new(),
                9,
                None,
            )
            .await;
        assert!(empty_result.is_ok(), "Empty message should be allowed");

        // Test long content
        let long_content = "A".repeat(10000);
        let long_result = whitenoise
            .send_message_to_group(
                &creator_account,
                &group.mls_group_id,
                long_content.clone(),
                9,
                None,
            )
            .await;
        assert!(long_result.is_ok(), "Long message should be sendable");
        assert_eq!(
            long_result.unwrap().message.content.len(),
            long_content.len()
        );

        // Test different event kinds
        for kind in [7, 9, 10] {
            let result = whitenoise
                .send_message_to_group(
                    &creator_account,
                    &group.mls_group_id,
                    format!("Kind {}", kind),
                    kind,
                    None,
                )
                .await;
            assert!(result.is_ok(), "Message with kind {} should succeed", kind);
        }
    }

    /// Test helper method: create_unsigned_nostr_event
    #[tokio::test]
    async fn test_create_unsigned_nostr_event() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let test_keys = create_test_keys();
        let pubkey = test_keys.public_key();

        // Test without tags
        let result = whitenoise.create_unsigned_nostr_event(&pubkey, &"Test".to_string(), 1, None);
        assert!(result.is_ok());
        let (event, event_id) = result.unwrap();
        assert_eq!(event.pubkey, pubkey);
        assert_eq!(event.content, "Test");
        assert!(event.tags.is_empty());
        assert_eq!(event.id.unwrap(), event_id);

        // Test with tags
        let tags = Some(vec![Tag::parse(vec!["e", "test_id"]).unwrap()]);
        let result = whitenoise.create_unsigned_nostr_event(&pubkey, &"Test".to_string(), 1, tags);
        assert!(result.is_ok());
        let (event, _) = result.unwrap();
        assert_eq!(event.tags.len(), 1);
    }

    /// Test message sending and retrieval integration
    #[tokio::test]
    async fn test_send_and_retrieve_message() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let admin_pubkeys = vec![creator_account.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator_account, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Send a message
        let message_content = "Test message for retrieval".to_string();
        let sent_result = whitenoise
            .send_message_to_group(
                &creator_account,
                &group.mls_group_id,
                message_content.clone(),
                9,
                None,
            )
            .await;
        assert!(sent_result.is_ok());

        // Retrieve and verify
        let messages = whitenoise
            .fetch_messages_for_group(&creator_account, &group.mls_group_id)
            .await
            .unwrap();

        assert!(!messages.is_empty(), "Should have at least one message");
        assert!(
            messages
                .iter()
                .any(|m| m.message.content == message_content),
            "Sent message should be retrievable"
        );
    }

    #[tokio::test]
    async fn test_cache_needs_sync_empty_mdk() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[1; 32]);

        // Empty MDK messages should not need sync
        let needs_sync = whitenoise.cache_needs_sync(&group_id, &[]).await.unwrap();
        assert!(!needs_sync);
    }

    #[tokio::test]
    async fn test_sync_cache_for_group_empty() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[2; 32]);
        let pubkey = nostr_sdk::Keys::generate().public_key();

        // Syncing empty messages should succeed without error
        let result = whitenoise
            .sync_cache_for_group(&pubkey, &group_id, vec![])
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_sync_cache_with_actual_messages() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create accounts
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        // Create a group
        let group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                crate::whitenoise::test_utils::create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        // Send a few messages — these are now proactively cached
        whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "Message 1".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "Message 2".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "Message 3".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        // Get messages from MDK
        let mdk = whitenoise.create_mdk_for_account(creator.pubkey).unwrap();
        let mdk_messages = mdk.get_messages(&group.mls_group_id, None).unwrap();

        // Verify we have 3 messages in MDK
        assert_eq!(mdk_messages.len(), 3);

        // With proactive caching, cache already has kind 9 messages.
        // But MDK also has kind 7/5 events (MLS protocol), so cache_needs_sync
        // compares total event count (MDK) vs cache count.
        // Cache only has kind 9 from proactive caching, sync fills in the rest.
        let cached_count =
            AggregatedMessage::count_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(
            cached_count, 3,
            "Proactive caching should have cached 3 kind-9 messages"
        );

        // Sync the cache (should be idempotent since messages already cached)
        whitenoise
            .sync_cache_for_group(&creator.pubkey, &group.mls_group_id, mdk_messages.clone())
            .await
            .unwrap();

        // Cache should not need sync anymore
        let needs_sync = whitenoise
            .cache_needs_sync(&group.mls_group_id, &mdk_messages)
            .await
            .unwrap();
        assert!(!needs_sync, "Cache should not need sync after syncing");

        // Verify we can fetch the messages from cache
        let messages =
            AggregatedMessage::find_messages_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(messages.len(), 3);
        assert!(messages[0].content.contains("Message"));
        assert!(messages[1].content.contains("Message"));
        assert!(messages[2].content.contains("Message"));
    }

    #[tokio::test]
    async fn test_incremental_sync() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create accounts and group
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                crate::whitenoise::test_utils::create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        // Send 2 messages — proactively cached
        whitenoise
            .send_message_to_group(&creator, &group.mls_group_id, "First".to_string(), 9, None)
            .await
            .unwrap();

        whitenoise
            .send_message_to_group(&creator, &group.mls_group_id, "Second".to_string(), 9, None)
            .await
            .unwrap();

        // Verify 2 messages already in cache (proactive caching)
        let cached_count =
            AggregatedMessage::count_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(cached_count, 2);

        // Sync the cache (should be idempotent for kind 9, may add other event types)
        let mdk = whitenoise.create_mdk_for_account(creator.pubkey).unwrap();
        let mdk_messages = mdk.get_messages(&group.mls_group_id, None).unwrap();
        whitenoise
            .sync_cache_for_group(&creator.pubkey, &group.mls_group_id, mdk_messages)
            .await
            .unwrap();

        // Send a 3rd message — also proactively cached
        whitenoise
            .send_message_to_group(&creator, &group.mls_group_id, "Third".to_string(), 9, None)
            .await
            .unwrap();

        // Verify 3 messages in cache
        let cached_count =
            AggregatedMessage::count_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(cached_count, 3);

        // Verify all messages are retrievable
        let messages =
            AggregatedMessage::find_messages_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(messages.len(), 3);

        let contents: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
        assert!(contents.contains(&"First".to_string()));
        assert!(contents.contains(&"Second".to_string()));
        assert!(contents.contains(&"Third".to_string()));
    }

    #[tokio::test]
    async fn test_sync_message_cache_on_startup() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create accounts and group
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                crate::whitenoise::test_utils::create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        // Send messages — proactively cached
        for i in 1..=5 {
            whitenoise
                .send_message_to_group(
                    &creator,
                    &group.mls_group_id,
                    format!("Startup test {}", i),
                    9,
                    None,
                )
                .await
                .unwrap();
        }

        // Cache already has 5 messages from proactive caching
        let cached_count =
            AggregatedMessage::count_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(cached_count, 5);

        // Run startup sync — should be idempotent
        whitenoise.sync_message_cache_on_startup().await.unwrap();

        // Cache should still have 5 messages
        let cached_count =
            AggregatedMessage::count_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(cached_count, 5);

        // Verify messages are correct
        let messages =
            AggregatedMessage::find_messages_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(messages.len(), 5);
        let contents: Vec<&str> = messages.iter().map(|m| m.content.as_str()).collect();
        for i in 1..=5 {
            let expected = format!("Startup test {}", i);
            assert!(
                contents.iter().any(|c| c.contains(&expected)),
                "Missing '{}' in cached messages",
                expected
            );
        }

        // Running startup sync again should be idempotent
        whitenoise.sync_message_cache_on_startup().await.unwrap();

        let cached_count =
            AggregatedMessage::count_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(cached_count, 5);
    }

    #[tokio::test]
    async fn test_fetch_aggregated_messages_reads_from_cache() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create accounts and group
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                crate::whitenoise::test_utils::create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        // Send messages
        for i in 1..=3 {
            whitenoise
                .send_message_to_group(
                    &creator,
                    &group.mls_group_id,
                    format!("Cache test {}", i),
                    9,
                    None,
                )
                .await
                .unwrap();
        }

        // Populate cache
        let mdk = whitenoise.create_mdk_for_account(creator.pubkey).unwrap();
        let mdk_messages = mdk.get_messages(&group.mls_group_id, None).unwrap();
        whitenoise
            .sync_cache_for_group(&creator.pubkey, &group.mls_group_id, mdk_messages)
            .await
            .unwrap();

        // Fetch messages via the main API - should read from cache
        let fetched_messages = whitenoise
            .fetch_aggregated_messages_for_group(&creator.pubkey, &group.mls_group_id)
            .await
            .unwrap();

        // Verify we got all 3 messages
        assert_eq!(fetched_messages.len(), 3);

        // Verify content (order not guaranteed for same-second messages)
        let contents: Vec<&str> = fetched_messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        for i in 1..=3 {
            let expected = format!("Cache test {}", i);
            assert!(
                contents.iter().any(|c| c.contains(&expected)),
                "Missing '{}' in cached messages",
                expected
            );
        }

        // Verify messages are ordered by created_at
        for i in 0..fetched_messages.len() - 1 {
            assert!(
                fetched_messages[i].created_at.as_secs()
                    <= fetched_messages[i + 1].created_at.as_secs(),
                "Messages should be ordered by timestamp"
            );
        }
    }

    #[tokio::test]
    async fn test_fetch_with_reactions_and_media() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create accounts and group
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                crate::whitenoise::test_utils::create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        // Send a message
        whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "Message with reactions".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        // Populate cache
        let mdk = whitenoise.create_mdk_for_account(creator.pubkey).unwrap();
        let mdk_messages = mdk.get_messages(&group.mls_group_id, None).unwrap();
        whitenoise
            .sync_cache_for_group(&creator.pubkey, &group.mls_group_id, mdk_messages)
            .await
            .unwrap();

        // Fetch messages - should include empty reactions and media
        let messages = whitenoise
            .fetch_aggregated_messages_for_group(&creator.pubkey, &group.mls_group_id)
            .await
            .unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "Message with reactions");

        // Verify reactions summary exists (even if empty)
        assert_eq!(messages[0].reactions.by_emoji.len(), 0);
        assert_eq!(messages[0].reactions.user_reactions.len(), 0);

        // Verify media attachments exists (even if empty)
        assert_eq!(messages[0].media_attachments.len(), 0);
    }

    /// Test publish_with_retries exhausts all attempts and marks status as Failed
    /// when relays are unreachable.
    #[tokio::test]
    async fn test_publish_with_retries_marks_failed_on_exhaustion() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let member = whitenoise.create_identity().await.unwrap();

        let group = whitenoise
            .create_group(
                &creator,
                vec![member.pubkey],
                create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        // Send a message (proactively cached with Sending status)
        let result = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "Retry test".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        let event_id = result.message.id.to_string();

        // Verify initial status is Sending
        let msg =
            AggregatedMessage::find_by_id(&event_id, &group.mls_group_id, &whitenoise.database)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(msg.delivery_status, Some(DeliveryStatus::Sending));

        // Build a test event for publish_with_retries
        let keys = Keys::generate();
        let event = EventBuilder::text_note("test")
            .sign_with_keys(&keys)
            .unwrap();

        // Call publish_with_retries directly with unreachable relays
        let unreachable_relays = vec![RelayUrl::parse("ws://127.0.0.1:1").unwrap()];

        Whitenoise::publish_with_retries(
            &whitenoise.nostr,
            event,
            &creator.pubkey,
            &unreachable_relays,
            &event_id,
            &group.mls_group_id,
            &whitenoise.database,
            &whitenoise.message_stream_manager,
        )
        .await;

        // Verify status transitioned to Failed
        let msg =
            AggregatedMessage::find_by_id(&event_id, &group.mls_group_id, &whitenoise.database)
                .await
                .unwrap()
                .unwrap();
        assert!(
            matches!(msg.delivery_status, Some(DeliveryStatus::Failed(_))),
            "Expected Failed status after exhausting retries, got {:?}",
            msg.delivery_status
        );
    }
}
