pub use crate::whitenoise::database::aggregated_messages::PaginationOptions;

use crate::{
    perf_instrument,
    types::MessageWithTokens,
    whitenoise::{
        Whitenoise,
        accounts::Account,
        aggregated_message::AggregatedMessage,
        chat_list_streaming::ChatListUpdateTrigger,
        error::Result,
        media_files::MediaFile,
        message_aggregator::{ChatMessage, SearchResult},
    },
};
use mdk_core::prelude::{message_types::Message, *};
use nostr_sdk::prelude::*;

impl Whitenoise {
    #[perf_instrument("messages")]
    pub async fn send_message_to_group(
        &self,
        account: &Account,
        group_id: &GroupId,
        message: String,
        kind: u16,
        tags: Option<Vec<Tag>>,
    ) -> Result<MessageWithTokens> {
        let session = self.require_session(&account.pubkey)?;
        let result = session
            .messages()
            .for_group(group_id)
            .send(message, kind, tags)
            .await?;

        if result.last_message_deleted {
            self.emit_chat_list_update_for_group(
                group_id,
                ChatListUpdateTrigger::LastMessageDeleted,
            )
            .await;
        }

        Ok(result.message)
    }

    #[perf_instrument("messages")]
    pub async fn retry_message_publish(
        &self,
        account: &Account,
        group_id: &GroupId,
        event_id: &EventId,
    ) -> Result<()> {
        let session = self.require_session(&account.pubkey)?;
        let result = session
            .messages()
            .for_group(group_id)
            .retry_publish(event_id)
            .await?;

        if result.last_message_deleted {
            self.emit_chat_list_update_for_group(
                group_id,
                ChatListUpdateTrigger::LastMessageDeleted,
            )
            .await;
        }

        Ok(())
    }

