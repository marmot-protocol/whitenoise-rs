use mdk_core::prelude::message_types::Message;
use mdk_core::prelude::{GroupId, MessageProcessingResult};
use nostr_sdk::prelude::*;

use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    aggregated_message::AggregatedMessage,
    chat_list_streaming::ChatListUpdateTrigger,
    error::{Result, WhitenoiseError},
    media_files::MediaFile,
    message_aggregator::{ChatMessage, emoji_utils, reaction_handler},
    message_streaming::{MessageUpdate, UpdateTrigger},
};

impl Whitenoise {
    pub async fn handle_mls_message(&self, account: &Account, event: Event) -> Result<()> {
        tracing::debug!(
          target: "whitenoise::event_handlers::handle_mls_message",
          "Handling MLS message for account: {}",
          account.pubkey.to_hex()
        );

        let mdk = self.create_mdk_for_account(account.pubkey)?;
        match mdk.process_message(&event) {
            Ok(result) => {
                tracing::debug!(
                  target: "whitenoise::event_handlers::handle_mls_message",
                  "Handled MLS message - Result: {:?}",
                  result
                );

                // Extract and store media references synchronously
                if let Some((group_id, inner_event, message)) =
                    Self::extract_message_details(&result)
                {
                    let parsed_references = {
                        let media_manager = mdk.media_manager(group_id.clone());
                        self.media_files()
                            .parse_imeta_tags_from_event(&inner_event, &media_manager)?
                    };

                    self.media_files()
                        .store_parsed_media_references(
                            &group_id,
                            &account.pubkey,
                            parsed_references,
                        )
                        .await?;

                    match message.kind {
                        Kind::ChatMessage => {
                            let msg = self.cache_chat_message(&group_id, &message).await?;
                            let group_name =
                                mdk.get_group(&group_id).ok().flatten().map(|g| g.name);
                            Whitenoise::spawn_new_message_notification_if_enabled(
                                account, &group_id, &msg, group_name,
                            );
                            self.emit_message_update(&group_id, UpdateTrigger::NewMessage, msg);
                            self.emit_chat_list_update(
                                account,
                                &group_id,
                                ChatListUpdateTrigger::NewLastMessage,
                            )
                            .await;
                        }
                        Kind::Reaction => {
                            if let Some(target) = self.cache_reaction(&group_id, &message).await? {
                                self.emit_message_update(
                                    &group_id,
                                    UpdateTrigger::ReactionAdded,
                                    target,
                                );
                            }
                        }
                        Kind::EventDeletion => {
                            let last_message_id = self.get_last_message_id(&group_id).await;

                            for (trigger, msg) in self.cache_deletion(&group_id, &message).await? {
                                self.emit_message_update(&group_id, trigger, msg);
                            }

                            // Check if the deleted message was the last message.
                            // This check must happen AFTER get_last_message_id but the
                            // result is only valid for the FIRST handler (before cache_deletion
                            // modifies shared state). We emit for ALL subscribed accounts because
                            // subsequent handlers will see incorrect post-deletion state.
                            if let Some(last_message_id) = last_message_id {
                                let deleted_ids = Self::extract_deletion_target_ids(&message.tags);
                                if deleted_ids.contains(&last_message_id) {
                                    self.emit_chat_list_update_for_group(
                                        &group_id,
                                        ChatListUpdateTrigger::LastMessageDeleted,
                                    )
                                    .await;
                                }
                            }
                        }
                        _ => {
                            tracing::debug!("Ignoring message kind {:?} for cache", message.kind);
                        }
                    }
                }

                // Handle auto-committed proposals (e.g., admin auto-commits a
                // member's self-removal): publish the resulting commit event so
                // other group members learn about the change, then merge the
                // pending commit into our local MLS state.
                if let MessageProcessingResult::Proposal(ref update_result) = result {
                    let group_id = &update_result.mls_group_id;
                    let relay_urls = Self::ensure_group_relays(&mdk, group_id)?;

                    mdk.merge_pending_commit(group_id)?;

                    self.nostr
                        .publish_event_to(
                            update_result.evolution_event.clone(),
                            &account.pubkey,
                            &relay_urls,
                        )
                        .await?;

                    if let Some(welcome_rumors) = &update_result.welcome_rumors {
                        tracing::warn!(
                            target: "whitenoise::event_handlers::handle_mls_message",
                            "Auto-committed proposal produced {} welcome rumors \
                             that were not delivered",
                            welcome_rumors.len()
                        );
                    }

                    tracing::info!(
                        target: "whitenoise::event_handlers::handle_mls_message",
                        "Published auto-committed proposal evolution event \
                         for group {}",
                        hex::encode(group_id.as_slice())
                    );

                    self.emit_chat_list_update(
                        account,
                        group_id,
                        ChatListUpdateTrigger::NewLastMessage,
                    )
                    .await;
                }

                // Background sync for group images after commits or
                // auto-committed proposals (group state may have changed).
                match result {
                    MessageProcessingResult::Commit { ref mls_group_id } => {
                        Self::background_sync_group_image_cache_if_needed(account, mls_group_id);
                    }
                    MessageProcessingResult::Proposal(ref update_result) => {
                        Self::background_sync_group_image_cache_if_needed(
                            account,
                            &update_result.mls_group_id,
                        );
                    }
                    _ => {}
                }
                Ok(())
            }
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::event_handlers::handle_mls_message",
                    "MLS message handling failed for account {}: {}",
                    account.pubkey.to_hex(),
                    e
                );
                Err(WhitenoiseError::MdkCoreError(e))
            }
        }
    }

    /// Extracts group_id, inner_event, and the full Message from a processing result.
    ///
    /// Returns Some if the result contains an application message with inner event content,
    /// None otherwise (e.g., for commits, proposals, or other non-message results).
    /// The returned Message preserves all MDK-set fields (processed_at, epoch, state, etc.).
    fn extract_message_details(
        result: &MessageProcessingResult,
    ) -> Option<(mdk_core::prelude::GroupId, UnsignedEvent, Message)> {
        match result {
            MessageProcessingResult::ApplicationMessage(message) => {
                // The message.event is the decrypted rumor (UnsignedEvent) from the MLS message
                Some((
                    message.mls_group_id.clone(),
                    message.event.clone(),
                    message.clone(),
                ))
            }
            _ => None,
        }
    }

    /// Emit a message update to all subscribers of a group.
    fn emit_message_update(
        &self,
        group_id: &GroupId,
        trigger: UpdateTrigger,
        message: ChatMessage,
    ) {
        self.message_stream_manager
            .emit(group_id, MessageUpdate { trigger, message });
    }

    /// Gets the ID of the last message in a group (if any).
    async fn get_last_message_id(&self, group_id: &GroupId) -> Option<String> {
        AggregatedMessage::find_last_by_group_ids(std::slice::from_ref(group_id), &self.database)
            .await
            .ok()
            .and_then(|v| v.into_iter().next())
            .map(|s| s.message_id.to_hex())
    }

    /// Cache a new chat message and return it for emission.
    ///
    /// Processes the message through the aggregator, inserts into database,
    /// and applies any orphaned reactions/deletions that arrived before this message.
    async fn cache_chat_message(
        &self,
        group_id: &GroupId,
        message: &Message,
    ) -> Result<ChatMessage> {
        let media_files = MediaFile::find_by_group(&self.database, group_id).await?;

        let chat_message = self
            .message_aggregator
            .process_single_message(message, &self.nostr, media_files)
            .await?;

        AggregatedMessage::insert_message(&chat_message, group_id, &self.database).await?;

        // Apply orphaned reactions/deletions - modifies in-place and returns final state
        let final_message = self
            .apply_orphaned_reactions_and_deletions(chat_message, group_id)
            .await?;

        tracing::debug!(
            target: "whitenoise::cache",
            "Cached ChatMessage {} in group {}",
            message.id,
            hex::encode(group_id.as_slice())
        );

        Ok(final_message)
    }

    /// Cache a reaction and return the updated target message for emission.
    ///
    /// Returns `Ok(None)` if the target message isn't cached yet (orphaned reaction).
    /// Propagates real errors (malformed tags, invalid emoji, DB failures).
    async fn cache_reaction(
        &self,
        group_id: &GroupId,
        message: &Message,
    ) -> Result<Option<ChatMessage>> {
        AggregatedMessage::insert_reaction(message, group_id, &self.database).await?;

        let result = self.apply_reaction_to_target(message, group_id).await?;

        if result.is_none() {
            tracing::debug!(
                target: "whitenoise::cache",
                "Reaction {} orphaned (target not yet cached)",
                message.id,
            );
        }

        tracing::debug!(
            target: "whitenoise::cache",
            "Cached kind 7 reaction {} in group {}",
            message.id,
            hex::encode(group_id.as_slice())
        );

        Ok(result)
    }

    /// Apply a reaction to its target message, returning the updated target.
    ///
    /// Returns `Ok(None)` if the target message isn't cached yet (true orphan case).
    /// Returns `Err` for real failures (malformed tags, invalid emoji, DB errors).
    async fn apply_reaction_to_target(
        &self,
        reaction: &Message,
        group_id: &GroupId,
    ) -> Result<Option<ChatMessage>> {
        let target_id = Self::extract_reaction_target_id(&reaction.tags)?;

        let Some(mut target) =
            AggregatedMessage::find_by_id(&target_id, group_id, &self.database).await?
        else {
            return Ok(None); // True orphan: target not yet cached
        };

        let emoji = emoji_utils::validate_and_normalize_reaction(
            &reaction.content,
            self.message_aggregator.config().normalize_emoji,
        )?;

        reaction_handler::add_reaction_to_message(
            &mut target,
            &reaction.pubkey,
            &emoji,
            reaction.created_at,
            reaction.id,
        );

        AggregatedMessage::update_reactions(
            &target.id,
            group_id,
            &target.reactions,
            &self.database,
        )
        .await?;

        Ok(Some(target))
    }

    /// Cache a deletion and return updates for all affected messages.
    ///
    /// A single deletion can target multiple events (reactions and/or messages),
    /// so this returns a Vec of (trigger, message) pairs.
    async fn cache_deletion(
        &self,
        group_id: &GroupId,
        message: &Message,
    ) -> Result<Vec<(UpdateTrigger, ChatMessage)>> {
        AggregatedMessage::insert_deletion(message, group_id, &self.database).await?;

        let updates = self.apply_deletions_to_targets(message, group_id).await?;

        tracing::debug!(
            target: "whitenoise::cache",
            "Cached kind 5 deletion {} in group {} ({} targets affected)",
            message.id,
            hex::encode(group_id.as_slice()),
            updates.len()
        );

        Ok(updates)
    }

    /// Apply deletion to all targets and collect updates to emit.
    async fn apply_deletions_to_targets(
        &self,
        deletion: &Message,
        group_id: &GroupId,
    ) -> Result<Vec<(UpdateTrigger, ChatMessage)>> {
        let target_ids = Self::extract_deletion_target_ids(&deletion.tags);
        let mut updates = Vec::with_capacity(target_ids.len());

        for target_id in target_ids {
            if let Some(update) = self
                .apply_single_deletion(&target_id, &deletion.id, group_id)
                .await?
            {
                updates.push(update);
            }
        }

        Ok(updates)
    }

    /// Apply deletion to a single target, returning the appropriate update.
    async fn apply_single_deletion(
        &self,
        target_id: &str,
        deletion_event_id: &EventId,
        group_id: &GroupId,
    ) -> Result<Option<(UpdateTrigger, ChatMessage)>> {
        // Check if target is a reaction
        if let Some(reaction) =
            AggregatedMessage::find_reaction_by_id(target_id, group_id, &self.database).await?
        {
            let parent_update = self
                .remove_reaction_from_parent(&reaction, group_id)
                .await?;
            AggregatedMessage::mark_deleted(
                target_id,
                group_id,
                &deletion_event_id.to_string(),
                &self.database,
            )
            .await?;
            return Ok(parent_update.map(|msg| (UpdateTrigger::ReactionRemoved, msg)));
        }

        // Check if target is a message
        if let Some(mut msg) =
            AggregatedMessage::find_by_id(target_id, group_id, &self.database).await?
        {
            msg.is_deleted = true;
            AggregatedMessage::mark_deleted(
                target_id,
                group_id,
                &deletion_event_id.to_string(),
                &self.database,
            )
            .await?;
            return Ok(Some((UpdateTrigger::MessageDeleted, msg)));
        }

        // Unknown target - still mark for audit trail (orphaned deletion)
        AggregatedMessage::mark_deleted(
            target_id,
            group_id,
            &deletion_event_id.to_string(),
            &self.database,
        )
        .await?;
        Ok(None)
    }

    /// Remove a reaction from its parent message and return the updated parent.
    async fn remove_reaction_from_parent(
        &self,
        reaction: &AggregatedMessage,
        group_id: &GroupId,
    ) -> Result<Option<ChatMessage>> {
        let Ok(parent_id) = Self::extract_reaction_target_id(&reaction.tags) else {
            return Ok(None);
        };

        let Some(mut parent) =
            AggregatedMessage::find_by_id(&parent_id, group_id, &self.database).await?
        else {
            return Ok(None);
        };

        if reaction_handler::remove_reaction_from_message(&mut parent, &reaction.author) {
            AggregatedMessage::update_reactions(
                &parent_id,
                group_id,
                &parent.reactions,
                &self.database,
            )
            .await?;

            tracing::debug!(
                target: "whitenoise::cache",
                "Removed reaction {} from message {}",
                reaction.event_id,
                parent_id
            );

            Ok(Some(parent))
        } else {
            Ok(None)
        }
    }

    fn extract_reaction_target_id(tags: &Tags) -> Result<String> {
        tags.iter()
            .find(|tag| tag.kind() == nostr_sdk::TagKind::e())
            .and_then(|tag| tag.content().map(|s| s.to_string()))
            .ok_or_else(|| WhitenoiseError::Other(anyhow::anyhow!("Reaction missing e-tag")))
    }

    fn extract_deletion_target_ids(tags: &Tags) -> Vec<String> {
        tags.iter()
            .filter(|tag| tag.kind() == nostr_sdk::TagKind::e())
            .filter_map(|tag| tag.content().map(|s| s.to_string()))
            .collect()
    }

    /// Apply any orphaned reactions/deletions to a newly cached message.
    ///
    /// Takes ownership of the message, modifies in-place, and returns the final state.
    /// This avoids re-fetching from the database after applying orphans.
    async fn apply_orphaned_reactions_and_deletions(
        &self,
        mut message: ChatMessage,
        group_id: &GroupId,
    ) -> Result<ChatMessage> {
        let orphaned_reactions =
            AggregatedMessage::find_orphaned_reactions(&message.id, group_id, &self.database)
                .await?;

        let orphaned_deletions =
            AggregatedMessage::find_orphaned_deletions(&message.id, group_id, &self.database)
                .await?;

        if !orphaned_reactions.is_empty() || !orphaned_deletions.is_empty() {
            tracing::info!(
                target: "whitenoise::cache",
                "Found {} orphaned reactions and {} orphaned deletions for message {}, applying...",
                orphaned_reactions.len(),
                orphaned_deletions.len(),
                message.id
            );
        }

        // Apply orphaned reactions in-memory and persist each
        for reaction in orphaned_reactions {
            let reaction_emoji = match emoji_utils::validate_and_normalize_reaction(
                &reaction.content,
                self.message_aggregator.config().normalize_emoji,
            ) {
                Ok(emoji) => emoji,
                Err(e) => {
                    tracing::debug!(
                        target: "whitenoise::cache",
                        "Skipping orphaned reaction {} from {} with invalid content '{}': {}",
                        reaction.event_id,
                        reaction.author,
                        reaction.content,
                        e
                    );
                    continue;
                }
            };

            let reaction_timestamp = Timestamp::from(reaction.created_at.timestamp() as u64);
            reaction_handler::add_reaction_to_message(
                &mut message,
                &reaction.author,
                &reaction_emoji,
                reaction_timestamp,
                reaction.event_id,
            );

            AggregatedMessage::update_reactions(
                &message.id,
                group_id,
                &message.reactions,
                &self.database,
            )
            .await?;
        }

        // Apply orphaned deletions
        for deletion_event_id in orphaned_deletions {
            message.is_deleted = true;
            AggregatedMessage::mark_deleted(
                &message.id,
                group_id,
                &deletion_event_id.to_string(),
                &self.database,
            )
            .await?;
        }

        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::whitenoise::{aggregated_message::AggregatedMessage, relays::Relay, test_utils::*};

    /// Test handling of different MLS message types: regular messages, reactions, and deletions
    #[tokio::test]
    async fn test_handle_mls_message_different_types() {
        // Arrange: Setup whitenoise and create a group
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator_account,
                vec![member_pubkey],
                create_nostr_group_config_data(vec![creator_account.pubkey]),
                None,
            )
            .await
            .unwrap();

        let mdk = whitenoise
            .create_mdk_for_account(creator_account.pubkey)
            .unwrap();
        let group_id = &group.mls_group_id;

        // Test 1: Regular message (Kind 9)
        let mut inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Test message".to_string(),
        );
        inner.ensure_id();
        let message_id = inner.id.unwrap();
        let message_event = mdk.create_message(group_id, inner).unwrap();

        let result = whitenoise
            .handle_mls_message(&creator_account, message_event)
            .await;
        assert!(result.is_ok(), "Failed to handle regular message");

        // Verify message was cached
        let cached_msg =
            AggregatedMessage::find_by_id(&message_id.to_string(), group_id, &whitenoise.database)
                .await
                .unwrap();
        assert!(cached_msg.is_some(), "Message should be cached");

        // Test 2: Reaction message (Kind 7)
        let mut reaction_inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Reaction,
            vec![Tag::parse(vec!["e", &message_id.to_string()]).unwrap()],
            "👍".to_string(),
        );
        reaction_inner.ensure_id();
        let reaction_event = mdk.create_message(group_id, reaction_inner).unwrap();

        let result = whitenoise
            .handle_mls_message(&creator_account, reaction_event)
            .await;
        assert!(result.is_ok(), "Failed to handle reaction");

        // Verify reaction was applied to cached message
        let cached_msg =
            AggregatedMessage::find_by_id(&message_id.to_string(), group_id, &whitenoise.database)
                .await
                .unwrap()
                .unwrap();
        assert!(
            !cached_msg.reactions.by_emoji.is_empty(),
            "Reaction should be applied"
        );

        // Test 3: Deletion message (Kind 5)
        let mut deletion_inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::EventDeletion,
            vec![Tag::parse(vec!["e", &message_id.to_string()]).unwrap()],
            String::new(),
        );
        deletion_inner.ensure_id();
        let deletion_event = mdk.create_message(group_id, deletion_inner).unwrap();

        let result = whitenoise
            .handle_mls_message(&creator_account, deletion_event)
            .await;
        assert!(result.is_ok(), "Failed to handle deletion");

        // Verify message was marked as deleted
        let cached_msg =
            AggregatedMessage::find_by_id(&message_id.to_string(), group_id, &whitenoise.database)
                .await
                .unwrap()
                .unwrap();
        assert!(cached_msg.is_deleted, "Message should be marked as deleted");
    }

    /// Test error handling for invalid MLS messages
    #[tokio::test]
    async fn test_handle_mls_message_error_handling() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator_account,
                vec![member_pubkey],
                create_nostr_group_config_data(vec![creator_account.pubkey]),
                None,
            )
            .await
            .unwrap();

        let mdk = whitenoise
            .create_mdk_for_account(creator_account.pubkey)
            .unwrap();
        let mut inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Valid message".to_string(),
        );
        inner.ensure_id();
        let valid_event = mdk.create_message(&group.mls_group_id, inner).unwrap();

        // Corrupt the event by changing its kind (MLS processing should fail)
        let mut bad_event = valid_event;
        bad_event.kind = Kind::TextNote;

        let result = whitenoise
            .handle_mls_message(&creator_account, bad_event)
            .await;

        assert!(result.is_err(), "Expected error for corrupted event");
        match result.err().unwrap() {
            WhitenoiseError::MdkCoreError(_) => {}
            other => panic!("Expected MdkCoreError, got: {:?}", other),
        }
    }

    /// Test orphaned reactions and deletions are applied when target message arrives later
    #[tokio::test]
    async fn test_handle_mls_message_orphaned_reactions_and_deletions() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator_account,
                vec![member_pubkey],
                create_nostr_group_config_data(vec![creator_account.pubkey]),
                None,
            )
            .await
            .unwrap();

        let mdk = whitenoise
            .create_mdk_for_account(creator_account.pubkey)
            .unwrap();
        let group_id = &group.mls_group_id;

        // Create a message ID that doesn't exist yet (simulating out-of-order delivery)
        let future_message_id = EventId::all_zeros();

        // Send reaction to non-existent message (orphaned reaction)
        let mut orphaned_reaction = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Reaction,
            vec![Tag::parse(vec!["e", &future_message_id.to_string()]).unwrap()],
            "+".to_string(), // Use simple emoji that won't be normalized
        );
        orphaned_reaction.ensure_id();
        let reaction_event = mdk.create_message(group_id, orphaned_reaction).unwrap();

        let result = whitenoise
            .handle_mls_message(&creator_account, reaction_event)
            .await;
        assert!(result.is_ok(), "Orphaned reaction should be stored");

        // Verify orphaned reaction is stored
        let orphaned_reactions = AggregatedMessage::find_orphaned_reactions(
            &future_message_id.to_string(),
            group_id,
            &whitenoise.database,
        )
        .await
        .unwrap();
        assert_eq!(
            orphaned_reactions.len(),
            1,
            "Should have one orphaned reaction"
        );

        // Now send the actual message with the matching ID
        let mut actual_message = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Late message".to_string(),
        );
        actual_message.id = Some(future_message_id);
        let message_event = mdk.create_message(group_id, actual_message).unwrap();

        let result = whitenoise
            .handle_mls_message(&creator_account, message_event)
            .await;
        assert!(
            result.is_ok(),
            "Message with orphaned reaction should succeed"
        );

        // Verify the orphaned reaction was applied
        let cached_msg = AggregatedMessage::find_by_id(
            &future_message_id.to_string(),
            group_id,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();

        assert!(
            !cached_msg.reactions.by_emoji.is_empty(),
            "Orphaned reaction should be applied to message"
        );
        // Verify total reaction count instead of specific emoji (due to normalization)
        let total_reactions: usize = cached_msg
            .reactions
            .by_emoji
            .values()
            .map(|v| v.count)
            .sum();
        assert_eq!(total_reactions, 1, "Should have one reaction applied");
    }

    /// Test that invalid orphaned reactions are skipped gracefully without failing the entire method
    #[tokio::test]
    async fn test_invalid_orphaned_reactions_are_skipped() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator_account,
                vec![member_pubkey],
                create_nostr_group_config_data(vec![creator_account.pubkey]),
                None,
            )
            .await
            .unwrap();

        let mdk = whitenoise
            .create_mdk_for_account(creator_account.pubkey)
            .unwrap();
        let group_id = &group.mls_group_id;

        let future_message_id = EventId::all_zeros();

        // Send a VALID orphaned reaction
        let mut valid_reaction = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Reaction,
            vec![Tag::parse(vec!["e", &future_message_id.to_string()]).unwrap()],
            "👍".to_string(),
        );
        valid_reaction.ensure_id();
        let valid_event = mdk.create_message(group_id, valid_reaction).unwrap();

        whitenoise
            .handle_mls_message(&creator_account, valid_event)
            .await
            .unwrap();

        // Send an INVALID orphaned reaction (empty content - not a valid emoji)
        let mut invalid_reaction = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Reaction,
            vec![Tag::parse(vec!["e", &future_message_id.to_string()]).unwrap()],
            "".to_string(), // Empty content is invalid
        );
        invalid_reaction.ensure_id();
        let invalid_event = mdk.create_message(group_id, invalid_reaction).unwrap();

        whitenoise
            .handle_mls_message(&creator_account, invalid_event)
            .await
            .unwrap();

        // Now send the target message - this should succeed despite invalid orphaned reaction
        let mut actual_message = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Target message".to_string(),
        );
        actual_message.id = Some(future_message_id);
        let message_event = mdk.create_message(group_id, actual_message).unwrap();

        let result = whitenoise
            .handle_mls_message(&creator_account, message_event)
            .await;

        // The critical assertion: message processing should succeed
        assert!(
            result.is_ok(),
            "Message processing should succeed despite invalid orphaned reaction"
        );

        // Verify only the valid reaction was applied
        let cached_msg = AggregatedMessage::find_by_id(
            &future_message_id.to_string(),
            group_id,
            &whitenoise.database,
        )
        .await
        .unwrap()
        .unwrap();

        let total_reactions: usize = cached_msg
            .reactions
            .by_emoji
            .values()
            .map(|v| v.count)
            .sum();
        assert_eq!(
            total_reactions, 1,
            "Should have exactly one valid reaction applied (invalid one skipped)"
        );
    }

    /// Test helper methods: extract_message_details, extract_reaction_target_id, etc.
    #[tokio::test]
    async fn test_helper_methods() {
        let pubkey = nostr_sdk::Keys::generate().public_key();
        let group_id = GroupId::from_slice(&[1; 32]);

        // Test extract_message_details with ApplicationMessage
        let mut inner_event = UnsignedEvent::new(
            pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Test".to_string(),
        );
        inner_event.ensure_id();

        let message = mdk_core::prelude::message_types::Message {
            id: inner_event.id.unwrap(),
            pubkey,
            created_at: Timestamp::now(),
            processed_at: Timestamp::now(),
            kind: Kind::Custom(9),
            tags: Tags::new(),
            content: "Test".to_string(),
            mls_group_id: group_id.clone(),
            event: inner_event.clone(),
            wrapper_event_id: EventId::all_zeros(),
            epoch: None, // Epoch not needed for test message
            state: mdk_core::prelude::message_types::MessageState::Processed,
        };

        let result = MessageProcessingResult::ApplicationMessage(message);
        let extracted = Whitenoise::extract_message_details(&result);
        assert!(extracted.is_some(), "Should extract application message");
        let (extracted_group_id, extracted_event, extracted_message) = extracted.unwrap();
        assert_eq!(extracted_group_id, group_id);
        assert_eq!(extracted_event.content, "Test");
        assert_eq!(extracted_message.content, "Test");
        assert_eq!(extracted_message.mls_group_id, group_id);

        // Test extract_message_details with non-ApplicationMessage
        let commit_result = MessageProcessingResult::Commit {
            mls_group_id: group_id,
        };
        let extracted = Whitenoise::extract_message_details(&commit_result);
        assert!(extracted.is_none(), "Should not extract commit");

        // Test extract_reaction_target_id
        let mut tags = Tags::new();
        tags.push(Tag::parse(vec!["e", "test_event_id"]).unwrap());
        let target_id = Whitenoise::extract_reaction_target_id(&tags).unwrap();
        assert_eq!(target_id, "test_event_id");

        // Test extract_reaction_target_id with missing e-tag
        let empty_tags = Tags::new();
        let result = Whitenoise::extract_reaction_target_id(&empty_tags);
        assert!(result.is_err(), "Should fail with missing e-tag");

        // Test extract_deletion_target_ids with multiple targets
        let mut tags = Tags::new();
        tags.push(Tag::parse(vec!["e", "id1"]).unwrap());
        tags.push(Tag::parse(vec!["e", "id2"]).unwrap());
        tags.push(Tag::parse(vec!["p", "some_pubkey"]).unwrap()); // Should be ignored

        let target_ids = Whitenoise::extract_deletion_target_ids(&tags);
        assert_eq!(target_ids.len(), 2);
        assert!(target_ids.contains(&"id1".to_string()));
        assert!(target_ids.contains(&"id2".to_string()));
    }

    /// Test message cache integration with real message flow
    #[tokio::test]
    async fn test_handle_mls_message_cache_integration() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group = whitenoise
            .create_group(
                &creator_account,
                vec![members[0].0.pubkey],
                create_nostr_group_config_data(vec![creator_account.pubkey]),
                None,
            )
            .await
            .unwrap();

        let mdk = whitenoise
            .create_mdk_for_account(creator_account.pubkey)
            .unwrap();

        // Send multiple messages
        for i in 1..=3 {
            let mut inner = UnsignedEvent::new(
                creator_account.pubkey,
                Timestamp::now(),
                Kind::Custom(9),
                vec![],
                format!("Message {}", i),
            );
            inner.ensure_id();
            let event = mdk.create_message(&group.mls_group_id, inner).unwrap();

            whitenoise
                .handle_mls_message(&creator_account, event)
                .await
                .unwrap();
        }

        // Verify all messages are in cache
        let messages =
            AggregatedMessage::find_messages_by_group(&group.mls_group_id, &whitenoise.database)
                .await
                .unwrap();

        assert_eq!(messages.len(), 3, "All messages should be cached");
        for (i, msg) in messages.iter().enumerate() {
            assert!(
                msg.content.contains(&format!("Message {}", i + 1)),
                "Message {} content should be correct",
                i + 1
            );
        }

        // Verify messages are accessible via public API
        let fetched = whitenoise
            .fetch_aggregated_messages_for_group(&creator_account.pubkey, &group.mls_group_id)
            .await
            .unwrap();
        assert_eq!(fetched.len(), 3, "Should fetch all cached messages");
    }

    /// Helper: create a group via MDK directly and have a member join via
    /// welcome giftwrap, returning the group ID. This gives both the admin's
    /// and member's MDK instances full MLS state for the group.
    async fn setup_two_member_group(
        whitenoise: &Whitenoise,
        admin_account: &Account,
        member_account: &Account,
    ) -> GroupId {
        // Fetch the member's key package from relays
        let relay_urls = Relay::urls(&admin_account.key_package_relays(whitenoise).await.unwrap());
        let key_pkg_event = whitenoise
            .nostr
            .fetch_user_key_package(member_account.pubkey, &relay_urls)
            .await
            .unwrap()
            .expect("member must have a published key package");

        // Create group via MDK directly so we get the welcome rumor
        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let create_result = admin_mdk
            .create_group(
                &admin_account.pubkey,
                vec![key_pkg_event],
                create_nostr_group_config_data(vec![admin_account.pubkey]),
            )
            .unwrap();

        let group_id = create_result.group.mls_group_id.clone();
        let welcome_rumor = create_result
            .welcome_rumors
            .first()
            .expect("welcome rumor exists")
            .clone();

        // Build giftwrap so the member can process the welcome
        let admin_signer = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&admin_account.pubkey)
            .unwrap();
        let giftwrap =
            EventBuilder::gift_wrap(&admin_signer, &member_account.pubkey, welcome_rumor, vec![])
                .await
                .unwrap();

        // Member processes the welcome -- now their MDK has the group
        whitenoise
            .handle_giftwrap(member_account, giftwrap)
            .await
            .expect("member should process welcome successfully");

        group_id
    }

    /// Test that auto-committed proposals (e.g., admin auto-commits a
    /// member's self-removal) are published and merged successfully.
    #[tokio::test]
    async fn test_handle_mls_message_auto_committed_proposal() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Set up a group where both admin and member have full MLS state
        let group_id = setup_two_member_group(&whitenoise, &admin_account, &member_account).await;

        // Member creates a self-removal proposal (leave)
        let member_mdk = whitenoise
            .create_mdk_for_account(member_account.pubkey)
            .unwrap();
        let leave_result = member_mdk.leave_group(&group_id).unwrap();

        // Admin processes the leave proposal -- should auto-commit because
        // admin_account is the group admin
        let result = whitenoise
            .handle_mls_message(&admin_account, leave_result.evolution_event)
            .await;
        assert!(
            result.is_ok(),
            "Admin should successfully auto-commit the leave proposal: {:?}",
            result.err()
        );

        // Verify the admin's MLS state was updated (pending commit merged)
        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let group = admin_mdk
            .get_group(&group_id)
            .expect("should be able to get group")
            .expect("group should exist");

        // After merging the commit that removed the member, the epoch
        // should have advanced beyond 0 (the initial epoch)
        assert!(
            group.epoch > 0,
            "Group epoch should have advanced after auto-committed removal"
        );
    }

    /// Test that handle_mls_message returns Ok for non-auto-committed
    /// proposal variants (PendingProposal, IgnoredProposal, etc.) without
    /// publishing or crashing.
    #[tokio::test]
    async fn test_handle_mls_message_commit_after_auto_committed_proposal() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        tokio::time::sleep(Duration::from_millis(200)).await;

        let group_id = setup_two_member_group(&whitenoise, &admin_account, &member_account).await;

        // Member leaves
        let member_mdk = whitenoise
            .create_mdk_for_account(member_account.pubkey)
            .unwrap();
        let leave_result = member_mdk.leave_group(&group_id).unwrap();

        // Admin auto-commits the leave proposal
        whitenoise
            .handle_mls_message(&admin_account, leave_result.evolution_event)
            .await
            .expect("auto-commit should succeed");

        // After auto-commit, admin should still be able to send messages
        // to the group (verifies the MLS state is consistent)
        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let mut inner = UnsignedEvent::new(
            admin_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Message after member left".to_string(),
        );
        inner.ensure_id();

        let message_event = admin_mdk.create_message(&group_id, inner);
        assert!(
            message_event.is_ok(),
            "Admin should be able to create messages after auto-committed removal: {:?}",
            message_event.err()
        );
    }
}
