use std::sync::Arc;

use mdk_core::prelude::*;
use nostr_sdk::prelude::*;

use crate::nostr_manager::parser::ContentParser;
use crate::types::MessageWithTokens;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::aggregated_message::AggregatedMessage;
use crate::whitenoise::database::aggregated_messages::PaginationOptions;
use crate::whitenoise::database::{Database, retry_on_lock};
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::message_aggregator::processor::{
    extract_deletion_target_ids, extract_reaction_target_id,
};
use crate::whitenoise::message_aggregator::{
    ChatMessage, DeliveryStatus, MessageAggregator, SearchResult, emoji_utils, reaction_handler,
};
use crate::whitenoise::message_streaming::{MessageStreamManager, MessageUpdate, UpdateTrigger};
use crate::whitenoise::push_notifications::publish_notification_requests_after_delivery_with;
use crate::whitenoise::session::AccountSession;

/// Account-scoped message view. Constructed via [`AccountSession::messages()`].
pub struct MessageOps<'a> {
    session: &'a AccountSession,
}

impl<'a> MessageOps<'a> {
    pub(crate) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    /// Narrow to a specific group.
    pub fn for_group(&self, group_id: &'a GroupId) -> MessageOpsForGroup<'a> {
        MessageOpsForGroup {
            session: self.session,
            group_id,
        }
    }

    /// Search messages across all groups.
    pub async fn search(&self, query: &str, limit: Option<u32>) -> Result<Vec<SearchResult>> {
        let limit_val = limit.unwrap_or(50);
        Ok(AggregatedMessage::search_messages(self.pubkey(), query, limit_val, self.db()).await?)
    }

    fn pubkey(&self) -> &PublicKey {
        &self.session.account_pubkey
    }

    fn db(&self) -> &Arc<Database> {
        &self.session.database
    }
}

/// Group-scoped message view. Both account and group identity are baked in.
pub struct MessageOpsForGroup<'a> {
    session: &'a AccountSession,
    group_id: &'a GroupId,
}

impl<'a> MessageOpsForGroup<'a> {
    fn pubkey(&self) -> &PublicKey {
        &self.session.account_pubkey
    }

    fn db(&self) -> &Arc<Database> {
        &self.session.database
    }

    fn emit(&self, trigger: UpdateTrigger, message: ChatMessage) {
        self.session
            .message_stream_manager
            .emit(self.group_id, MessageUpdate { trigger, message });
    }

    // ── Read operations ────────────────────────────────────────────

    /// Fetch raw MLS messages from MDK storage with parsed content tokens.
    pub async fn fetch(&self) -> Result<Vec<MessageWithTokens>> {
        let parser = ContentParser::new();
        let messages = self.session.mdk.get_messages(self.group_id, None)?;
        Ok(messages
            .iter()
            .map(|m| MessageWithTokens {
                message: m.clone(),
                tokens: parser.parse(&m.content),
            })
            .collect())
    }

    /// Fetch aggregated messages with cursor-based pagination (oldest-first).
    pub async fn fetch_aggregated(
        &self,
        options: &PaginationOptions,
        limit: Option<u32>,
    ) -> Result<Vec<ChatMessage>> {
        let cleared_at_ms =
            AccountGroup::chat_cleared_at_ms(self.pubkey(), self.group_id, self.db()).await?;

        AggregatedMessage::find_messages_by_group_paginated(
            self.group_id,
            self.pubkey(),
            self.db(),
            options,
            limit,
            cleared_at_ms,
        )
        .await
        .map_err(|e| match e {
            crate::whitenoise::database::DatabaseError::InvalidCursor { reason } => {
                WhitenoiseError::InvalidCursor { reason }
            }
            other => {
                WhitenoiseError::Internal(format!("Failed to read cached messages: {}", other))
            }
        })
    }

    /// Fetch newest messages ensuring all unreads are included, with a minimum floor.
    pub async fn fetch_unread_with_minimum(
        &self,
        minimum: Option<u32>,
    ) -> Result<Vec<ChatMessage>> {
        let account_group =
            AccountGroup::find_by_account_and_group(self.pubkey(), self.group_id, self.db())
                .await?
                .ok_or(WhitenoiseError::GroupNotFound)?;

        Ok(AggregatedMessage::find_messages_with_unread_minimum(
            self.group_id,
            self.pubkey(),
            account_group.last_read_message_id.as_ref(),
            minimum.unwrap_or(50),
            self.db(),
            account_group
                .chat_cleared_at
                .map(|dt| dt.timestamp_millis()),
        )
        .await?)
    }