    #[perf_instrument("messages")]
    pub async fn fetch_messages_for_group(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Vec<MessageWithTokens>> {
        let session = self.require_session(&account.pubkey)?;
        session.messages().for_group(group_id).fetch().await
    }

    #[perf_instrument("messages")]
    pub async fn fetch_aggregated_messages_for_group(
        &self,
        pubkey: &PublicKey,
        group_id: &GroupId,
        options: &PaginationOptions,
        limit: Option<u32>,
    ) -> Result<Vec<ChatMessage>> {
        let session = self.require_session(pubkey)?;
        session
            .messages()
            .for_group(group_id)
            .fetch_aggregated(options, limit)
            .await
    }

    #[perf_instrument("messages")]
    pub async fn fetch_messages_unread_with_minimum(
        &self,
        pubkey: &PublicKey,
        group_id: &GroupId,
        minimum: Option<u32>,
    ) -> Result<Vec<ChatMessage>> {
        let session = self.require_session(pubkey)?;
        session
            .messages()
            .for_group(group_id)
            .fetch_unread_with_minimum(minimum)
            .await
    }

    #[perf_instrument("messages")]
    pub async fn fetch_message_by_id(
        &self,
        pubkey: &PublicKey,
        group_id: &GroupId,
        message_id: &str,
    ) -> Result<Option<ChatMessage>> {
        let session = self.require_session(pubkey)?;
        session
            .messages()
            .for_group(group_id)
            .fetch_by_id(message_id)
            .await
    }

    pub async fn search_messages_in_group(
        &self,
        pubkey: &PublicKey,
        group_id: &GroupId,
        query: &str,
        limit: Option<u32>,
    ) -> Result<Vec<SearchResult>> {
        let session = self.require_session(pubkey)?;
        session
            .messages()
            .for_group(group_id)
            .search(query, limit)
            .await
    }

    pub async fn search_messages(
        &self,
        pubkey: &PublicKey,
        query: &str,
        limit: Option<u32>,
    ) -> Result<Vec<SearchResult>> {
        let session = self.require_session(pubkey)?;
        session.messages().search(query, limit).await
    }

    /// Synchronize message cache with MDK on startup
    ///
    /// MUST be called BEFORE event processor starts to avoid race conditions.
    /// Uses simple count comparison to detect sync needs, then incrementally syncs missing events.
    #[perf_instrument("messages")]
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

    #[perf_instrument("messages")]
    async fn cache_needs_sync(&self, group_id: &GroupId, mdk_messages: &[Message]) -> Result<bool> {
        if mdk_messages.is_empty() {
            return Ok(false);
        }

        let cached_count = AggregatedMessage::count_by_group(group_id, &self.database).await?;

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
    #[perf_instrument("messages")]
    async fn sync_cache_for_group(
        &self,
        pubkey: &PublicKey,
        group_id: &GroupId,
        mdk_messages: Vec<Message>,
    ) -> Result<()> {
        if mdk_messages.is_empty() {
            return Ok(());
        }

        let cached_ids =
            AggregatedMessage::get_all_event_ids_by_group(group_id, &self.database).await?;

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
                &self.content_parser,
                media_files,
            )
            .await?;

        AggregatedMessage::save_events(new_events, processed_messages, group_id, &self.database)
            .await?;

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
    use std::time::Duration;

    use super::*;
    use crate::whitenoise::error::WhitenoiseError;
    use crate::whitenoise::message_aggregator::DeliveryStatus;
    use crate::whitenoise::message_streaming::{MessageStreamManager, UpdateTrigger};
    use crate::whitenoise::session::messages::cascade_delivery_failure;
    use crate::whitenoise::test_utils::*;

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

        // Test supported event kinds (7=reaction needs an e-tag, so skip here; 5=deletion is fine)
        for kind in [5, 9] {
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

        // Unsupported kind should fail
        let result = whitenoise
            .send_message_to_group(
                &creator_account,
                &group.mls_group_id,
                "Kind 10".to_string(),
                10,
                None,
            )
            .await;
        assert!(
            result.is_err(),
            "Unsupported event kind 10 should return an error"
        );
    }

    /// Test helper method: create_unsigned_nostr_event
    #[tokio::test]
    async fn test_create_unsigned_nostr_event() {
        let test_keys = create_test_keys();
        let pubkey = test_keys.public_key();

        use crate::whitenoise::session::messages::create_unsigned_nostr_event;

        // Test without tags
        let result = create_unsigned_nostr_event(&pubkey, "Test", 1, None);
        assert!(result.is_ok());
        let (event, event_id) = result.unwrap();
        assert_eq!(event.pubkey, pubkey);
        assert_eq!(event.content, "Test");
        assert!(event.tags.is_empty());
        assert_eq!(event.id.unwrap(), event_id);

        // Test with tags
        let tags = Some(vec![Tag::parse(vec!["e", "test_id"]).unwrap()]);
        let result = create_unsigned_nostr_event(&pubkey, "Test", 1, tags);
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
        let messages = AggregatedMessage::find_messages_by_group(
            &group.mls_group_id,
            &creator.pubkey,
            None,
            &whitenoise.database,
        )
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
        let messages = AggregatedMessage::find_messages_by_group(
            &group.mls_group_id,
            &creator.pubkey,
            None,
            &whitenoise.database,
        )
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
        let messages = AggregatedMessage::find_messages_by_group(
            &group.mls_group_id,
            &creator.pubkey,
            None,
            &whitenoise.database,
        )
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
            .fetch_aggregated_messages_for_group(
                &creator.pubkey,
                &group.mls_group_id,
                &PaginationOptions::default(),
                None,
            )
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
            .fetch_aggregated_messages_for_group(
                &creator.pubkey,
                &group.mls_group_id,
                &PaginationOptions::default(),
                None,
            )
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

    /// Test that sending a non-kind-9 message (e.g., reaction) skips proactive caching
    /// and delivery tracking but still succeeds.
    #[tokio::test]
    async fn test_send_reaction_tracks_delivery_and_applies_to_parent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let admin_pubkeys = vec![creator.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // First send a kind 9 message (needed as reaction target)
        let chat_result = whitenoise
            .send_message_to_group(&creator, &group.mls_group_id, "Hello".to_string(), 9, None)
            .await
            .unwrap();

        // Verify kind 9 was cached
        let cached_count =
            AggregatedMessage::count_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(cached_count, 1, "kind 9 message should be cached");

        // Let the background publish_with_retries from the kind 9 message settle
        // to avoid SQLite write contention with the reaction's delivery status update.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Send a kind 7 (reaction) — should be cached with delivery tracking
        let reaction_tags = Some(vec![
            Tag::parse(vec!["e", &chat_result.message.id.to_hex()]).unwrap(),
        ]);
        let reaction_result = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "+".to_string(),
                7,
                reaction_tags,
            )
            .await;
        assert!(
            reaction_result.is_ok(),
            "Reaction send should succeed: {:?}",
            reaction_result.err()
        );

        // Cache count should be 2 (kind 9 + kind 7)
        let cached_count =
            AggregatedMessage::count_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(
            cached_count, 2,
            "kind 7 reaction should be cached alongside kind 9 message"
        );

        // The reaction should have been applied to the parent message
        let parent = AggregatedMessage::find_by_id(
            &chat_result.message.id.to_string(),
            &group.mls_group_id,
            &creator.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            !parent.reactions.by_emoji.is_empty(),
            "Parent message should have the reaction applied"
        );
    }

    /// Test that retry_message_publish rejects a message not in Failed state.
    #[tokio::test]
    async fn test_retry_rejects_non_failed_status() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let admin_pubkeys = vec![creator.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Send a message — it will be cached with Sending status
        let result = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "Retry guard test".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        let event_id = result.message.id;

        // Verify status is Sending
        let msg = AggregatedMessage::find_by_id(
            &event_id.to_string(),
            &group.mls_group_id,
            &creator.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(msg.delivery_status, Some(DeliveryStatus::Sending));

        // Attempt to retry — should be rejected because status is Sending, not Failed
        let retry_result = whitenoise
            .retry_message_publish(&creator, &group.mls_group_id, &event_id)
            .await;
        assert!(
            retry_result.is_err(),
            "Should reject retry for non-Failed status"
        );

        let err_msg = retry_result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Failed delivery status"),
            "Error should mention Failed status requirement, got: {}",
            err_msg
        );
    }

    /// Test that retry_message_publish rejects a message not found in cache.
    #[tokio::test]
    async fn test_retry_rejects_uncached_message() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let admin_pubkeys = vec![creator.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Use a fabricated event ID that doesn't exist in the cache
        let fake_event_id = EventId::all_zeros();

        let retry_result = whitenoise
            .retry_message_publish(&creator, &group.mls_group_id, &fake_event_id)
            .await;
        assert!(
            retry_result.is_err(),
            "Should reject retry for uncached message"
        );

        let err = retry_result.unwrap_err();
        assert!(
            matches!(err, WhitenoiseError::MessageNotFound),
            "Expected MessageNotFound for uncached retry, got: {err:?}"
        );
    }

    /// Test the full retry happy path: creates a new message, marks original as Retried.
    #[tokio::test]
    async fn test_retry_happy_path_from_failed_status() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let admin_pubkeys = vec![creator.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Send a message (proactively cached with Sending status)
        let result = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "Retry happy path".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        let original_event_id = result.message.id;

        // Sleep 1s so the Nostr timestamp (1-second granularity) differs,
        // producing a distinct inner event ID for the retry.
        // Also lets background publish_with_retries finish before we override the status.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // Manually mark the message as Failed (simulating exhausted retries).
        // Done after the sleep so background publish_with_retries doesn't overwrite it.
        AggregatedMessage::update_delivery_status(
            &original_event_id.to_string(),
            &group.mls_group_id,
            &creator.pubkey,
            &DeliveryStatus::Failed("simulated failure".to_string()),
            &whitenoise.database,
        )
        .await
        .unwrap();

        // Verify status is Failed
        let msg = AggregatedMessage::find_by_id(
            &original_event_id.to_string(),
            &group.mls_group_id,
            &creator.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(
            msg.delivery_status,
            Some(DeliveryStatus::Failed(_))
        ));

        // Subscribe before retry so we can verify live delivery status transition.
        let mut updates = whitenoise
            .subscribe_to_group_messages(&creator.pubkey, &group.mls_group_id, None)
            .await
            .unwrap()
            .updates;

        // Retry should succeed — creates a new message, marks original as Retried
        let retry_result = whitenoise
            .retry_message_publish(&creator, &group.mls_group_id, &original_event_id)
            .await;
        assert!(
            retry_result.is_ok(),
            "Retry from Failed should succeed: {:?}",
            retry_result.err()
        );

        // Original message should now be marked as Retried
        let original_msg = AggregatedMessage::find_by_id(
            &original_event_id.to_string(),
            &group.mls_group_id,
            &creator.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            original_msg.delivery_status,
            Some(DeliveryStatus::Retried),
            "Original should be marked as Retried, got {:?}",
            original_msg.delivery_status
        );

        // A new message should exist in cache in a non-failure state.
        // The background publish may complete before we read, so accept Sending OR Sent.
        let all_messages = AggregatedMessage::find_messages_by_group(
            &group.mls_group_id,
            &creator.pubkey,
            None,
            &whitenoise.database,
        )
        .await
        .unwrap();

        // find_messages_by_group excludes Retried, so only the new message should appear
        let new_msg = all_messages
            .iter()
            .find(|m| m.content == "Retry happy path" && m.id != original_event_id.to_string())
            .expect("New retry message should exist in cache");
        assert!(
            matches!(
                new_msg.delivery_status,
                Some(DeliveryStatus::Sending) | Some(DeliveryStatus::Sent(_))
            ),
            "New message should have Sending or Sent status, got {:?}",
            new_msg.delivery_status
        );

        let retried_update = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let update = updates.recv().await.expect("stream should remain open");
                if update.trigger == UpdateTrigger::DeliveryStatusChanged
                    && update.message.id == original_event_id.to_string()
                {
                    return update;
                }
            }
        })
        .await
        .expect("Should receive DeliveryStatusChanged for original retried message");

