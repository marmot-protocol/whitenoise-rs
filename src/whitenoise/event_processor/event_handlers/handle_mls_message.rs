use std::sync::Arc;

use mdk_core::prelude::group_types::GroupState;
use mdk_core::prelude::message_types::Message;
use mdk_core::prelude::{GroupId, MessageProcessingOutcome, MessageProcessingResult};
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::prelude::*;

#[cfg(test)]
use crate::whitenoise::database::aggregated_messages::PaginationOptions;
use crate::{
    nostr_manager::parser::ContentParser,
    perf_instrument, perf_span,
    whitenoise::{
        Whitenoise,
        accounts::Account,
        accounts_groups::AccountGroup,
        aggregated_message::AggregatedMessage,
        chat_list_streaming::ChatListUpdateTrigger,
        error::{Result, WhitenoiseError},
        media_files::MediaFile,
        message_aggregator::processor::{extract_deletion_target_ids, extract_reaction_target_id},
        message_aggregator::{ChatMessage, emoji_utils, reaction_handler},
        message_streaming::{MessageUpdate, UpdateTrigger},
        push_notifications::is_push_group_message_kind,
        session::AccountSession,
    },
};
#[cfg(test)]
use crate::{
    relay_control::{RelayPlane, SubscriptionContext, SubscriptionStream},
    types::EventSource,
};

/// Extracts the group ID from a `MessageProcessingResult`, if present.
///
/// Every variant except `PreviouslyFailed` carries an `mls_group_id`.
fn extract_group_id(result: &MessageProcessingResult) -> Option<&GroupId> {
    match result {
        MessageProcessingResult::ApplicationMessage(msg) => Some(&msg.mls_group_id),
        MessageProcessingResult::Commit { mls_group_id }
        | MessageProcessingResult::PendingProposal { mls_group_id }
        | MessageProcessingResult::ExternalJoinProposal { mls_group_id }
        | MessageProcessingResult::Unprocessable { mls_group_id } => Some(mls_group_id),
        MessageProcessingResult::IgnoredProposal { mls_group_id, .. } => Some(mls_group_id),
        MessageProcessingResult::Proposal(update_result) => Some(&update_result.mls_group_id),
        MessageProcessingResult::PreviouslyFailed => None,
    }
}