    /// Fetch a single aggregated message by event ID.
    pub async fn fetch_by_id(&self, message_id: &str) -> Result<Option<ChatMessage>> {
        Ok(
            AggregatedMessage::find_by_id(message_id, self.group_id, self.pubkey(), self.db())
                .await?,
        )
    }

    /// Search messages by content within this group.
    pub async fn search(&self, query: &str, limit: Option<u32>) -> Result<Vec<SearchResult>> {
        let cleared_at_ms =
            AccountGroup::chat_cleared_at_ms(self.pubkey(), self.group_id, self.db()).await?;

        Ok(AggregatedMessage::search_messages_in_group(
            self.group_id,
            self.pubkey(),
            query,
            limit.unwrap_or(50),
            self.db(),
            cleared_at_ms,
        )
        .await?)
    }

    // ── Write operations ───────────────────────────────────────────

    /// Send a message (chat, reaction, or deletion) to this group.
    ///
    /// Returns the sent message and whether the last group message was deleted.
    pub async fn send(
        &self,
        message: String,
        kind: u16,
        tags: Option<Vec<Tag>>,
    ) -> Result<SendResult> {
        let tags_vec = tags.unwrap_or_default();

        // Validate kind + tag invariants before any MDK/DB writes.
        // Reactions require an e-tag (target message), deletions require at
        // least one e-tag (target messages/reactions).
        match kind {
            9 => {}
            7 => {
                if !tags_vec.iter().any(|t| t.kind() == TagKind::e()) {
                    return Err(WhitenoiseError::Internal(
                        "Reaction event missing e-tag".to_string(),
                    ));
                }
            }
            5 => {
                if !tags_vec.iter().any(|t| t.kind() == TagKind::e()) {
                    return Err(WhitenoiseError::Internal(
                        "Deletion event requires at least one e-tag".to_string(),
                    ));
                }
            }
            other => {
                return Err(WhitenoiseError::Internal(format!(
                    "Unsupported outgoing event kind: {other}"
                )));
            }
        }

        let (inner_event, event_id) =
            create_unsigned_nostr_event(self.pubkey(), &message, kind, Some(tags_vec))?;

        let mdk = &self.session.mdk;

        let group_relays: Vec<RelayUrl> = mdk.get_relays(self.group_id)?.into_iter().collect();
        if group_relays.is_empty() {
            return Err(WhitenoiseError::GroupMissingRelays);
        }

        let message_event = mdk.create_message(self.group_id, inner_event, None)?;
        let mdk_message =
            mdk.get_message(self.group_id, &event_id)?
                .ok_or(WhitenoiseError::MdkCoreError(
                    mdk_core::error::Error::MessageNotFound,
                ))?;

        let tokens = ContentParser::new().parse(&mdk_message.content);

        let mut last_message_deleted = false;
        match kind {
            9 => self.process_and_emit_outgoing_message(&mdk_message).await?,
            7 => self.cache_and_apply_outgoing_reaction(&mdk_message).await?,
            5 => {
                last_message_deleted = self.cache_and_apply_outgoing_deletion(&mdk_message).await?
            }
            _ => unreachable!("validated above"),
        }

        self.spawn_publish_task(message_event, group_relays, event_id, kind, &mdk_message);

        Ok(SendResult {
            message: MessageWithTokens::new(mdk_message, tokens),
            last_message_deleted,
        })
    }