        assert!(matches!(
            retried_update.message.delivery_status,
            Some(DeliveryStatus::Retried)
        ));
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
        let msg = AggregatedMessage::find_by_id(
            &event_id,
            &group.mls_group_id,
            &creator.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(msg.delivery_status, Some(DeliveryStatus::Sending));

        // Build a test event for bounded relay-control publish
        let keys = Keys::generate();
        let event = EventBuilder::text_note("test")
            .sign_with_keys(&keys)
            .unwrap();

        // Call the relay-control publish helper directly with unreachable relays.
        // Use max_publish_attempts=1 so there are no retry sleeps and the test
        // runs without needing to pause the Tokio clock (which would break DB
        // pool timeouts on the status-update write inside publish_message_event).
        let unreachable_relays = vec![RelayUrl::parse("ws://127.0.0.1:1").unwrap()];
        let ephemeral = crate::relay_control::ephemeral::EphemeralPlane::new(
            crate::relay_control::ephemeral::EphemeralPlaneConfig {
                timeout: std::time::Duration::from_millis(200),
                reconnect_policy:
                    crate::relay_control::sessions::RelaySessionReconnectPolicy::Disabled,
                auth_policy: crate::relay_control::sessions::RelaySessionAuthPolicy::Disabled,
                max_publish_attempts: 1,
                ad_hoc_relay_ttl: std::time::Duration::from_secs(30),
            },
            whitenoise.database.clone(),
            whitenoise.event_sender.clone(),
            whitenoise.relay_control.observability().clone(),
        );

        ephemeral
            .publish_message_event(
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
        let msg = AggregatedMessage::find_by_id(
            &event_id,
            &group.mls_group_id,
            &creator.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(msg.delivery_status, Some(DeliveryStatus::Failed(_))),
            "Expected Failed status after exhausting retries, got {:?}",
            msg.delivery_status
        );
    }

    /// Test that cache_needs_sync returns false when there are no MDK messages.
    #[tokio::test]
    async fn test_cache_needs_sync_empty_messages() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let group_id = GroupId::from_slice(&[42u8; 32]);
        let empty_messages: Vec<Message> = vec![];

        let needs_sync = whitenoise
            .cache_needs_sync(&group_id, &empty_messages)
            .await
            .unwrap();
        assert!(!needs_sync, "Empty MDK messages should not need sync");
    }