impl Whitenoise {
    #[perf_instrument("event_handlers")]
    pub async fn handle_mls_message(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        event: Event,
    ) -> Result<()> {
        if session.account_pubkey != account.pubkey {
            tracing::error!(
                target: "whitenoise::event_handlers::handle_mls_message",
                session_pubkey = %session.account_pubkey.to_hex(),
                account_pubkey = %account.pubkey.to_hex(),
                "Refusing to process MLS message: session and account refer to different identities"
            );
            return Err(WhitenoiseError::AccountNotFound);
        }

        tracing::debug!(
          target: "whitenoise::event_processor::handle_mls_message",
          "Handling MLS message {} (kind {}) for account: {}",
          event.id.to_hex(),
          event.kind.as_u16(),
          account.pubkey.to_hex()
        );

        let mdk = &*session.mdk;
        let _mls_proc = perf_span!("event_handlers::mls_process_message");
        let outcome = match mdk.process_message_with_context(&event) {
            Ok(outcome) => {
                tracing::debug!(
                  target: "whitenoise::event_processor::handle_mls_message",
                  "MLS message {} processed - Result variant: {}",
                  event.id.to_hex(),
                  match &outcome.result {
                      mdk_core::prelude::MessageProcessingResult::ApplicationMessage(_) => "ApplicationMessage",
                      mdk_core::prelude::MessageProcessingResult::Commit { .. } => "Commit",
                      mdk_core::prelude::MessageProcessingResult::Proposal(_) => "Proposal",
                      mdk_core::prelude::MessageProcessingResult::PendingProposal { .. } => "PendingProposal",
                      mdk_core::prelude::MessageProcessingResult::IgnoredProposal { .. } => "IgnoredProposal",
                      mdk_core::prelude::MessageProcessingResult::ExternalJoinProposal { .. } => "ExternalJoinProposal",
                      mdk_core::prelude::MessageProcessingResult::Unprocessable { .. } => "Unprocessable",
                      mdk_core::prelude::MessageProcessingResult::PreviouslyFailed => "PreviouslyFailed",
                  }
                );
                outcome
            }
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    "MLS message handling failed for account {}: {}",
                    account.pubkey.to_hex(),
                    e
                );
                return Err(WhitenoiseError::MdkCoreError(e));
            }
        };
        drop(_mls_proc);

        // Guard: skip outcome handling if no AccountGroup exists for this group.
        // MDK has already processed the message (updating MLS state), but we
        // must not write application-level data for a group the user deleted.
        if let Some(group_id) = extract_group_id(&outcome.result) {
            let has_account_group = AccountGroup::find_by_account_and_group(
                &account.pubkey,
                group_id,
                &session.account_db.inner.pool,
            )
            .await?
            .is_some();

            if !has_account_group {
                tracing::info!(
                    target: "whitenoise::event_processor::event_handlers::handle_mls_message",
                    group_id = %hex::encode(group_id.as_slice()),
                    account = %account.pubkey.to_hex(),
                    "Skipping outcome handling: no AccountGroup exists \
                     (group may have been deleted)"
                );
                return Ok(());
            }
        }

        if let Some((group_id, inner_event, message)) = Self::extract_message_details(&outcome) {
            self.handle_application_message_outcome(
                session,
                account,
                mdk,
                &outcome,
                group_id,
                inner_event,
                message,
            )
            .await?;
        }

        self.handle_non_application_outcome(session, account, mdk, outcome.result)
            .await?;

        Ok(())
    }

    #[perf_instrument("event_handlers")]
    async fn handle_application_message_outcome(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        mdk: &mdk_core::prelude::MDK<MdkSqliteStorage>,
        outcome: &MessageProcessingOutcome,
        group_id: GroupId,
        inner_event: UnsignedEvent,
        message: Message,
    ) -> Result<()> {
        if is_push_group_message_kind(message.kind) {
            if let Err(error) = session
                .push()
                .handle_received_push_group_message(&message, outcome.context.sender_leaf_index)
                .await
            {
                tracing::warn!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    account = %account.pubkey.to_hex(),
                    group_id = %hex::encode(group_id.as_slice()),
                    sender_leaf_index = ?outcome.context.sender_leaf_index,
                    message_id = ?message.event.id.map(|event_id| event_id.to_hex()),
                    error = %error,
                    "Failed to reconcile received MIP-05 group message after MLS acceptance"
                );
            }
            return Ok(());
        }

        self.handle_standard_application_message(account, mdk, group_id, inner_event, message)
            .await
    }

    #[perf_instrument("event_handlers")]
    async fn handle_standard_application_message(
        &self,
        account: &Account,
        mdk: &mdk_core::prelude::MDK<MdkSqliteStorage>,
        group_id: GroupId,
        inner_event: UnsignedEvent,
        message: Message,
    ) -> Result<()> {
        let session = self
            .account_manager
            .get_session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        let media_files = crate::whitenoise::media_files::MediaFiles::new(
            &self.shared.storage,
            &self.shared.database,
            &session.account_db.inner.pool,
        );

        let parsed_references = {
            let media_manager = mdk.media_manager(group_id.clone());
            media_files.parse_imeta_tags_from_event(&inner_event, &media_manager)?
        };

        media_files
            .store_parsed_media_references(&group_id, &account.pubkey, parsed_references)
            .await?;

        match message.kind {
            Kind::ChatMessage => {
                let msg = self
                    .cache_chat_message(&account.pubkey, &group_id, &message)
                    .await?;
                let group_name = mdk.get_group(&group_id).ok().flatten().map(|g| g.name);
                self.spawn_new_message_notification_if_enabled(
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
                if let Some(target) = self
                    .cache_reaction(&account.pubkey, &group_id, &message)
                    .await?
                {
                    self.emit_message_update(&group_id, UpdateTrigger::ReactionAdded, target);
                }
            }
            Kind::EventDeletion => {
                self.handle_deletion_application_message(&account.pubkey, &group_id, &message)
                    .await?;
            }
            _ => {
                tracing::debug!("Ignoring message kind {:?} for cache", message.kind);
            }
        }

        Ok(())
    }

    async fn handle_deletion_application_message(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        message: &Message,
    ) -> Result<()> {
        let last_message_id = self.get_last_message_id(account_pubkey, group_id).await;

        for (trigger, msg) in self
            .cache_deletion(account_pubkey, group_id, message)
            .await?
        {
            self.emit_message_update(group_id, trigger, msg);
        }

        // Check if the deleted message was the last message.
        // This check must happen AFTER get_last_message_id but the
        // result is only valid for the FIRST handler (before cache_deletion
        // modifies shared state). We emit for ALL subscribed accounts because
        // subsequent handlers will see incorrect post-deletion state.
        if let Some(last_message_id) = last_message_id {
            let deleted_ids = extract_deletion_target_ids(&message.tags);
            if deleted_ids.contains(&last_message_id) {
                self.emit_chat_list_update_for_group(
                    group_id,
                    ChatListUpdateTrigger::LastMessageDeleted,
                )
                .await;
            }
        }

        Ok(())
    }

    #[perf_instrument("event_handlers")]
    async fn handle_non_application_outcome(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        mdk: &mdk_core::prelude::MDK<MdkSqliteStorage>,
        result: MessageProcessingResult,
    ) -> Result<()> {
        // Dispatch on every variant explicitly so the compiler enforces exhaustiveness.
        // Unprocessable and PreviouslyFailed are returned as errors so the caller does
        // not mark the event as processed or advance last_synced_at.
        match result {
            MessageProcessingResult::ApplicationMessage(_) => Ok(()),
            MessageProcessingResult::Proposal(update_result) => {
                self.handle_auto_committed_proposal(session, account, update_result)
                    .await
            }
            MessageProcessingResult::PendingProposal { mls_group_id } => {
                tracing::info!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    "Stored pending proposal for group {} (awaiting admin commit)",
                    hex::encode(mls_group_id.as_slice())
                );
                Ok(())
            }
            MessageProcessingResult::IgnoredProposal {
                mls_group_id,
                reason,
            } => {
                tracing::info!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    "Ignored proposal for group {}: {}",
                    hex::encode(mls_group_id.as_slice()),
                    reason
                );
                Ok(())
            }
            MessageProcessingResult::ExternalJoinProposal { mls_group_id } => {
                tracing::info!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    "Received external join proposal for group {}",
                    hex::encode(mls_group_id.as_slice())
                );
                Ok(())
            }
            MessageProcessingResult::Commit { mls_group_id } => {
                self.handle_commit_outcome(session, account, mdk, &mls_group_id)
                    .await
            }
            MessageProcessingResult::Unprocessable { mls_group_id } => {
                tracing::warn!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    "MLS message unprocessable for group {} (account {}): \
                     event will not be marked processed",
                    hex::encode(mls_group_id.as_slice()),
                    account.pubkey.to_hex()
                );
                Err(WhitenoiseError::MlsMessageUnprocessable(hex::encode(
                    mls_group_id.as_slice(),
                )))
            }
            MessageProcessingResult::PreviouslyFailed => {
                tracing::warn!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    "MLS message was previously failed for account {}: \
                     event will not be marked processed",
                    account.pubkey.to_hex()
                );
                Err(WhitenoiseError::MlsMessagePreviouslyFailed)
            }
        }
    }

    #[perf_instrument("event_handlers")]
    async fn handle_auto_committed_proposal(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        update_result: mdk_core::prelude::UpdateGroupResult,
    ) -> Result<()> {
        let group_id = &update_result.mls_group_id;
        let groups = session.groups();
        let relay_urls = groups.ensure_relays(group_id)?;

        groups
            .publish_and_merge_commit(update_result.evolution_event.clone(), group_id, &relay_urls)
            .await?;

        self.background_refresh_account_group_subscriptions(account);

        if let Some(welcome_rumors) = &update_result.welcome_rumors
            && !welcome_rumors.is_empty()
        {
            tracing::warn!(
                target: "whitenoise::event_processor::handle_mls_message",
                "Auto-committed proposal produced {} welcome rumors that were not delivered",
                welcome_rumors.len()
            );
        }

        tracing::info!(
            target: "whitenoise::event_processor::handle_mls_message",
            "Published auto-committed proposal evolution event for group {}",
            hex::encode(group_id.as_slice())
        );

        self.emit_chat_list_update(account, group_id, ChatListUpdateTrigger::NewLastMessage)
            .await;
        self.background_sync_group_image_cache_if_needed(account, group_id);

        Ok(())
    }

    #[perf_instrument("event_handlers")]
    async fn handle_commit_outcome(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        mdk: &mdk_core::prelude::MDK<MdkSqliteStorage>,
        mls_group_id: &GroupId,
    ) -> Result<()> {
        tracing::info!(
            target: "whitenoise::event_processor::handle_mls_message",
            "Processed commit for group {}",
            hex::encode(mls_group_id.as_slice())
        );

        let still_active = match mdk.get_group(mls_group_id).map_err(WhitenoiseError::from)? {
            Some(group) => group.state == GroupState::Active,
            None => false,
        };

        if !still_active {
            tracing::info!(
                target: "whitenoise::event_processor::handle_mls_message",
                "Account {} was removed from group {} — marking group as removed",
                account.pubkey.to_hex(),
                hex::encode(mls_group_id.as_slice())
            );
            let _ = session
                .membership()
                .for_group(mls_group_id)
                .mark_as_removed()
                .await?;
        }

        if still_active
            && let Err(error) = session
                .push()
                .reconcile_group_tokens_for_active_leaves(mls_group_id)
                .await
        {
            tracing::warn!(
                target: "whitenoise::event_processor::handle_mls_message",
                account = %account.pubkey.to_hex(),
                group_id = %hex::encode(mls_group_id.as_slice()),
                error = %error,
                "Failed to reconcile group push tokens after commit processing"
            );
        }

        self.background_refresh_account_group_subscriptions(account);
        if still_active {
            self.background_sync_group_image_cache_if_needed(account, mls_group_id);
        }

        Ok(())
    }

    /// Extracts group_id, inner_event, and the full Message from a processing result.
    ///
    /// Returns Some if the result contains an application message with inner event content,
    /// None otherwise (e.g., for commits, proposals, or other non-message results).
    /// The returned Message preserves all MDK-set fields (processed_at, epoch, state, etc.).
    fn extract_message_details(
        outcome: &MessageProcessingOutcome,
    ) -> Option<(mdk_core::prelude::GroupId, UnsignedEvent, Message)> {
        match &outcome.result {
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
        self.shared
            .message_stream_manager
            .emit(group_id, MessageUpdate { trigger, message });
    }

    /// Gets the ID of the last message in a group (if any).
    async fn get_last_message_id(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
    ) -> Option<String> {
        let session = self.account_manager.get_session(account_pubkey)?;
        AggregatedMessage::find_last_by_group_ids(
            std::slice::from_ref(group_id),
            &session.account_db.inner,
        )
        .await
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|s| s.message_id.to_hex())
    }

    /// Cache a new chat message and return it for emission.
    ///
    /// Processes the message through the aggregator, inserts into database,
    /// and applies any orphaned reactions/deletions that arrived before this message.
    #[perf_instrument("event_handlers")]
    async fn cache_chat_message(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        message: &Message,
    ) -> Result<ChatMessage> {
        let session = self
            .account_manager
            .get_session(account_pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        let media_files = MediaFile::find_by_group(
            &session.account_db.inner.pool,
            &self.shared.database,
            account_pubkey,
            group_id,
        )
        .await?;

        let mut chat_message = self
            .shared
            .message_aggregator
            .process_single_message(message, &ContentParser::new(), media_files)
            .await?;

        // Preserve existing delivery status for relay echoes of locally-sent messages.
        // This keeps stream payloads aligned with the latest DB state instead of
        // regressing to `None` on reprocessing.
        if let Some(existing_message) = AggregatedMessage::find_by_id(
            &chat_message.id,
            group_id,
            account_pubkey,
            &session.account_db.inner,
        )
        .await?
            && existing_message.delivery_status.is_some()
        {
            chat_message.delivery_status = existing_message.delivery_status;
        }

        AggregatedMessage::insert_message(
            &chat_message,
            group_id,
            account_pubkey,
            &session.account_db.inner,
        )
        .await?;

        // Apply orphaned reactions/deletions - modifies in-place and returns final state
        let final_message = self
            .apply_orphaned_reactions_and_deletions(account_pubkey, chat_message, group_id)
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
    /// Returns `Ok(None)` if the target message isn't cached yet (orphaned reaction),
    /// or if the reaction was already processed as an outgoing event (echo from relay).
    /// Propagates real errors (malformed tags, invalid emoji, DB failures).
    #[perf_instrument("event_handlers")]
    async fn cache_reaction(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        message: &Message,
    ) -> Result<Option<ChatMessage>> {
        let session = self
            .account_manager
            .get_session(account_pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        // If this reaction already has a delivery status, it was sent by us and already
        // applied to the parent — skip re-applying to avoid unnecessary DB writes and
        // duplicate UI emissions.
        if AggregatedMessage::has_delivery_status(
            &message.id.to_string(),
            group_id,
            account_pubkey,
            &session.account_db.inner,
        )
        .await?
        {
            tracing::debug!(
                target: "whitenoise::cache",
                "Skipping echo of outgoing reaction {} in group {}",
                message.id,
                hex::encode(group_id.as_slice())
            );
            return Ok(None);
        }

        AggregatedMessage::insert_reaction(message, group_id, &session.account_db.inner).await?;

        let result = self
            .apply_reaction_to_target(account_pubkey, message, group_id)
            .await?;

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
        account_pubkey: &PublicKey,
        reaction: &Message,
        group_id: &GroupId,
    ) -> Result<Option<ChatMessage>> {
        let session = self
            .account_manager
            .get_session(account_pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        let target_id = extract_reaction_target_id(&reaction.tags)?;

        let Some(mut target) = AggregatedMessage::find_by_id(
            &target_id,
            group_id,
            account_pubkey,
            &session.account_db.inner,
        )
        .await?
        else {
            return Ok(None); // True orphan: target not yet cached
        };

        let emoji = emoji_utils::validate_and_normalize_reaction(
            &reaction.content,
            self.shared.message_aggregator.config().normalize_emoji,
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
            &session.account_db.inner,
        )
        .await?;

        Ok(Some(target))
    }

    /// Cache a deletion and return updates for all affected messages.
    ///
    /// A single deletion can target multiple events (reactions and/or messages),
    /// so this returns a Vec of (trigger, message) pairs.
    #[perf_instrument("event_handlers")]
    async fn cache_deletion(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        message: &Message,
    ) -> Result<Vec<(UpdateTrigger, ChatMessage)>> {
        let session = self
            .account_manager
            .get_session(account_pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        // If this deletion already has a delivery status, it was sent by us and already
        // applied to targets — skip re-applying to avoid unnecessary DB writes and
        // duplicate UI emissions.
        if AggregatedMessage::has_delivery_status(
            &message.id.to_string(),
            group_id,
            account_pubkey,
            &session.account_db.inner,
        )
        .await?
        {
            tracing::debug!(
                target: "whitenoise::cache",
                "Skipping echo of outgoing deletion {} in group {}",
                message.id,
                hex::encode(group_id.as_slice())
            );
            return Ok(Vec::new());
        }

        AggregatedMessage::insert_deletion(message, group_id, &session.account_db.inner).await?;

        let updates = self
            .apply_deletions_to_targets(account_pubkey, message, group_id)
            .await?;

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
        account_pubkey: &PublicKey,
        deletion: &Message,
        group_id: &GroupId,
    ) -> Result<Vec<(UpdateTrigger, ChatMessage)>> {
        let target_ids = extract_deletion_target_ids(&deletion.tags);
        let mut updates = Vec::with_capacity(target_ids.len());

        for target_id in target_ids {
            if let Some(update) = self
                .apply_single_deletion(
                    account_pubkey,
                    &deletion.pubkey,
                    &target_id,
                    &deletion.id,
                    group_id,
                )
                .await?
            {
                updates.push(update);
            }
        }

        Ok(updates)
    }

    /// Apply deletion to a single target, returning the appropriate update.
    ///
    /// Only authors may delete their own reactions or messages. Deletions whose
    /// `deletion_author` does not match the target row's `author` are skipped
    /// with a warning so a malicious peer can't wipe other members' content.
    async fn apply_single_deletion(
        &self,
        account_pubkey: &PublicKey,
        deletion_author: &PublicKey,
        target_id: &str,
        deletion_event_id: &EventId,
        group_id: &GroupId,
    ) -> Result<Option<(UpdateTrigger, ChatMessage)>> {
        let session = self
            .account_manager
            .get_session(account_pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        // Check if target is a reaction
        if let Some(reaction) =
            AggregatedMessage::find_reaction_by_id(target_id, group_id, &session.account_db.inner)
                .await?
        {
            if &reaction.author != deletion_author {
                tracing::warn!(
                    target: "whitenoise::cache",
                    deletion_author = %deletion_author.to_hex(),
                    target_author = %reaction.author.to_hex(),
                    target_id = target_id,
                    "Ignoring deletion whose author does not match reaction author"
                );
                return Ok(None);
            }

            let parent_update = self
                .remove_reaction_from_parent(account_pubkey, &reaction, group_id)
                .await?;
            AggregatedMessage::mark_deleted(
                target_id,
                group_id,
                &deletion_event_id.to_string(),
                deletion_author,
                &session.account_db.inner,
            )
            .await?;
            return Ok(parent_update.map(|msg| (UpdateTrigger::ReactionRemoved, msg)));
        }

        // Check if target is a message
        if let Some(mut msg) = AggregatedMessage::find_by_id(
            target_id,
            group_id,
            account_pubkey,
            &session.account_db.inner,
        )
        .await?
        {
            if &msg.author != deletion_author {
                tracing::warn!(
                    target: "whitenoise::cache",
                    deletion_author = %deletion_author.to_hex(),
                    target_author = %msg.author.to_hex(),
                    target_id = target_id,
                    "Ignoring deletion whose author does not match message author"
                );
                return Ok(None);
            }

            msg.is_deleted = true;
            AggregatedMessage::mark_deleted(
                target_id,
                group_id,
                &deletion_event_id.to_string(),
                deletion_author,
                &session.account_db.inner,
            )
            .await?;
            return Ok(Some((UpdateTrigger::MessageDeleted, msg)));
        }

        // Unknown target — orphan; reconciliation happens when the target arrives.
        Ok(None)
    }

    /// Remove a reaction from its parent message and return the updated parent.
    async fn remove_reaction_from_parent(
        &self,
        account_pubkey: &PublicKey,
        reaction: &AggregatedMessage,
        group_id: &GroupId,
    ) -> Result<Option<ChatMessage>> {
        let session = self
            .account_manager
            .get_session(account_pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        let Ok(parent_id) = extract_reaction_target_id(&reaction.tags) else {
            return Ok(None);
        };

        let Some(mut parent) = AggregatedMessage::find_by_id(
            &parent_id,
            group_id,
            account_pubkey,
            &session.account_db.inner,
        )
        .await?
        else {
            return Ok(None);
        };

        if reaction_handler::remove_reaction_from_message(&mut parent, &reaction.author) {
            AggregatedMessage::update_reactions(
                &parent_id,
                group_id,
                &parent.reactions,
                &session.account_db.inner,
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

    /// Apply any orphaned reactions/deletions to a newly cached message.
    ///
    /// Takes ownership of the message, modifies in-place, and returns the final state.
    /// This avoids re-fetching from the database after applying orphans.
    async fn apply_orphaned_reactions_and_deletions(
        &self,
        account_pubkey: &PublicKey,
        mut message: ChatMessage,
        group_id: &GroupId,
    ) -> Result<ChatMessage> {
        let session = self
            .account_manager
            .get_session(account_pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        let orphaned_reactions = AggregatedMessage::find_orphaned_reactions(
            &message.id,
            group_id,
            &session.account_db.inner,
        )
        .await?;

        let orphaned_deletions = AggregatedMessage::find_orphaned_deletions(
            &message.id,
            group_id,
            &session.account_db.inner,
        )
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
                self.shared.message_aggregator.config().normalize_emoji,
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
                &session.account_db.inner,
            )
            .await?;
        }

        // Apply orphaned deletions, but only those whose author matches the
        // target message author. Cross-author deletions never legitimately
        // apply and would let a malicious peer wipe other members' content.
        for deletion in orphaned_deletions {
            if deletion.author != message.author {
                tracing::warn!(
                    target: "whitenoise::cache",
                    deletion_author = %deletion.author.to_hex(),
                    target_author = %message.author.to_hex(),
                    target_id = %message.id,
                    "Ignoring orphaned deletion whose author does not match message author"
                );
                continue;
            }

            message.is_deleted = true;
            AggregatedMessage::mark_deleted(
                &message.id,
                group_id,
                &deletion.event_id.to_string(),
                &deletion.author,
                &session.account_db.inner,
            )
            .await?;
        }

        Ok(message)
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use std::time::Duration;

    use mdk_core::mip05::{
        ENCRYPTED_TOKEN_LEN, LeafTokenTag, TokenTag, build_token_list_response_rumor,
        build_token_removal_rumor, build_token_request_rumor,
    };
    use mdk_core::prelude::UpdateGroupResult;

    use super::*;
    use crate::whitenoise::{
        aggregated_message::AggregatedMessage, message_aggregator::DeliveryStatus,
        push_notifications::GroupPushToken, test_utils::*,
    };

    fn make_token_tag(seed: u8) -> TokenTag {
        TokenTag {
            encrypted_token: mdk_core::mip05::EncryptedToken::from([seed; ENCRYPTED_TOKEN_LEN]),
            server_pubkey: Keys::generate().public_key(),
            relay_hint: RelayUrl::parse("wss://push.example.com").unwrap(),
        }
    }

    async fn setup_two_member_group(
        whitenoise: &Whitenoise,
        admin_account: &Account,
        member_account: &Account,
    ) -> GroupId {
        setup_two_member_group_with_accepted_account_groups(
            whitenoise,
            admin_account,
            member_account,
        )
        .await
    }

    /// Test handling of different MLS message types: regular messages, reactions, and deletions
    #[tokio::test]
    async fn test_handle_mls_message_different_types() {
        // Arrange: Setup whitenoise and create a group
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        wait_for_key_package_publication(&whitenoise, &[&members[0].0]).await;

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
        let message_event = mdk.create_message(group_id, inner, None).unwrap();

        let result = whitenoise
            .handle_mls_message(&creator_session, &creator_account, message_event)
            .await;
        assert!(result.is_ok(), "Failed to handle regular message");

        // Verify message was cached
        let cached_msg = AggregatedMessage::find_by_id(
            &message_id.to_string(),
            group_id,
            &creator_account.pubkey,
            &creator_session.account_db.inner,
        )
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
        let reaction_event = mdk.create_message(group_id, reaction_inner, None).unwrap();

        let result = whitenoise
            .handle_mls_message(&creator_session, &creator_account, reaction_event)
            .await;
        assert!(result.is_ok(), "Failed to handle reaction");

        // Verify reaction was applied to cached message
        let cached_msg = AggregatedMessage::find_by_id(
            &message_id.to_string(),
            group_id,
            &creator_account.pubkey,
            &creator_session.account_db.inner,
        )
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
        let deletion_event = mdk.create_message(group_id, deletion_inner, None).unwrap();

        let result = whitenoise
            .handle_mls_message(&creator_session, &creator_account, deletion_event)
            .await;
        assert!(result.is_ok(), "Failed to handle deletion");

        // Verify message was marked as deleted
        let cached_msg = AggregatedMessage::find_by_id(
            &message_id.to_string(),
            group_id,
            &creator_account.pubkey,
            &creator_session.account_db.inner,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(cached_msg.is_deleted, "Message should be marked as deleted");
    }

    #[tokio::test]
    async fn test_cache_chat_message_preserves_existing_delivery_status() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        wait_for_key_package_publication(&whitenoise, &[&members[0].0]).await;

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
            "Echo status preservation".to_string(),
        );
        inner.ensure_id();
        let message_id = inner.id.unwrap();
        mdk.create_message(&group.mls_group_id, inner, None)
            .unwrap();

        let message = mdk
            .get_message(&group.mls_group_id, &message_id)
            .unwrap()
            .expect("message should exist in mdk");

        // Initial cache pass creates the row without delivery status.
        let first = whitenoise
            .cache_chat_message(&creator_account.pubkey, &group.mls_group_id, &message)
            .await
            .unwrap();
        assert_eq!(first.delivery_status, None);

        // Simulate background publish completion updating delivery status.
        AggregatedMessage::update_delivery_status(
            &message_id.to_string(),
            &group.mls_group_id,
            &creator_account.pubkey,
            &DeliveryStatus::Sent(1),
            &creator_session.account_db.inner,
        )
        .await
        .unwrap();

        // Relay echo reprocess should preserve the existing status.
        let second = whitenoise
            .cache_chat_message(&creator_account.pubkey, &group.mls_group_id, &message)
            .await
            .unwrap();
        assert_eq!(second.delivery_status, Some(DeliveryStatus::Sent(1)));

        let persisted = AggregatedMessage::find_by_id(
            &message_id.to_string(),
            &group.mls_group_id,
            &creator_account.pubkey,
            &creator_session.account_db.inner,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(persisted.delivery_status, Some(DeliveryStatus::Sent(1)));
    }

    /// Test error handling for invalid MLS messages
    #[tokio::test]
    async fn test_handle_mls_message_error_handling() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        wait_for_key_package_publication(&whitenoise, &[&members[0].0]).await;

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
        let valid_event = mdk
            .create_message(&group.mls_group_id, inner, None)
            .unwrap();

        // Corrupt the event by changing its kind (MLS processing should fail)
        let mut bad_event = valid_event;
        bad_event.kind = Kind::TextNote;

        let result = whitenoise
            .handle_mls_message(&creator_session, &creator_account, bad_event)
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
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        wait_for_key_package_publication(&whitenoise, &[&members[0].0]).await;

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

        // Build the message first so MDK can stamp a valid content-hash id,
        // then tag the orphaned reaction at that id.
        let mut actual_message = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Late message".to_string(),
        );
        actual_message.ensure_id();
        let future_message_id = actual_message.id.expect("ensure_id sets the id");

        let mut orphaned_reaction = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Reaction,
            vec![Tag::parse(vec!["e", &future_message_id.to_string()]).unwrap()],
            "+".to_string(), // Use simple emoji that won't be normalized
        );
        orphaned_reaction.ensure_id();
        let reaction_event = mdk
            .create_message(group_id, orphaned_reaction, None)
            .unwrap();

        let result = whitenoise
            .handle_mls_message(&creator_session, &creator_account, reaction_event)
            .await;
        assert!(result.is_ok(), "Orphaned reaction should be stored");

        // Verify orphaned reaction is stored
        let orphaned_reactions = AggregatedMessage::find_orphaned_reactions(
            &future_message_id.to_string(),
            group_id,
            &creator_session.account_db.inner,
        )
        .await
        .unwrap();
        assert_eq!(
            orphaned_reactions.len(),
            1,
            "Should have one orphaned reaction"
        );

        let message_event = mdk.create_message(group_id, actual_message, None).unwrap();

        let result = whitenoise
            .handle_mls_message(&creator_session, &creator_account, message_event)
            .await;
        assert!(
            result.is_ok(),
            "Message with orphaned reaction should succeed"
        );

        // Verify the orphaned reaction was applied
        let cached_msg = AggregatedMessage::find_by_id(
            &future_message_id.to_string(),
            group_id,
            &creator_account.pubkey,
            &creator_session.account_db.inner,
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
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        wait_for_key_package_publication(&whitenoise, &[&members[0].0]).await;

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

        // Build the target first so MDK can stamp a valid content-hash id,
        // then tag the orphaned reactions at that id.
        let mut actual_message = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Target message".to_string(),
        );
        actual_message.ensure_id();
        let future_message_id = actual_message.id.expect("ensure_id sets the id");

        // Send a VALID orphaned reaction
        let mut valid_reaction = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Reaction,
            vec![Tag::parse(vec!["e", &future_message_id.to_string()]).unwrap()],
            "👍".to_string(),
        );
        valid_reaction.ensure_id();
        let valid_event = mdk.create_message(group_id, valid_reaction, None).unwrap();

        whitenoise
            .handle_mls_message(&creator_session, &creator_account, valid_event)
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
        let invalid_event = mdk
            .create_message(group_id, invalid_reaction, None)
            .unwrap();

        whitenoise
            .handle_mls_message(&creator_session, &creator_account, invalid_event)
            .await
            .unwrap();

        let message_event = mdk.create_message(group_id, actual_message, None).unwrap();

        let result = whitenoise
            .handle_mls_message(&creator_session, &creator_account, message_event)
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
            &creator_account.pubkey,
            &creator_session.account_db.inner,
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

        let outcome = MessageProcessingOutcome {
            result: MessageProcessingResult::ApplicationMessage(message),
            context: mdk_core::prelude::MessageProcessingContext::default(),
        };
        let extracted = Whitenoise::extract_message_details(&outcome);
        assert!(extracted.is_some(), "Should extract application message");
        let (extracted_group_id, extracted_event, extracted_message) = extracted.unwrap();
        assert_eq!(extracted_group_id, group_id);
        assert_eq!(extracted_event.content, "Test");
        assert_eq!(extracted_message.content, "Test");
        assert_eq!(extracted_message.mls_group_id, group_id);

        // Test extract_message_details with non-ApplicationMessage
        let commit_outcome = MessageProcessingOutcome {
            result: MessageProcessingResult::Commit {
                mls_group_id: group_id,
            },
            context: mdk_core::prelude::MessageProcessingContext::default(),
        };
        let extracted = Whitenoise::extract_message_details(&commit_outcome);
        assert!(extracted.is_none(), "Should not extract commit");

        // Test extract_reaction_target_id
        let mut tags = Tags::new();
        tags.push(Tag::parse(vec!["e", "test_event_id"]).unwrap());
        let target_id = extract_reaction_target_id(&tags).unwrap();
        assert_eq!(target_id, "test_event_id");

        // Test extract_reaction_target_id with missing e-tag
        let empty_tags = Tags::new();
        let result = extract_reaction_target_id(&empty_tags);
        assert!(result.is_err(), "Should fail with missing e-tag");

        // Test extract_deletion_target_ids with multiple targets
        let mut tags = Tags::new();
        tags.push(Tag::parse(vec!["e", "id1"]).unwrap());
        tags.push(Tag::parse(vec!["e", "id2"]).unwrap());
        tags.push(Tag::parse(vec!["p", "some_pubkey"]).unwrap()); // Should be ignored

        let target_ids = extract_deletion_target_ids(&tags);
        assert_eq!(target_ids.len(), 2);
        assert!(target_ids.contains(&"id1".to_string()));
        assert!(target_ids.contains(&"id2".to_string()));
    }

    /// Test message cache integration with real message flow
    #[tokio::test]
    async fn test_handle_mls_message_cache_integration() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;

        let member_accounts = members
            .iter()
            .map(|(account, _)| account)
            .collect::<Vec<_>>();
        wait_for_key_package_publication(&whitenoise, &member_accounts).await;

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
            let event = mdk
                .create_message(&group.mls_group_id, inner, None)
                .unwrap();

            whitenoise
                .handle_mls_message(&creator_session, &creator_account, event)
                .await
                .unwrap();
        }

        // Verify all messages are in cache
        let messages = AggregatedMessage::find_messages_by_group(
            &group.mls_group_id,
            &creator_account.pubkey,
            None,
            &creator_session.account_db.inner,
        )
        .await
        .unwrap();

        assert_eq!(messages.len(), 3, "All messages should be cached");
        let mut contents: Vec<&str> = messages.iter().map(|m| m.content.as_str()).collect();
        contents.sort();
        assert_eq!(contents, vec!["Message 1", "Message 2", "Message 3"]);

        // Verify messages are accessible via public API
        let fetched = whitenoise
            .fetch_aggregated_messages_for_group(
                &creator_account.pubkey,
                &group.mls_group_id,
                &PaginationOptions::default(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(fetched.len(), 3, "Should fetch all cached messages");
    }

    #[tokio::test]
    async fn test_handle_mls_message_stores_token_request_in_group_push_cache() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group(&whitenoise, &admin_account, &member_account).await;
        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let admin_leaf_index = admin_mdk.own_leaf_index(&group_id).unwrap();

        let token_tag = make_token_tag(1);
        let request = build_token_request_rumor(
            admin_account.pubkey,
            Timestamp::now(),
            vec![token_tag.clone()],
        )
        .unwrap();
        let event = admin_mdk.create_message(&group_id, request, None).unwrap();

        whitenoise
            .handle_mls_message(&member_session, &member_account, event)
            .await
            .unwrap();

        let member_pool = &whitenoise
            .require_session(&member_account.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        let stored = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            member_pool,
        )
        .await
        .unwrap();

        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].leaf_index, admin_leaf_index);
        assert_eq!(stored[0].server_pubkey, token_tag.server_pubkey);
        assert_eq!(stored[0].relay_hint, Some(token_tag.relay_hint));
        assert_eq!(
            stored[0].encrypted_token,
            token_tag.encrypted_token.to_base64()
        );

        let cached_messages = AggregatedMessage::find_messages_by_group(
            &group_id,
            &member_account.pubkey,
            None,
            &member_session.account_db.inner,
        )
        .await
        .unwrap();
        assert!(cached_messages.is_empty());
    }

    #[tokio::test]
    async fn test_handle_mls_message_merges_token_list_response_into_group_push_cache() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group(&whitenoise, &admin_account, &member_account).await;
        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();

        let leaf_zero = make_token_tag(2);
        let leaf_one = make_token_tag(3);
        let inactive_leaf = make_token_tag(9);
        let response = build_token_list_response_rumor(
            admin_account.pubkey,
            Timestamp::now(),
            EventId::all_zeros(),
            vec![
                LeafTokenTag {
                    token_tag: leaf_zero.clone(),
                    leaf_index: 0,
                },
                LeafTokenTag {
                    token_tag: leaf_one.clone(),
                    leaf_index: 1,
                },
                LeafTokenTag {
                    token_tag: inactive_leaf,
                    leaf_index: 99,
                },
            ],
        )
        .unwrap();
        let event = admin_mdk.create_message(&group_id, response, None).unwrap();

        whitenoise
            .handle_mls_message(&member_session, &member_account, event)
            .await
            .unwrap();

        let member_pool = &whitenoise
            .require_session(&member_account.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        let stored = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            member_pool,
        )
        .await
        .unwrap();

        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].leaf_index, 0);
        assert_eq!(
            stored[0].encrypted_token,
            leaf_zero.encrypted_token.to_base64()
        );
        assert_eq!(stored[1].leaf_index, 1);
        assert_eq!(
            stored[1].encrypted_token,
            leaf_one.encrypted_token.to_base64()
        );

        let cached_messages = AggregatedMessage::find_messages_by_group(
            &group_id,
            &member_account.pubkey,
            None,
            &member_session.account_db.inner,
        )
        .await
        .unwrap();
        assert!(cached_messages.is_empty());
    }

    #[tokio::test]
    async fn test_handle_mls_message_removes_token_on_token_removal() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group(&whitenoise, &admin_account, &member_account).await;
        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let admin_leaf_index = admin_mdk.own_leaf_index(&group_id).unwrap();
        let token_tag = make_token_tag(4);

        let member_pool = &whitenoise
            .require_session(&member_account.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        GroupPushToken::upsert(
            &member_account.pubkey,
            &group_id,
            &admin_account.pubkey,
            admin_leaf_index,
            &token_tag.server_pubkey,
            Some(&token_tag.relay_hint),
            &token_tag.encrypted_token.to_base64(),
            member_pool,
        )
        .await
        .unwrap();

        let removal = build_token_removal_rumor(admin_account.pubkey, Timestamp::now());
        let event = admin_mdk.create_message(&group_id, removal, None).unwrap();

        whitenoise
            .handle_mls_message(&member_session, &member_account, event)
            .await
            .unwrap();

        let stored = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            member_pool,
        )
        .await
        .unwrap();
        assert!(stored.is_empty());

        let cached_messages = AggregatedMessage::find_messages_by_group(
            &group_id,
            &member_account.pubkey,
            None,
            &member_session.account_db.inner,
        )
        .await
        .unwrap();
        assert!(cached_messages.is_empty());
    }

    #[tokio::test]
    async fn test_handle_mls_message_token_request_schedules_token_list_response() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group(&whitenoise, &admin_account, &member_account).await;
        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let before_count = count_published_events_for_account(&whitenoise, &member_account).await;

        let token_tag = make_token_tag(5);
        let request =
            build_token_request_rumor(admin_account.pubkey, Timestamp::now(), vec![token_tag])
                .unwrap();
        let request_event_id = request.id.expect("447 rumor must have an event id");
        let event = admin_mdk.create_message(&group_id, request, None).unwrap();

        whitenoise
            .handle_mls_message(&member_session, &member_account, event)
            .await
            .unwrap();

        assert!(whitenoise.has_pending_token_response(
            &member_account.pubkey,
            &group_id,
            &request_event_id,
        ));

        let dispatched = whitenoise
            .dispatch_pending_token_response(&member_account, &group_id, request_event_id)
            .await
            .unwrap();
        assert!(dispatched);

        let after_count =
            wait_for_published_event_count(&whitenoise, &member_account, before_count).await;
        assert!(after_count > before_count);
    }

    #[tokio::test]
    async fn test_handle_mls_message_matching_token_list_response_suppresses_pending_reply() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group(&whitenoise, &admin_account, &member_account).await;
        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();

        let request = build_token_request_rumor(
            admin_account.pubkey,
            Timestamp::now(),
            vec![make_token_tag(6)],
        )
        .unwrap();
        let request_event_id = request.id.expect("447 rumor must have an event id");
        let request_event = admin_mdk.create_message(&group_id, request, None).unwrap();

        whitenoise
            .handle_mls_message(&member_session, &member_account, request_event)
            .await
            .unwrap();

        assert!(whitenoise.has_pending_token_response(
            &member_account.pubkey,
            &group_id,
            &request_event_id,
        ));

        let response = build_token_list_response_rumor(
            admin_account.pubkey,
            Timestamp::now(),
            request_event_id,
            vec![LeafTokenTag {
                token_tag: make_token_tag(7),
                leaf_index: 0,
            }],
        )
        .unwrap();
        let response_event = admin_mdk.create_message(&group_id, response, None).unwrap();

        whitenoise
            .handle_mls_message(&member_session, &member_account, response_event)
            .await
            .unwrap();

        assert!(!whitenoise.has_pending_token_response(
            &member_account.pubkey,
            &group_id,
            &request_event_id,
        ));

        let dispatched = whitenoise
            .dispatch_pending_token_response(&member_account, &group_id, request_event_id)
            .await
            .unwrap();
        assert!(!dispatched);
    }

    #[tokio::test]
    async fn test_handle_mls_message_commit_prunes_inactive_leaf_tokens() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 2).await;
        let member_one = members[0].0.clone();
        let member_two = members[1].0.clone();
        let member_two_session = whitenoise.require_session(&member_two.pubkey).unwrap();

        wait_for_key_package_publication(&whitenoise, &[&member_one, &member_two]).await;

        let group_id =
            setup_three_member_group(&whitenoise, &admin_account, &member_one, &member_two).await;

        let observer_mdk = whitenoise
            .create_mdk_for_account(member_two.pubkey)
            .unwrap();
        let removed_leaf_index = observer_mdk
            .group_leaf_map(&group_id)
            .unwrap()
            .iter()
            .find_map(|(leaf_index, pubkey)| (*pubkey == member_one.pubkey).then_some(*leaf_index))
            .expect("removed member leaf should exist before removal");
        let stale_token = make_token_tag(8);

        let member_two_pool = &whitenoise
            .require_session(&member_two.pubkey)
            .unwrap()
            .account_db
            .inner
            .pool;
        GroupPushToken::upsert(
            &member_two.pubkey,
            &group_id,
            &member_one.pubkey,
            removed_leaf_index,
            &stale_token.server_pubkey,
            Some(&stale_token.relay_hint),
            &stale_token.encrypted_token.to_base64(),
            member_two_pool,
        )
        .await
        .unwrap();

        let removal_event = {
            let admin_mdk = whitenoise
                .create_mdk_for_account(admin_account.pubkey)
                .unwrap();
            admin_mdk
                .remove_members(&group_id, &[member_one.pubkey])
                .unwrap()
                .evolution_event
        };

        whitenoise
            .handle_mls_message(&member_two_session, &member_two, removal_event)
            .await
            .unwrap();

        let stored = GroupPushToken::find_by_account_and_group(
            &member_two.pubkey,
            &group_id,
            member_two_pool,
        )
        .await
        .unwrap();

        assert!(
            stored
                .iter()
                .all(|token| token.leaf_index != removed_leaf_index),
            "inactive leaf token should be pruned after commit processing"
        );
    }

    /// Test that auto-committed proposals (e.g., admin auto-commits a
    /// member's self-removal) are published and merged successfully.
    #[tokio::test]
    async fn test_handle_mls_message_auto_committed_proposal() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let admin_account = whitenoise.create_identity().await.unwrap();
        let admin_session = whitenoise.require_session(&admin_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

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
            .handle_mls_message(&admin_session, &admin_account, leave_result.evolution_event)
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

    /// Duplicate MLS messages (already-processed by MDK) return
    /// `MlsMessageUnprocessable` so the caller does not mark the event as
    /// processed or advance `last_synced_at`.
    #[tokio::test]
    async fn test_unprocessable_message_returns_err() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;

        let member_accounts = members
            .iter()
            .map(|(account, _)| account)
            .collect::<Vec<_>>();
        wait_for_key_package_publication(&whitenoise, &member_accounts).await;

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

        let mut inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Test message".to_string(),
        );
        inner.ensure_id();
        let event = mdk
            .create_message(&group.mls_group_id, inner, None)
            .unwrap();

        // First processing: should succeed
        let first = whitenoise
            .handle_mls_message(&creator_session, &creator_account, event.clone())
            .await;
        assert!(first.is_ok(), "First processing should succeed: {first:?}");

        // Second processing of the same event: MDK returns Unprocessable because
        // the event was already processed and its state recorded as Processed.
        // Our handler must propagate this as an error.
        let second = whitenoise
            .handle_mls_message(&creator_session, &creator_account, event)
            .await;
        assert!(
            second.is_err(),
            "Second processing of same event should return Err"
        );
        match second.err().unwrap() {
            WhitenoiseError::MlsMessageUnprocessable(_) => {}
            other => panic!("Expected MlsMessageUnprocessable, got: {other:?}"),
        }
    }

    /// Verify that when `handle_mls_message` returns `Err` for an unprocessable
    /// event, the account event processor does NOT record it as processed.
    ///
    /// This is tested end-to-end through `process_account_event`: we send the
    /// same event twice.  After the second attempt (which the handler rejects as
    /// `Unprocessable`), the event must NOT appear in the processed-event tracker
    /// for the account.  It was already recorded by the first successful pass, so
    /// the tracker returns `true`; the important thing is that the second `Err`
    /// path does not call `track_processed_account_event` a second time (the
    /// tracker would silently ignore duplicates, but we can still assert the
    /// correct error path by inspecting `already_processed_account_event` and
    /// confirming it reflects the one tracked entry from the FIRST call only,
    /// not a spurious second call that could corrupt `last_synced_at`).
    ///
    /// The real observable guarantee: `process_account_event` only advances
    /// `last_synced_at` on `Ok`.  We confirm this indirectly by asserting that
    /// the second call to `handle_mls_message` returns `Err`.
    #[tokio::test]
    async fn test_unprocessable_not_tracked_via_process_account_event() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;

        let member_accounts = members
            .iter()
            .map(|(account, _)| account)
            .collect::<Vec<_>>();
        wait_for_key_package_publication(&whitenoise, &member_accounts).await;

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

        let mut inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Unprocessable test".to_string(),
        );
        inner.ensure_id();
        let event = mdk
            .create_message(&group.mls_group_id, inner, None)
            .unwrap();
        let event_id = event.id;

        // Build a relay-plane source context for this account.
        let source = EventSource::RelaySubscription(SubscriptionContext {
            plane: RelayPlane::Group,
            account_pubkey: Some(creator_account.pubkey),
            relay_url: RelayUrl::parse("wss://relay.example.com").unwrap(),
            stream: SubscriptionStream::GroupMessages,
            group_ids: vec![],
        });

        // First pass through process_account_event: succeeds, event is tracked.
        whitenoise
            .process_account_event(event.clone(), source.clone(), Default::default())
            .await;

        let tracked_after_first = whitenoise
            .shared
            .event_tracker
            .already_processed_account_event(&event_id, &creator_account.pubkey)
            .await
            .unwrap();
        assert!(
            tracked_after_first,
            "Event should be tracked after first successful processing"
        );

        // Second pass: the should_skip check will detect it as already processed
        // and skip it WITHOUT calling handle_mls_message at all, so last_synced_at
        // is not advanced for a duplicate.  This is the intended guard.
        // We verify the skip path by confirming it doesn't panic or double-advance.
        whitenoise
            .process_account_event(event.clone(), source.clone(), Default::default())
            .await;

        // Still tracked — no double-entry, no crash.
        let tracked_after_second = whitenoise
            .shared
            .event_tracker
            .already_processed_account_event(&event_id, &creator_account.pubkey)
            .await
            .unwrap();
        assert!(
            tracked_after_second,
            "Event should still be tracked after second call"
        );

        // Confirm that handle_mls_message itself returns Err on duplicate so
        // any caller that bypasses the skip check also cannot silently succeed.
        let direct_result = whitenoise
            .handle_mls_message(&creator_session, &creator_account, event)
            .await;
        assert!(
            direct_result.is_err(),
            "Direct second call to handle_mls_message must return Err"
        );
    }

    /// Verify MLS state consistency after an auto-committed removal:
    /// the admin can still create messages in the group, confirming
    /// that merge_pending_commit left the state valid.
    #[tokio::test]
    async fn test_handle_mls_message_commit_after_auto_committed_proposal() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let admin_account = whitenoise.create_identity().await.unwrap();
        let admin_session = whitenoise.require_session(&admin_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group(&whitenoise, &admin_account, &member_account).await;

        // Member leaves
        let member_mdk = whitenoise
            .create_mdk_for_account(member_account.pubkey)
            .unwrap();
        let leave_result = member_mdk.leave_group(&group_id).unwrap();

        // Admin auto-commits the leave proposal
        whitenoise
            .handle_mls_message(&admin_session, &admin_account, leave_result.evolution_event)
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

        let message_event = admin_mdk.create_message(&group_id, inner, None);
        assert!(
            message_event.is_ok(),
            "Admin should be able to create messages after auto-committed removal: {:?}",
            message_event.err()
        );
    }

    #[tokio::test]
    async fn test_duplicate_token_request_does_not_create_second_task() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let _admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        let fake_event_id = EventId::all_zeros();
        let fake_group_id = GroupId::from_slice(&[1u8; 32]);

        // Pre-insert the key to simulate an in-flight task holding it. This
        // avoids the race where a spawned task with delay_ms=0 could clear the
        // key before the assertion runs.
        whitenoise.insert_pending_token_response_for_test(
            member_account.pubkey,
            fake_group_id.clone(),
            fake_event_id,
        );

        assert!(whitenoise.has_pending_token_response(
            &member_account.pubkey,
            &fake_group_id,
            &fake_event_id,
        ));

        // Schedule with the same key hits the duplicate-dedup early-return;
        // the existing entry must remain intact.
        whitenoise.schedule_token_response_for_test(
            member_account.clone(),
            fake_group_id.clone(),
            fake_event_id,
        );

        assert!(
            whitenoise.has_pending_token_response(
                &member_account.pubkey,
                &fake_group_id,
                &fake_event_id,
            ),
            "pending entry should still be present after duplicate request"
        );
    }

    #[tokio::test]
    async fn test_scheduled_token_response_task_runs_and_clears_pending_entry() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group_id = setup_two_member_group(&whitenoise, &admin_account, &member_account).await;
        let admin_mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();

        let token_tag = make_token_tag(22);
        let request =
            build_token_request_rumor(admin_account.pubkey, Timestamp::now(), vec![token_tag])
                .unwrap();
        let request_event_id = request.id.expect("447 rumor must have an event id");
        let event = admin_mdk.create_message(&group_id, request, None).unwrap();

        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        whitenoise
            .handle_mls_message(&member_session, &member_account, event)
            .await
            .unwrap();

        assert!(whitenoise.has_pending_token_response(
            &member_account.pubkey,
            &group_id,
            &request_event_id,
        ));

        // In tests the spawn delay is 0ms; wait for the spawned task to clear the entry.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !whitenoise.has_pending_token_response(
                    &member_account.pubkey,
                    &group_id,
                    &request_event_id,
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for spawned token-response task to complete");
    }

    // Note: `test_token_request_dropped_when_semaphore_exhausted` was removed
    // in Phase 18c. It exercised a token-response concurrency cap that only
    // existed on the legacy `Whitenoise::schedule_pending_token_response` test
    // fixture (which writes to `SharedServices::pending_push_token_responses`,
    // a `#[cfg(test)]` map). Production token-response scheduling moved to
    // `AccountSession::schedule_pending_token_response` in Phase 12 and never
    // had a cap. The test passed pre-18c only because `has_pending_token_response`
    // was reading the wrong map; once it correctly checks the session map,
    // the assertion's premise (cap drops the entry) no longer holds.

    /// Verify `extract_group_id` returns the correct group ID for every variant
    /// that carries one, and `None` for `PreviouslyFailed`.
    #[test]
    fn test_extract_group_id_returns_correct_id_for_each_variant() {
        let group_id = GroupId::from_slice(&[0xAB; 32]);
        let pubkey = nostr_sdk::Keys::generate().public_key();

        // ApplicationMessage
        let mut inner_event = UnsignedEvent::new(
            pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "test".to_string(),
        );
        inner_event.ensure_id();
        let message = mdk_core::prelude::message_types::Message {
            id: inner_event.id.unwrap(),
            pubkey,
            created_at: Timestamp::now(),
            processed_at: Timestamp::now(),
            kind: Kind::Custom(9),
            tags: Tags::new(),
            content: "test".to_string(),
            mls_group_id: group_id.clone(),
            event: inner_event,
            wrapper_event_id: EventId::all_zeros(),
            epoch: None,
            state: mdk_core::prelude::message_types::MessageState::Processed,
        };
        let result = MessageProcessingResult::ApplicationMessage(message);
        assert_eq!(
            extract_group_id(&result),
            Some(&group_id),
            "ApplicationMessage should return group_id"
        );

        // Commit
        let result = MessageProcessingResult::Commit {
            mls_group_id: group_id.clone(),
        };
        assert_eq!(
            extract_group_id(&result),
            Some(&group_id),
            "Commit should return group_id"
        );

        // PendingProposal
        let result = MessageProcessingResult::PendingProposal {
            mls_group_id: group_id.clone(),
        };
        assert_eq!(
            extract_group_id(&result),
            Some(&group_id),
            "PendingProposal should return group_id"
        );

        // IgnoredProposal
        let result = MessageProcessingResult::IgnoredProposal {
            mls_group_id: group_id.clone(),
            reason: "test reason".to_string(),
        };
        assert_eq!(
            extract_group_id(&result),
            Some(&group_id),
            "IgnoredProposal should return group_id"
        );

        // ExternalJoinProposal
        let result = MessageProcessingResult::ExternalJoinProposal {
            mls_group_id: group_id.clone(),
        };
        assert_eq!(
            extract_group_id(&result),
            Some(&group_id),
            "ExternalJoinProposal should return group_id"
        );

        // Unprocessable
        let result = MessageProcessingResult::Unprocessable {
            mls_group_id: group_id.clone(),
        };
        assert_eq!(
            extract_group_id(&result),
            Some(&group_id),
            "Unprocessable should return group_id"
        );

        // Proposal (auto-committed) - requires constructing UpdateGroupResult with a
        // signed Event; use a throwaway key to produce a valid one.
        let keys = nostr_sdk::Keys::generate();
        let dummy_event = nostr_sdk::EventBuilder::text_note("dummy")
            .sign_with_keys(&keys)
            .unwrap();
        let update_result = UpdateGroupResult {
            evolution_event: dummy_event,
            welcome_rumors: None,
            mls_group_id: group_id.clone(),
        };
        let result = MessageProcessingResult::Proposal(update_result);
        assert_eq!(
            extract_group_id(&result),
            Some(&group_id),
            "Proposal should return group_id"
        );

        // PreviouslyFailed
        let result = MessageProcessingResult::PreviouslyFailed;
        assert_eq!(
            extract_group_id(&result),
            None,
            "PreviouslyFailed should return None"
        );
    }
}