    /// Retry publishing a failed message.
    pub async fn retry_publish(&self, event_id: &EventId) -> Result<SendResult> {
        let event_id_str = event_id.to_string();
        let original = match AggregatedMessage::find_by_id(
            &event_id_str,
            self.group_id,
            self.pubkey(),
            self.db(),
        )
        .await?
        {
            Some(c) if matches!(c.delivery_status, Some(DeliveryStatus::Failed(_))) => c,
            Some(c) => {
                return Err(WhitenoiseError::Internal(format!(
                    "Can only retry messages with Failed delivery status, got {:?}",
                    c.delivery_status
                )));
            }
            None => return Err(WhitenoiseError::MessageNotFound),
        };

        let tags = original.tags.to_vec();
        let result = self
            .send(original.content, original.kind, Some(tags))
            .await?;

        // Mark original as Retried (best-effort)
        match AggregatedMessage::update_delivery_status_with_retry(
            &event_id_str,
            self.group_id,
            self.pubkey(),
            &DeliveryStatus::Retried,
            self.db(),
        )
        .await
        {
            Ok(updated) => self.emit(UpdateTrigger::DeliveryStatusChanged, updated),
            Err(e) => tracing::warn!(
                target: "whitenoise::messages::delivery",
                "Failed to mark original message {} as Retried: {e}",
                event_id_str,
            ),
        }

        Ok(result)
    }

    // ── Private helpers ────────────────────────────────────────────

    // TODO(durable-task-runtime): Move spawn to caller. Views should not call
    // tokio::spawn internally per the session-projection architecture rules.
    // Blocked on the durable task runtime that replaces fire-and-forget publishing.
    fn spawn_publish_task(
        &self,
        message_event: Event,
        group_relays: Vec<RelayUrl>,
        event_id: EventId,
        kind: u16,
        mdk_message: &mdk_core::prelude::message_types::Message,
    ) {
        let ephemeral = self.session.ephemeral.clone_inner();
        let account_pubkey = *self.pubkey();
        let database = self.session.database.clone();
        let stream_manager = self.session.message_stream_manager.clone();
        let group_id = self.group_id.clone();
        let event_id_str = event_id.to_string();
        let tags = mdk_message.tags.clone();
        let author = mdk_message.pubkey;
        let content = mdk_message.content.clone();

        // TODO: Push config and relay sync context should live on AccountSession
        // instead of reaching back to the singleton. Tracked for Phase 16b.
        let (push_config, push_relay_sync) = match crate::whitenoise::Whitenoise::get_instance() {
            Ok(wn) => (Some(wn.config.clone()), Some(wn.user_relay_sync_context())),
            Err(_) => {
                tracing::debug!(
                    target: "whitenoise::messages",
                    "Whitenoise singleton unavailable, push notifications skipped"
                );
                (None, None)
            }
        };

        tokio::spawn(async move {
            let ok = ephemeral
                .publish_message_event(
                    message_event,
                    &account_pubkey,
                    &group_relays,
                    &event_id_str,
                    &group_id,
                    &database,
                    &stream_manager,
                )
                .await
                .succeeded();

            if ok {
                if let Some(config) = &push_config
                    && let Some(sync) = &push_relay_sync
                    && let Err(e) = publish_notification_requests_after_delivery_with(
                        config,
                        &database,
                        sync,
                        &ephemeral,
                        account_pubkey,
                        &group_id,
                    )
                    .await
                {
                    tracing::warn!(
                        target: "whitenoise::push_notifications",
                        account = %account_pubkey.to_hex(),
                        error = %e,
                        "Best-effort push notification failed after delivery"
                    );
                }
            } else {
                cascade_delivery_failure(
                    kind,
                    &event_id_str,
                    &tags,
                    &author,
                    &content,
                    &group_id,
                    &database,
                    &stream_manager,
                )
                .await;
            }
        });
    }

    async fn process_and_emit_outgoing_message(
        &self,
        mdk_message: &mdk_core::prelude::message_types::Message,
    ) -> Result<()> {
        let media_files =
            crate::whitenoise::media_files::MediaFile::find_by_group(self.db(), self.group_id)
                .await?;

        let aggregator = MessageAggregator::with_config(self.session.aggregator_config.clone());
        let chat_message = aggregator
            .process_single_message(mdk_message, &ContentParser::new(), media_files)
            .await
            .map(|mut msg| {
                msg.delivery_status = Some(DeliveryStatus::Sending);
                msg
            })?;

        AggregatedMessage::insert_message(&chat_message, self.group_id, self.pubkey(), self.db())
            .await?;

        self.emit(UpdateTrigger::NewMessage, chat_message);
        Ok(())
    }