    /// Test that cache_needs_sync detects a count mismatch between MDK and cache.
    #[tokio::test]
    async fn test_cache_needs_sync_detects_mismatch() {
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

        // Send messages (proactively cached)
        for i in 1..=3 {
            whitenoise
                .send_message_to_group(
                    &creator,
                    &group.mls_group_id,
                    format!("Sync detect {}", i),
                    9,
                    None,
                )
                .await
                .unwrap();
        }

        let mdk = whitenoise.create_mdk_for_account(creator.pubkey).unwrap();
        let mdk_messages = mdk.get_messages(&group.mls_group_id, None).unwrap();

        // Cache is in sync — should return false
        let needs_sync = whitenoise
            .cache_needs_sync(&group.mls_group_id, &mdk_messages)
            .await
            .unwrap();
        assert!(
            !needs_sync,
            "Cache should be in sync after proactive caching"
        );

        // Delete the cache to simulate stale state
        AggregatedMessage::delete_by_group(&group.mls_group_id, &whitenoise.database)
            .await
            .unwrap();

        // Now cache count (0) != MDK count (3) — should return true
        let needs_sync = whitenoise
            .cache_needs_sync(&group.mls_group_id, &mdk_messages)
            .await
            .unwrap();
        assert!(needs_sync, "Cache should need sync after deletion");
    }