    async fn cache_and_apply_outgoing_reaction(
        &self,
        mdk_message: &mdk_core::prelude::message_types::Message,
    ) -> Result<()> {
        AggregatedMessage::insert_reaction(mdk_message, self.group_id, self.db()).await?;
        AggregatedMessage::insert_delivery_status(
            &mdk_message.id.to_string(),
            self.group_id,
            self.pubkey(),
            &DeliveryStatus::Sending,
            self.db(),
        )
        .await?;

        if let Ok(target_id) = extract_reaction_target_id(&mdk_message.tags)
            && let Some(mut target) =
                AggregatedMessage::find_by_id(&target_id, self.group_id, self.pubkey(), self.db())
                    .await?
        {
            let emoji = emoji_utils::validate_and_normalize_reaction(
                &mdk_message.content,
                self.session.aggregator_config.normalize_emoji,
            )?;
            reaction_handler::add_reaction_to_message(
                &mut target,
                &mdk_message.pubkey,
                &emoji,
                mdk_message.created_at,
                mdk_message.id,
            );
            AggregatedMessage::update_reactions(
                &target.id,
                self.group_id,
                &target.reactions,
                self.db(),
            )
            .await?;
            self.emit(UpdateTrigger::ReactionAdded, target);
        }

        Ok(())
    }

    /// Returns `true` if the deletion removed the last message in the group.
    async fn cache_and_apply_outgoing_deletion(
        &self,
        mdk_message: &mdk_core::prelude::message_types::Message,
    ) -> Result<bool> {
        AggregatedMessage::insert_deletion(mdk_message, self.group_id, self.db()).await?;
        AggregatedMessage::insert_delivery_status(
            &mdk_message.id.to_string(),
            self.group_id,
            self.pubkey(),
            &DeliveryStatus::Sending,
            self.db(),
        )
        .await?;

        let last_message_id = AggregatedMessage::find_last_by_group_ids(
            std::slice::from_ref(self.group_id),
            self.db(),
        )
        .await
        .ok()
        .and_then(|v| v.into_iter().next())
        .map(|s| s.message_id.to_hex());

        let deletion_id = mdk_message.id.to_string();
        let target_ids = extract_deletion_target_ids(&mdk_message.tags);

        for target_id in &target_ids {
            if let Some(reaction) =
                AggregatedMessage::find_reaction_by_id(target_id, self.group_id, self.db()).await?
            {
                if let Ok(parent_id) = extract_reaction_target_id(&reaction.tags)
                    && let Some(mut parent) = AggregatedMessage::find_by_id(
                        &parent_id,
                        self.group_id,
                        self.pubkey(),
                        self.db(),
                    )
                    .await?
                    && reaction_handler::remove_reaction_from_message(&mut parent, &reaction.author)
                {
                    AggregatedMessage::update_reactions(
                        &parent_id,
                        self.group_id,
                        &parent.reactions,
                        self.db(),
                    )
                    .await?;
                    self.emit(UpdateTrigger::ReactionRemoved, parent);
                }
                AggregatedMessage::mark_deleted(target_id, self.group_id, &deletion_id, self.db())
                    .await?;
                continue;
            }

            AggregatedMessage::mark_deleted(target_id, self.group_id, &deletion_id, self.db())
                .await?;

            if let Some(mut msg) =
                AggregatedMessage::find_by_id(target_id, self.group_id, self.pubkey(), self.db())
                    .await?
            {
                msg.is_deleted = true;
                self.emit(UpdateTrigger::MessageDeleted, msg);
            }
        }

        Ok(last_message_id.is_some_and(|id| target_ids.contains(&id)))
    }
}

/// Result of a send operation.
pub struct SendResult {
    pub message: MessageWithTokens,
    pub last_message_deleted: bool,
}

// ── Free functions ─────────────────────────────────────────────────

pub(crate) fn create_unsigned_nostr_event(
    pubkey: &PublicKey,
    message: &str,
    kind: u16,
    tags: Option<Vec<Tag>>,
) -> Result<(UnsignedEvent, EventId)> {
    let mut inner_event = UnsignedEvent::new(
        *pubkey,
        Timestamp::now(),
        kind.into(),
        tags.unwrap_or_default(),
        message,
    );
    inner_event.ensure_id();
    let event_id = inner_event.id.unwrap();
    Ok((inner_event, event_id))
}

/// Reverse optimistic aggregated effects when a reaction/deletion fails to publish.
pub(crate) async fn cascade_delivery_failure(
    kind: u16,
    event_id: &str,
    tags: &Tags,
    author: &PublicKey,
    content: &str,
    group_id: &GroupId,
    database: &Database,
    stream_manager: &MessageStreamManager,
) {
    match kind {
        7 => {
            cascade_reaction_failure(
                event_id,
                tags,
                author,
                content,
                group_id,
                database,
                stream_manager,
            )
            .await
        }
        5 => {
            cascade_deletion_failure(event_id, tags, author, group_id, database, stream_manager)
                .await
        }
        _ => {}
    }
}

async fn cascade_reaction_failure(
    _event_id: &str,
    tags: &Tags,
    author: &PublicKey,
    content: &str,
    group_id: &GroupId,
    database: &Database,
    stream_manager: &MessageStreamManager,
) {
    let target_id = match extract_reaction_target_id(tags) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(
                target: "whitenoise::messages::delivery",
                "Failed to extract reaction target for cascade: {e}",
            );
            return;
        }
    };

    let result = retry_on_lock(|| async {
        let Some(mut parent) =
            AggregatedMessage::find_by_id(&target_id, group_id, author, database).await?
        else {
            return Ok(None);
        };
        if !reaction_handler::remove_reaction_from_message(&mut parent, author) {
            return Ok(None);
        }
        AggregatedMessage::update_reactions(&target_id, group_id, &parent.reactions, database)
            .await?;
        Ok(Some(parent))
    })
    .await;

    match result {
        Ok(Some(parent)) => {
            stream_manager.emit(
                group_id,
                MessageUpdate {
                    trigger: UpdateTrigger::ReactionRemoved,
                    message: parent,
                },
            );
            tracing::info!(
                target: "whitenoise::messages::delivery",
                "Cascaded reaction failure: removed '{content}' from {target_id}",
            );
        }
        Ok(None) => {}
        Err(e) => tracing::error!(
            target: "whitenoise::messages::delivery",
            "Failed to cascade reaction failure after retries: {e}",
        ),
    }
}

async fn cascade_deletion_failure(
    event_id: &str,
    tags: &Tags,
    author: &PublicKey,
    group_id: &GroupId,
    database: &Database,
    stream_manager: &MessageStreamManager,
) {
    if let Err(e) = retry_on_lock(|| async {
        AggregatedMessage::unmark_deleted(event_id, group_id, database).await
    })
    .await
    {
        tracing::error!(
            target: "whitenoise::messages::delivery",
            "Failed to cascade deletion failure after retries: {e}",
        );
        return;
    }

    // Restore each target. For reactions, re-add them to the parent's summary
    // (the optimistic delete path removed them). For messages, just re-emit.
    for target_id in extract_deletion_target_ids(tags) {
        // Check if the restored target is a reaction — if so, re-attach to parent
        if let Ok(Some(reaction)) =
            AggregatedMessage::find_reaction_by_id(&target_id, group_id, database).await
        {
            if let Ok(parent_id) = extract_reaction_target_id(&reaction.tags)
                && let Ok(Some(mut parent)) =
                    AggregatedMessage::find_by_id(&parent_id, group_id, author, database).await
            {
                reaction_handler::add_reaction_to_message(
                    &mut parent,
                    &reaction.author,
                    &reaction.content,
                    Timestamp::from(reaction.created_at.timestamp() as u64),
                    reaction.event_id,
                );
                if let Err(e) = AggregatedMessage::update_reactions(
                    &parent_id,
                    group_id,
                    &parent.reactions,
                    database,
                )
                .await
                {
                    tracing::error!(
                        target: "whitenoise::messages::delivery",
                        "Failed to restore reaction on parent {parent_id}: {e}",
                    );
                } else {
                    stream_manager.emit(
                        group_id,
                        MessageUpdate {
                            trigger: UpdateTrigger::ReactionAdded,
                            message: parent,
                        },
                    );
                }
            }
            continue;
        }

        // Regular message target — just re-emit so UI reflects it as not deleted
        if let Ok(Some(msg)) =
            AggregatedMessage::find_by_id(&target_id, group_id, author, database).await
        {
            stream_manager.emit(
                group_id,
                MessageUpdate {
                    trigger: UpdateTrigger::DeliveryStatusChanged,
                    message: msg,
                },
            );
        }
    }

    tracing::info!(
        target: "whitenoise::messages::delivery",
        "Cascaded deletion failure: unmarked targets of deletion {event_id}",
    );
}