    /// Test that sync_cache_for_group recovers a cleared cache.
    ///
    /// Exercises the full sync path: fetching cached IDs, filtering new events,
    /// aggregating messages, and saving to the cache.
    #[tokio::test]
    async fn test_sync_cache_for_group_recovers_cleared_cache() {
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

        // Send messages (proactively cached)
        for i in 1..=3 {
            whitenoise
                .send_message_to_group(
                    &creator,
                    &group.mls_group_id,
                    format!("Recover {}", i),
                    9,
                    None,
                )
                .await
                .unwrap();
        }

        // Get MDK messages before clearing cache
        let mdk = whitenoise.create_mdk_for_account(creator.pubkey).unwrap();
        let mdk_messages = mdk.get_messages(&group.mls_group_id, None).unwrap();
        assert!(!mdk_messages.is_empty());

        // Clear the cache entirely
        AggregatedMessage::delete_by_group(&group.mls_group_id, &whitenoise.database)
            .await
            .unwrap();
        let cached_count =
            AggregatedMessage::count_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(cached_count, 0, "Cache should be empty after deletion");

        // Sync should recover all messages from MDK
        whitenoise
            .sync_cache_for_group(&creator.pubkey, &group.mls_group_id, mdk_messages)
            .await
            .unwrap();

        // Verify messages were recovered
        let messages = AggregatedMessage::find_messages_by_group(
            &group.mls_group_id,
            &creator.pubkey,
            None,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(messages.len(), 3, "All 3 messages should be recovered");

        let contents: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
        for i in 1..=3 {
            assert!(
                contents
                    .iter()
                    .any(|c| c.contains(&format!("Recover {}", i))),
                "Missing 'Recover {}' in recovered messages",
                i
            );
        }
    }

    /// Test that sync_cache_for_group with empty input is a no-op.
    #[tokio::test]
    async fn test_sync_cache_for_group_empty_input() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();

        let group_id = GroupId::from_slice(&[99u8; 32]);
        let empty_messages: Vec<Message> = vec![];

        // Should return Ok without touching the database
        whitenoise
            .sync_cache_for_group(&creator.pubkey, &group_id, empty_messages)
            .await
            .unwrap();

        let cached_count = AggregatedMessage::count_by_group(&group_id, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(cached_count, 0);
    }

    /// Test that sync_message_cache_on_startup recovers a stale cache.
    ///
    /// This exercises the full startup sync flow: iterating accounts, checking
    /// each group for sync needs, and syncing when a mismatch is detected.
    #[tokio::test]
    async fn test_sync_message_cache_on_startup_recovers_stale_cache() {
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

        // Send messages (proactively cached)
        for i in 1..=4 {
            whitenoise
                .send_message_to_group(
                    &creator,
                    &group.mls_group_id,
                    format!("Startup recover {}", i),
                    9,
                    None,
                )
                .await
                .unwrap();
        }

        // Verify proactive cache has 4 messages
        let cached_count =
            AggregatedMessage::count_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(cached_count, 4);

        // Clear the cache to simulate a stale/corrupt state
        AggregatedMessage::delete_by_group(&group.mls_group_id, &whitenoise.database)
            .await
            .unwrap();
        let cached_count =
            AggregatedMessage::count_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();
        assert_eq!(cached_count, 0, "Cache should be empty");

        // Run startup sync — should detect mismatch and recover
        whitenoise.sync_message_cache_on_startup().await.unwrap();

        // Verify all messages were recovered
        let messages = AggregatedMessage::find_messages_by_group(
            &group.mls_group_id,
            &creator.pubkey,
            None,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(messages.len(), 4, "All 4 messages should be recovered");

        let contents: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
        for i in 1..=4 {
            assert!(
                contents
                    .iter()
                    .any(|c| c.contains(&format!("Startup recover {}", i))),
                "Missing 'Startup recover {}' in recovered messages",
                i
            );
        }
    }

    /// Test sending a kind 5 (deletion) tracks delivery and applies to target message.
    #[tokio::test]
    async fn test_send_deletion_tracks_delivery_and_marks_target() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let admin_pubkeys = vec![creator.pubkey];
        let config = create_nostr_group_config_data(admin_pubkeys);
        let group = whitenoise
            .create_group(&creator, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Send a kind 9 message to delete later
        let chat_result = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "Delete me".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let target_id = chat_result.message.id.to_hex();

        // Send a kind 5 (deletion) targeting the message
        let deletion_tags = Some(vec![Tag::parse(vec!["e", &target_id]).unwrap()]);
        let del_result = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                String::new(),
                5,
                deletion_tags,
            )
            .await;
        assert!(
            del_result.is_ok(),
            "Deletion send should succeed: {:?}",
            del_result.err()
        );

        // find_by_id should return the message but with is_deleted=true
        let target = AggregatedMessage::find_by_id(
            &target_id,
            &group.mls_group_id,
            &creator.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(target.is_deleted, "Message should be marked as deleted");

        // Also verify via find_messages_by_group — message should have is_deleted flag
        let messages = AggregatedMessage::find_messages_by_group(
            &group.mls_group_id,
            &creator.pubkey,
            None,
            &whitenoise.database,
        )
        .await
        .unwrap();
        let target_msg = messages.iter().find(|m| m.id == target_id).unwrap();
        assert!(
            target_msg.is_deleted,
            "Message should be marked as deleted in find_messages_by_group"
        );

        // The deletion event (kind 5) should have delivery status
        let del_event_id = del_result.unwrap().message.id.to_string();
        let has_status = AggregatedMessage::has_delivery_status(
            &del_event_id,
            &group.mls_group_id,
            &creator.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert!(has_status, "Deletion event should have delivery status");
    }

    /// Test cascade_delivery_failure for kind 7 (reaction) removes reaction from parent.
    #[tokio::test]
    async fn test_cascade_reaction_failure_removes_from_parent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let config = create_nostr_group_config_data(vec![creator.pubkey]);
        let group = whitenoise
            .create_group(&creator, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Send a kind 9 message as reaction target
        let chat_result = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "React to me".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        // Send a kind 7 (reaction)
        let target_id = chat_result.message.id.to_hex();
        let reaction_tags = Some(vec![Tag::parse(vec!["e", &target_id]).unwrap()]);
        let reaction_result = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "+".to_string(),
                7,
                reaction_tags,
            )
            .await
            .unwrap();

        // Verify reaction was applied to parent
        let parent = AggregatedMessage::find_by_id(
            &target_id,
            &group.mls_group_id,
            &creator.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            !parent.reactions.by_emoji.is_empty(),
            "Parent should have reaction applied"
        );

        // Now simulate cascade failure for the reaction
        let reaction_event_id = reaction_result.message.id.to_string();
        let tags = Tags::from_list(vec![Tag::parse(vec!["e", &target_id]).unwrap()]);
        let stream_manager = MessageStreamManager::new();

        cascade_delivery_failure(
            7,
            &reaction_event_id,
            &tags,
            &creator.pubkey,
            "+",
            &group.mls_group_id,
            &whitenoise.database,
            &stream_manager,
        )
        .await;

        // Parent should no longer have the reaction
        let parent = AggregatedMessage::find_by_id(
            &target_id,
            &group.mls_group_id,
            &creator.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            parent.reactions.by_emoji.is_empty(),
            "Reaction should be removed after cascade failure"
        );
    }

    /// Test cascade_delivery_failure for kind 5 (deletion) unmarks targets.
    #[tokio::test]
    async fn test_cascade_deletion_failure_unmarks_targets() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let config = create_nostr_group_config_data(vec![creator.pubkey]);
        let group = whitenoise
            .create_group(&creator, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Send a kind 9 message to delete
        let chat_result = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "Delete and cascade".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let target_id = chat_result.message.id.to_hex();

        // Send a kind 5 deletion
        let deletion_tags = Some(vec![Tag::parse(vec!["e", &target_id]).unwrap()]);
        let del_result = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                String::new(),
                5,
                deletion_tags,
            )
            .await
            .unwrap();

        // Verify message is marked as deleted
        let messages = AggregatedMessage::find_messages_by_group(
            &group.mls_group_id,
            &creator.pubkey,
            None,
            &whitenoise.database,
        )
        .await
        .unwrap();
        let target_msg = messages.iter().find(|m| m.id == target_id).unwrap();
        assert!(target_msg.is_deleted, "Message should be marked as deleted");

        // Cascade the deletion failure — reverses the optimistic deletion
        let del_event_id = del_result.message.id.to_string();
        let tags = Tags::from_list(vec![Tag::parse(vec!["e", &target_id]).unwrap()]);
        let stream_manager = MessageStreamManager::new();

        cascade_delivery_failure(
            5,
            &del_event_id,
            &tags,
            &creator.pubkey,
            "",
            &group.mls_group_id,
            &whitenoise.database,
            &stream_manager,
        )
        .await;

        // Message should no longer be deleted after cascade
        let messages = AggregatedMessage::find_messages_by_group(
            &group.mls_group_id,
            &creator.pubkey,
            None,
            &whitenoise.database,
        )
        .await
        .unwrap();
        let target_msg = messages.iter().find(|m| m.id == target_id).unwrap();
        assert!(
            !target_msg.is_deleted,
            "Message should be un-deleted after deletion cascade failure"
        );
    }

    /// Test cascade_delivery_failure is a no-op for kind 9.
    #[tokio::test]
    async fn test_cascade_kind9_is_noop() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[199; 32]);
        let author = Keys::generate().public_key();
        let stream_manager = MessageStreamManager::new();

        // Should not panic or error — just a no-op
        cascade_delivery_failure(
            9,
            "some_event_id",
            &Tags::from_list(vec![]),
            &author,
            "",
            &group_id,
            &whitenoise.database,
            &stream_manager,
        )
        .await;
    }

    /// Test that sending an unsupported event kind returns an error.
    #[tokio::test]
    async fn test_send_unsupported_kind_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let config = create_nostr_group_config_data(vec![creator.pubkey]);
        let group = whitenoise
            .create_group(&creator, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Kind 10 should be rejected
        let result = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "unsupported".to_string(),
                10,
                None,
            )
            .await;
        assert!(result.is_err(), "Kind 10 should be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Unsupported"),
            "Error should mention unsupported kind, got: {err_msg}"
        );
    }

    /// Test cascade_delivery_failure for kind 7 when parent message is not cached.
    /// Should handle gracefully (warn + return) without panic.
    #[tokio::test]
    async fn test_cascade_reaction_failure_parent_not_found() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[200; 32]);
        let author = Keys::generate().public_key();
        let stream_manager = MessageStreamManager::new();

        // Point reaction at a target that doesn't exist in cache
        let nonexistent_target = format!("{:0>64x}", 0xdeadbeefu64);
        let tags = Tags::from_list(vec![Tag::parse(vec!["e", &nonexistent_target]).unwrap()]);

        // Should return cleanly without panic — parent not found is handled
        cascade_delivery_failure(
            7,
            "some_reaction_id",
            &tags,
            &author,
            "+",
            &group_id,
            &whitenoise.database,
            &stream_manager,
        )
        .await;
    }

    /// Test cascade_delivery_failure for kind 7 with no e-tag at all.
    /// Should handle the extract_reaction_target_id error gracefully.
    #[tokio::test]
    async fn test_cascade_reaction_failure_missing_etag() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[201; 32]);
        let author = Keys::generate().public_key();
        let stream_manager = MessageStreamManager::new();

        // Empty tags — no e-tag to extract
        cascade_delivery_failure(
            7,
            "some_reaction_id",
            &Tags::new(),
            &author,
            "+",
            &group_id,
            &whitenoise.database,
            &stream_manager,
        )
        .await;
    }

    /// Test that deleting the last message in a group exercises the
    /// find_last_by_group_ids path in cache_and_apply_outgoing_deletion.
    #[tokio::test]
    async fn test_send_deletion_of_last_message_exercises_last_check() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let config = create_nostr_group_config_data(vec![creator.pubkey]);
        let group = whitenoise
            .create_group(&creator, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Send two messages so we can delete the last one
        whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "First message".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let second = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                "Second (last) message".to_string(),
                9,
                None,
            )
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let last_id = second.message.id.to_hex();

        // Delete the last message — exercises find_last_by_group_ids + LastMessageDeleted path
        let deletion_tags = Some(vec![Tag::parse(vec!["e", &last_id]).unwrap()]);
        let del_result = whitenoise
            .send_message_to_group(
                &creator,
                &group.mls_group_id,
                String::new(),
                5,
                deletion_tags,
            )
            .await;
        assert!(del_result.is_ok(), "Deletion should succeed");

        // The second message should be marked deleted
        let msg = AggregatedMessage::find_by_id(
            &last_id,
            &group.mls_group_id,
            &creator.pubkey,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(msg.is_deleted, "Last message should be marked deleted");
    }

    /// Test cascade_delivery_failure for kind 7 handles DB errors gracefully.
    #[tokio::test]
    async fn test_cascade_reaction_failure_handles_db_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[202; 32]);
        let author = Keys::generate().public_key();
        let target_id = format!("{:0>64x}", 0xabc123u64);
        let tags = Tags::from_list(vec![Tag::parse(vec!["e", &target_id]).unwrap()]);
        let stream_manager = MessageStreamManager::new();

        // Close the pool to force DB errors on find_by_id
        whitenoise.database.pool.close().await;

        cascade_delivery_failure(
            7,
            "reaction_event_id",
            &tags,
            &author,
            "+",
            &group_id,
            &whitenoise.database,
            &stream_manager,
        )
        .await;

        // Should complete without panic — error is logged at error! level
    }

    /// Test cascade_delivery_failure for kind 5 handles DB errors gracefully.
    #[tokio::test]
    async fn test_cascade_deletion_failure_handles_db_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let group_id = GroupId::from_slice(&[203; 32]);
        let target_id = format!("{:0>64x}", 0xdef456u64);
        let tags = Tags::from_list(vec![Tag::parse(vec!["e", &target_id]).unwrap()]);
        let stream_manager = MessageStreamManager::new();

        // Close the pool to force DB errors on unmark_deleted
        whitenoise.database.pool.close().await;

        cascade_delivery_failure(
            5,
            "deletion_event_id",
            &tags,
            &Keys::generate().public_key(),
            "",
            &group_id,
            &whitenoise.database,
            &stream_manager,
        )
        .await;

        // Should complete without panic — error is logged at error! level
    }

    /// fetch_message_by_id returns Some with the correct message when the message exists.
    #[tokio::test]
    async fn test_fetch_message_by_id_returns_message_when_it_exists() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator,
                vec![member_pubkey],
                create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        let sent = whitenoise
            .send_message_to_group(&creator, &group.mls_group_id, "Hello".to_string(), 9, None)
            .await
            .unwrap();

        let message_id = sent.message.id.to_hex();

        let result = whitenoise
            .fetch_message_by_id(&creator.pubkey, &group.mls_group_id, &message_id)
            .await
            .unwrap();

        assert!(result.is_some(), "Expected Some for an existing message");
        assert_eq!(result.unwrap().id, message_id);
    }

    /// fetch_message_by_id returns None for an ID that does not exist in the cache.
    #[tokio::test]
    async fn test_fetch_message_by_id_returns_none_for_unknown_id() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator,
                vec![member_pubkey],
                create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        let nonexistent_id = format!("{:0>64}", "deadbeef");

        let result = whitenoise
            .fetch_message_by_id(&creator.pubkey, &group.mls_group_id, &nonexistent_id)
            .await
            .unwrap();

        assert!(result.is_none(), "Expected None for an unknown message ID");
    }

    /// fetch_messages_unread_with_minimum returns all unread messages when unread count
    /// exceeds the minimum — the caller gets every message since the read marker.
    #[tokio::test]
    async fn test_fetch_messages_unread_with_minimum_returns_all_unreads() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator,
                vec![member_pubkey],
                create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        // Send 3 messages spaced 1 second apart.  Nostr Timestamp is second-precision, so
        // messages sent within the same second share the same `created_at`.  The unread count
        // uses `created_at > marker_created_at` (strict greater-than), so all messages must
        // have distinct timestamps for the boundary to work correctly.
        let mut sent_ids = Vec::new();
        for i in 0..3 {
            let msg = whitenoise
                .send_message_to_group(
                    &creator,
                    &group.mls_group_id,
                    format!("message {i}"),
                    9,
                    None,
                )
                .await
                .unwrap();
            sent_ids.push(msg.message.id);
            if i < 2 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }

        // Mark the first message as read — 2 messages remain unread
        whitenoise
            .mark_message_read(&creator, &sent_ids[0])
            .await
            .unwrap();

        // minimum=1, but 2 messages are unread — unread count wins, expect 2
        let result = whitenoise
            .fetch_messages_unread_with_minimum(&creator.pubkey, &group.mls_group_id, Some(1))
            .await
            .unwrap();

        assert_eq!(
            result.len(),
            2,
            "Expected 2 unread messages, got {}",
            result.len()
        );
    }

    /// fetch_messages_unread_with_minimum applies the minimum floor when fewer messages
    /// are unread — ensures the caller always gets at least a full page.
    #[tokio::test]
    async fn test_fetch_messages_unread_with_minimum_applies_floor() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator,
                vec![member_pubkey],
                create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        // Send 5 messages, mark all as read
        let mut last_id = None;
        for i in 0..5 {
            let msg = whitenoise
                .send_message_to_group(
                    &creator,
                    &group.mls_group_id,
                    format!("message {i}"),
                    9,
                    None,
                )
                .await
                .unwrap();
            last_id = Some(msg.message.id);
        }
        whitenoise
            .mark_message_read(&creator, &last_id.unwrap())
            .await
            .unwrap();

        // 0 unread, minimum=3 — floor should kick in, returning 3 newest messages
        let result = whitenoise
            .fetch_messages_unread_with_minimum(&creator.pubkey, &group.mls_group_id, Some(3))
            .await
            .unwrap();

        assert_eq!(
            result.len(),
            3,
            "Expected minimum of 3 messages (floor), got {}",
            result.len()
        );
    }

    /// fetch_messages_unread_with_minimum with no read marker treats every message
    /// as unread, so all messages are returned (all unread > minimum=3).
    #[tokio::test]
    async fn test_fetch_messages_unread_with_minimum_no_read_marker() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator,
                vec![member_pubkey],
                create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        // Send 4 messages, never mark any as read
        for i in 0..4 {
            whitenoise
                .send_message_to_group(
                    &creator,
                    &group.mls_group_id,
                    format!("message {i}"),
                    9,
                    None,
                )
                .await
                .unwrap();
        }

        // No read marker: all 4 are unread, minimum=3 — unread wins
        let result = whitenoise
            .fetch_messages_unread_with_minimum(&creator.pubkey, &group.mls_group_id, Some(3))
            .await
            .unwrap();

        assert_eq!(
            result.len(),
            4,
            "Expected all 4 messages (all unread), got {}",
            result.len()
        );
    }

    /// fetch_messages_unread_with_minimum rejects an unknown account (security check).
    #[tokio::test]
    async fn test_fetch_messages_unread_with_minimum_rejects_unknown_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator,
                vec![members[0].0.pubkey],
                create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        let stranger = Keys::generate().public_key();
        let result = whitenoise
            .fetch_messages_unread_with_minimum(&stranger, &group.mls_group_id, None)
            .await;

        assert!(matches!(
            result.unwrap_err(),
            WhitenoiseError::AccountNotFound
        ));
    }

    /// fetch_messages_unread_with_minimum with `None` minimum uses the default floor
    /// of 50 — if fewer messages exist, all are returned.
    #[tokio::test]
    async fn test_fetch_messages_unread_with_minimum_none_uses_default_50() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator,
                vec![member_pubkey],
                create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        // Send 60 messages and mark all as read — 0 unread
        let mut last_id = None;
        for i in 0..60 {
            let msg = whitenoise
                .send_message_to_group(
                    &creator,
                    &group.mls_group_id,
                    format!("message {i}"),
                    9,
                    None,
                )
                .await
                .unwrap();
            last_id = Some(msg.message.id);
        }
        whitenoise
            .mark_message_read(&creator, &last_id.unwrap())
            .await
            .unwrap();

        // None minimum → default 50. All 60 are read, so unread=0, floor=50 → exactly 50 returned.
        let result = whitenoise
            .fetch_messages_unread_with_minimum(&creator.pubkey, &group.mls_group_id, None)
            .await
            .unwrap();

        assert_eq!(
            result.len(),
            50,
            "Expected exactly 50 messages (default floor), got {}",
            result.len()
        );
    }

    /// fetch_message_by_id enforces the account-existence security check: an unregistered
    /// public key is rejected before any database lookup is attempted.
    #[tokio::test]
    async fn test_fetch_message_by_id_rejects_unknown_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator,
                vec![member_pubkey],
                create_nostr_group_config_data(vec![creator.pubkey]),
                None,
            )
            .await
            .unwrap();

        let sent = whitenoise
            .send_message_to_group(&creator, &group.mls_group_id, "Hello".to_string(), 9, None)
            .await
            .unwrap();

        let message_id = sent.message.id.to_hex();

        // Use a random key that was never registered with this Whitenoise instance.
        let stranger_pubkey = Keys::generate().public_key();

        let result = whitenoise
            .fetch_message_by_id(&stranger_pubkey, &group.mls_group_id, &message_id)
            .await;

        assert!(
            result.is_err(),
            "Expected error when account does not exist"
        );
        assert!(
            matches!(result.unwrap_err(), WhitenoiseError::AccountNotFound),
            "Expected AccountNotFound for an unregistered public key"
        );
    }
}
