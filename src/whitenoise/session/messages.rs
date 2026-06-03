use std::sync::Arc;

use cgka_traits::{GroupId as MarmotGroupId, MessageId as MarmotMessageId, StorageError};
use nostr_sdk::prelude::*;
use tokio::sync::Mutex;

use crate::marmot::GroupId;
use crate::marmot::Message as MarmotMessage;
use crate::marmot::MessageState as MarmotMessageState;
use crate::marmot::message::{app_payload_from_unsigned_event, event_id_from_message_id};
use crate::marmot::publish::{MarmotPublishOutcome, MarmotPublishedEffects, publish_effects};
use crate::marmot::session::{MarmotSession, MarmotSessionEffects, PublishWork};
use crate::marmot::transport::{
    MarmotGroupPublishRoute, MarmotPublishRoutes, MarmotRelayControlPublisher,
};
use crate::types::MessageWithTokens;
use crate::whitenoise::accounts_groups::AccountGroup;
use crate::whitenoise::aggregated_message::AggregatedMessage;
use crate::whitenoise::database::aggregated_messages::PaginationOptions;
use crate::whitenoise::database::marmot_messages::MarmotMessageProjection;
use crate::whitenoise::database::{Database, DatabaseError, retry_on_lock};
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::message_aggregator::processor::{
    extract_deletion_target_ids, extract_reaction_target_id,
};
use crate::whitenoise::message_aggregator::{
    ChatMessage, DeliveryStatus, SearchResult, emoji_utils, reaction_handler,
};
use crate::whitenoise::message_streaming::{MessageStreamManager, MessageUpdate, UpdateTrigger};
use crate::whitenoise::session::AccountSession;
use crate::whitenoise::session::push::PushResponseContext;

/// Account-scoped message view. Constructed via [`AccountSession::messages()`].
pub struct MessageOps<'a> {
    session: &'a AccountSession,
}

impl<'a> MessageOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
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
        let pool = &self.session.account_db.inner.pool;
        let visible = AccountGroup::find_visible_for_account(self.pubkey(), pool).await?;
        let visible_group_ids: Vec<Vec<u8>> = visible
            .iter()
            .map(|ag| ag.mls_group_id.as_slice().to_vec())
            .collect();
        Ok(AggregatedMessage::search_messages(
            self.pubkey(),
            query,
            limit_val,
            &visible_group_ids,
            self.db(),
        )
        .await?)
    }

    fn pubkey(&self) -> &PublicKey {
        &self.session.account_pubkey
    }

    fn db(&self) -> &Database {
        &self.session.account_db.inner
    }
}

/// Group-scoped message view. Both account and group identity are baked in.
pub struct MessageOpsForGroup<'a> {
    session: &'a AccountSession,
    group_id: &'a GroupId,
}

struct MarmotPublishTask {
    marmot_session: Arc<Mutex<MarmotSession>>,
    effects: MarmotSessionEffects,
    group_route: MarmotGroupPublishRoute,
    application_message_id: MarmotMessageId,
    event_id: EventId,
    kind: u16,
    tags: Tags,
    author: PublicKey,
    content: String,
}

impl<'a> MessageOpsForGroup<'a> {
    fn pubkey(&self) -> &PublicKey {
        &self.session.account_pubkey
    }

    fn db(&self) -> &Database {
        &self.session.account_db.inner
    }

    fn pool(&self) -> &sqlx::SqlitePool {
        &self.session.account_db.inner.pool
    }

    fn emit(&self, trigger: UpdateTrigger, message: ChatMessage) {
        self.session
            .shared
            .message_stream_manager
            .emit(self.group_id, MessageUpdate { trigger, message });
    }

    fn marmot_group_id(&self) -> MarmotGroupId {
        MarmotGroupId::new(self.group_id.as_slice().to_vec())
    }

    fn has_marmot_group_projection(&self) -> Result<bool> {
        Ok(self
            .session
            .marmot_storage
            .find_group_projection(&self.marmot_group_id())?
            .is_some())
    }

    // ── Read operations ────────────────────────────────────────────

    /// Fetch raw MLS messages with parsed Markdown AST.
    pub async fn fetch(&self) -> Result<Vec<MessageWithTokens>> {
        if self.has_marmot_group_projection()? {
            let messages = MarmotMessageProjection::list_by_group(self.group_id, self.db()).await?;
            return Ok(messages
                .into_iter()
                .map(|message| {
                    let tokens = whitenoise_markdown::parse(&message.content);
                    MessageWithTokens { message, tokens }
                })
                .collect());
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Fetch aggregated messages with cursor-based pagination (oldest-first).
    pub async fn fetch_aggregated(
        &self,
        options: &PaginationOptions,
        limit: Option<u32>,
    ) -> Result<Vec<ChatMessage>> {
        let cleared_at_ms =
            AccountGroup::chat_cleared_at_ms(self.pubkey(), self.group_id, self.pool()).await?;

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
            AccountGroup::find_by_account_and_group(self.pubkey(), self.group_id, self.pool())
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
            AccountGroup::chat_cleared_at_ms(self.pubkey(), self.group_id, self.pool()).await?;

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

        // Validate kind + tag invariants before any protocol or DB writes.
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

        if let Some(result) = self
            .try_send_marmot_message(inner_event.clone(), event_id, kind)
            .await?
        {
            return Ok(result);
        }

        if self.has_marmot_group_projection()? {
            return Err(WhitenoiseError::MarmotSessionUnavailable(*self.pubkey()));
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    async fn try_send_marmot_message(
        &self,
        inner_event: UnsignedEvent,
        event_id: EventId,
        kind: u16,
    ) -> Result<Option<SendResult>> {
        let Some(marmot_session) = self.session.marmot.clone() else {
            return Ok(None);
        };

        let marmot_group_id = MarmotGroupId::new(self.group_id.as_slice().to_vec());
        let payload = app_payload_from_unsigned_event(&inner_event, event_id)?;
        let (effects, group_route, epoch) = {
            let mut session = marmot_session.lock().await;
            let group_route = match session.group_publish_route(&marmot_group_id) {
                Ok(group_route) => group_route,
                Err(error) if is_missing_marmot_group_error(&error) => return Ok(None),
                Err(error) => return Err(error),
            };
            let epoch = session.group_epoch(&marmot_group_id)?;
            let effects = session.send_app_message(marmot_group_id, payload).await?;
            (effects, group_route, epoch)
        };
        let application_message_id = application_message_id_from_effects(&effects)?;
        let wrapper_event_id = event_id_from_marmot_message_id(&application_message_id)?;
        let marmot_message = MarmotMessage::from_unsigned_app_event(
            self.group_id.clone(),
            inner_event,
            wrapper_event_id,
            Some(epoch),
            MarmotMessageState::Created,
        )?;

        let tokens = whitenoise_markdown::parse(&marmot_message.content);

        let mut last_message_deleted = false;
        match kind {
            9 => {
                self.process_and_emit_outgoing_message(&marmot_message)
                    .await?
            }
            7 => {
                self.cache_and_apply_outgoing_reaction(&marmot_message)
                    .await?
            }
            5 => {
                last_message_deleted = self
                    .cache_and_apply_outgoing_deletion(&marmot_message)
                    .await?
            }
            _ => unreachable!("validated above"),
        }

        self.spawn_marmot_publish_task(MarmotPublishTask {
            marmot_session,
            effects,
            group_route,
            application_message_id,
            event_id,
            kind,
            tags: marmot_message.tags.clone(),
            author: marmot_message.pubkey,
            content: marmot_message.content.clone(),
        });

        Ok(Some(SendResult {
            message: MessageWithTokens::new(marmot_message, tokens),
            last_message_deleted,
        }))
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

    // TODO(durable-task-runtime): Move spawn to caller once the durable task
    // runtime owns background publish work.
    fn spawn_marmot_publish_task(&self, task: MarmotPublishTask) {
        let Some(session_operation) = self.session.begin_operation() else {
            tracing::debug!(
                target: "whitenoise::session::messages",
                account = %self.pubkey().to_hex(),
                group_id = %hex::encode(self.group_id.as_slice()),
                "Skipping Marmot publish task because account session is closing"
            );
            return;
        };
        let ephemeral = self.session.ephemeral.clone();
        let account_pubkey = *self.pubkey();
        let shared = self.session.shared.clone();
        let account_db = self.session.account_db.clone();
        let group_id = self.group_id.clone();
        let event_id_str = task.event_id.to_string();
        let push_context = PushResponseContext::from_session(self.session);

        tokio::spawn(async move {
            let _session_operation = session_operation;
            let routes = MarmotPublishRoutes::new().with_group_publish_route(task.group_route);
            let publisher = MarmotRelayControlPublisher::new(&ephemeral, routes);
            let publish_result =
                publish_effects(task.marmot_session, &publisher, task.effects).await;

            match marmot_delivery_status(&task.application_message_id, publish_result) {
                MarmotDeliveryStatus::Sent { accepted_count } => {
                    update_and_emit_delivery_status(
                        &event_id_str,
                        &group_id,
                        &account_pubkey,
                        &DeliveryStatus::Sent(accepted_count),
                        &account_db.inner,
                        &shared.message_stream_manager,
                    )
                    .await;

                    if let Err(e) = push_context
                        .publish_notification_requests_after_delivery(&group_id)
                        .await
                    {
                        tracing::warn!(
                            target: "whitenoise::push_notifications",
                            account = %account_pubkey.to_hex(),
                            error = %e,
                            "Best-effort push notification failed after Marmot delivery"
                        );
                    }
                }
                MarmotDeliveryStatus::Failed { reason } => {
                    update_and_emit_delivery_status(
                        &event_id_str,
                        &group_id,
                        &account_pubkey,
                        &DeliveryStatus::Failed(reason),
                        &account_db.inner,
                        &shared.message_stream_manager,
                    )
                    .await;
                    cascade_delivery_failure(
                        task.kind,
                        &event_id_str,
                        &task.tags,
                        &task.author,
                        &task.content,
                        &group_id,
                        &account_db.inner,
                        &shared.message_stream_manager,
                    )
                    .await;
                }
            }
        });
    }

    async fn process_and_emit_outgoing_message(
        &self,
        marmot_message: &MarmotMessage,
    ) -> Result<()> {
        let media_files = crate::whitenoise::media_files::MediaFile::find_by_group(
            &self.session.account_db.inner.pool,
            &self.session.shared.database,
            self.pubkey(),
            self.group_id,
        )
        .await?;

        let chat_message = self
            .session
            .shared
            .message_aggregator
            .process_single_message(marmot_message, media_files)
            .await
            .map(|mut msg| {
                msg.delivery_status = Some(DeliveryStatus::Sending);
                msg
            })?;

        AggregatedMessage::insert_message(&chat_message, self.group_id, self.pubkey(), self.db())
            .await?;
        MarmotMessageProjection::upsert(marmot_message, self.db()).await?;

        self.emit(UpdateTrigger::NewMessage, chat_message);
        Ok(())
    }

    async fn cache_and_apply_outgoing_reaction(
        &self,
        marmot_message: &MarmotMessage,
    ) -> Result<()> {
        AggregatedMessage::insert_reaction(marmot_message, self.group_id, self.db()).await?;
        MarmotMessageProjection::upsert(marmot_message, self.db()).await?;
        AggregatedMessage::insert_delivery_status(
            &marmot_message.id.to_string(),
            self.group_id,
            self.pubkey(),
            &DeliveryStatus::Sending,
            self.db(),
        )
        .await?;

        if let Ok(target_id) = extract_reaction_target_id(&marmot_message.tags)
            && let Some(mut target) =
                AggregatedMessage::find_by_id(&target_id, self.group_id, self.pubkey(), self.db())
                    .await?
        {
            let emoji = emoji_utils::validate_and_normalize_reaction(
                &marmot_message.content,
                self.session
                    .shared
                    .message_aggregator
                    .config()
                    .normalize_emoji,
            )?;
            reaction_handler::add_reaction_to_message(
                &mut target,
                &marmot_message.pubkey,
                &emoji,
                marmot_message.created_at,
                marmot_message.id,
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
        marmot_message: &MarmotMessage,
    ) -> Result<bool> {
        AggregatedMessage::insert_deletion(marmot_message, self.group_id, self.db()).await?;
        MarmotMessageProjection::upsert(marmot_message, self.db()).await?;
        AggregatedMessage::insert_delivery_status(
            &marmot_message.id.to_string(),
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

        let deletion_id = marmot_message.id.to_string();
        let target_ids = extract_deletion_target_ids(&marmot_message.tags);
        let mut deleted_last_message = false;

        for target_id in &target_ids {
            if let Some(reaction) =
                AggregatedMessage::find_reaction_by_id(target_id, self.group_id, self.db()).await?
            {
                if reaction.author != marmot_message.pubkey {
                    tracing::warn!(
                        target: "whitenoise::session::messages",
                        deletion_author = %marmot_message.pubkey.to_hex(),
                        target_author = %reaction.author.to_hex(),
                        target_id = %target_id,
                        "Ignoring outgoing deletion whose author does not match reaction author"
                    );
                    continue;
                }

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
                AggregatedMessage::mark_deleted(
                    target_id,
                    self.group_id,
                    &deletion_id,
                    self.pubkey(),
                    self.db(),
                )
                .await?;
                continue;
            }

            if let Some(mut msg) =
                AggregatedMessage::find_by_id(target_id, self.group_id, self.pubkey(), self.db())
                    .await?
            {
                if msg.author != marmot_message.pubkey {
                    tracing::warn!(
                        target: "whitenoise::session::messages",
                        deletion_author = %marmot_message.pubkey.to_hex(),
                        target_author = %msg.author.to_hex(),
                        target_id = %target_id,
                        "Ignoring outgoing deletion whose author does not match message author"
                    );
                    continue;
                }

                AggregatedMessage::mark_deleted(
                    target_id,
                    self.group_id,
                    &deletion_id,
                    self.pubkey(),
                    self.db(),
                )
                .await?;

                msg.is_deleted = true;
                deleted_last_message |= last_message_id
                    .as_ref()
                    .is_some_and(|last_message_id| last_message_id == target_id);
                self.emit(UpdateTrigger::MessageDeleted, msg);
            }
        }

        Ok(deleted_last_message)
    }
}

/// Result of a send operation.
pub struct SendResult {
    pub message: MessageWithTokens,
    pub last_message_deleted: bool,
}

enum MarmotDeliveryStatus {
    Sent { accepted_count: usize },
    Failed { reason: String },
}

// ── Free functions ─────────────────────────────────────────────────

fn is_missing_marmot_group_error(error: &WhitenoiseError) -> bool {
    matches!(
        error,
        WhitenoiseError::MarmotEngine(cgka_traits::EngineError::UnknownGroup(_))
            | WhitenoiseError::MarmotStorage(StorageError::NotFound)
    )
}

fn application_message_id_from_effects(effects: &MarmotSessionEffects) -> Result<MarmotMessageId> {
    let mut application_messages = effects.publish.iter().filter_map(|work| match work {
        PublishWork::ApplicationMessage { msg } => Some(msg.id.clone()),
        _ => None,
    });

    match (application_messages.next(), application_messages.next()) {
        (Some(message_id), None) => Ok(message_id),
        (None, _) if !effects.queued.is_empty() => Err(WhitenoiseError::Internal(
            "Marmot app message send was queued; queued outgoing message projection is not implemented"
                .to_string(),
        )),
        (None, _) => Err(WhitenoiseError::Internal(
            "Marmot app message send produced no application transport message".to_string(),
        )),
        (Some(_), Some(_)) => Err(WhitenoiseError::Internal(
            "Marmot app message send produced multiple application transport messages"
                .to_string(),
        )),
    }
}

fn event_id_from_marmot_message_id(message_id: &MarmotMessageId) -> Result<EventId> {
    event_id_from_message_id(message_id)
}

fn marmot_delivery_status(
    message_id: &MarmotMessageId,
    publish_result: Result<MarmotPublishedEffects>,
) -> MarmotDeliveryStatus {
    let published = match publish_result {
        Ok(published) => published,
        Err(error) => {
            return MarmotDeliveryStatus::Failed {
                reason: error.to_string(),
            };
        }
    };

    if let Some(failure) = published
        .failures
        .iter()
        .find(|failure| failure.message_id == *message_id)
        .or_else(|| published.failures.first())
    {
        return MarmotDeliveryStatus::Failed {
            reason: failure.reason.clone(),
        };
    }

    match published
        .reports
        .iter()
        .find(|report| report.message_id == *message_id)
        .map(|report| &report.outcome)
    {
        Some(MarmotPublishOutcome::Published { accepted_count }) => MarmotDeliveryStatus::Sent {
            accepted_count: *accepted_count,
        },
        Some(MarmotPublishOutcome::Failed { reason }) => MarmotDeliveryStatus::Failed {
            reason: reason.clone(),
        },
        None => MarmotDeliveryStatus::Failed {
            reason: "Marmot publish produced no report for application message".to_string(),
        },
    }
}

async fn update_and_emit_delivery_status(
    event_id: &str,
    group_id: &GroupId,
    account_pubkey: &PublicKey,
    status: &DeliveryStatus,
    database: &Database,
    stream_manager: &MessageStreamManager,
) {
    match AggregatedMessage::update_delivery_status_with_retry(
        event_id,
        group_id,
        account_pubkey,
        status,
        database,
    )
    .await
    {
        Ok(updated_msg) => {
            stream_manager.emit(
                group_id,
                MessageUpdate {
                    trigger: UpdateTrigger::DeliveryStatusChanged,
                    message: updated_msg,
                },
            );
        }
        Err(error) => {
            tracing::warn!(
                target: "whitenoise::messages::delivery",
                "Failed to update delivery status for message {}: {}",
                event_id,
                error
            );
        }
    }
}

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
        5 => cascade_deletion_failure(event_id, author, group_id, database, stream_manager).await,
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

    let result: std::result::Result<Option<ChatMessage>, DatabaseError> = retry_on_lock(|| async {
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
    author: &PublicKey,
    group_id: &GroupId,
    database: &Database,
    stream_manager: &MessageStreamManager,
) {
    let target_ids = retry_on_lock(|| async {
        AggregatedMessage::find_deleted_target_ids_by_deletion_event_id(
            event_id, group_id, database,
        )
        .await
    })
    .await;

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

    let target_ids = match target_ids {
        Ok(target_ids) => target_ids,
        Err(e) => {
            tracing::error!(
                target: "whitenoise::messages::delivery",
                "Failed to restore reaction summaries for deletion {event_id} after retries: {e}",
            );
            return;
        }
    };

    // Restore each target. For reactions, re-add them to the parent's summary
    // (the optimistic delete path removed them). For messages, just re-emit.
    for target_id in target_ids {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex, Weak};

    use async_trait::async_trait;
    use cgka_traits::app_components::NostrRoutingV1;
    use cgka_traits::transport::TransportMessage;
    use cgka_traits::types::GroupId as MarmotGroupId;
    use nostr_sdk::{Keys, RelayUrl};

    use crate::marmot::publish::{MarmotMessagePublisher, MarmotPublishOutcome};
    use crate::marmot::{GroupConfig, MarmotCreatedGroupProjection};
    use crate::whitenoise::Whitenoise;
    use crate::whitenoise::error::WhitenoiseError;
    use crate::whitenoise::session::AccountSession;
    use crate::whitenoise::session::test_helpers::{test_db, test_shared};
    use crate::whitenoise::test_utils::insert_test_account;
    use crate::whitenoise::test_utils::{
        assert_obsolete_mls_artifacts_absent, create_mock_whitenoise,
        remove_obsolete_mls_artifacts, wait_for_key_package_publication,
    };

    #[derive(Clone, Default)]
    struct CapturingMarmotPublisher {
        messages: Arc<Mutex<Vec<TransportMessage>>>,
    }

    #[async_trait]
    impl MarmotMessagePublisher for CapturingMarmotPublisher {
        async fn publish(&self, message: TransportMessage) -> MarmotPublishOutcome {
            self.messages.lock().unwrap().push(message);
            MarmotPublishOutcome::Published { accepted_count: 1 }
        }
    }

    #[tokio::test]
    async fn fetch_returns_raw_message_projection_for_darkmatter_group() {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let config = GroupConfig::new(
            "darkmatter messages".to_string(),
            "message projection without obsolete MLS storage".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("wss://darkmatter-message-fetch.example").unwrap()],
            vec![creator_account.pubkey],
            None,
        );
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                config,
                None,
                &CapturingMarmotPublisher::default(),
            )
            .await
            .unwrap();
        let artifacts = remove_obsolete_mls_artifacts(&creator_account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        let sent = creator_session
            .messages()
            .for_group(&group.mls_group_id)
            .send("hello from Darkmatter".to_string(), 9, None)
            .await
            .unwrap();
        let fetched = creator_session
            .messages()
            .for_group(&group.mls_group_id)
            .fetch()
            .await
            .unwrap();

        assert_obsolete_mls_artifacts_absent(&artifacts);
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].message.id, sent.message.message.id);
        assert_eq!(fetched[0].message.content, "hello from Darkmatter");
        assert_eq!(
            fetched[0].message.wrapper_event_id,
            sent.message.message.wrapper_event_id
        );
        assert_eq!(fetched[0].message.epoch, sent.message.message.epoch);
        assert_eq!(fetched[0].tokens, sent.message.tokens);
    }

    #[tokio::test]
    async fn send_to_projected_darkmatter_group_without_live_session_does_not_fall_back_to_obsolete_mls_storage()
     {
        Whitenoise::initialize_mock_keyring_store();

        let keys = Keys::generate();
        let pubkey = keys.public_key();
        let db = test_db().await;
        insert_test_account(&db, &pubkey).await;

        let shared = test_shared(db).await;
        let session = AccountSession::new(pubkey, shared, Weak::new(), Some(Arc::new(keys)), None)
            .await
            .unwrap();

        let marmot_group_id = MarmotGroupId::new(vec![7; 32]);
        let group_id = crate::marmot::GroupId::from(&marmot_group_id);
        session
            .marmot_storage
            .put_group_projection(&MarmotCreatedGroupProjection {
                group_id: marmot_group_id,
                name: "projected".to_string(),
                description: "projected without live session".to_string(),
                epoch: 1,
                routing: NostrRoutingV1::new(
                    [9; 32],
                    vec!["wss://projected-darkmatter.example".to_string()],
                )
                .unwrap(),
                admin_pubkeys: BTreeSet::from([pubkey]),
                member_pubkeys: BTreeSet::from([pubkey]),
                self_update_completed_at_secs: 1,
                disappearing_message_secs: None,
            })
            .unwrap();

        let result = session
            .messages()
            .for_group(&group_id)
            .send("must use Darkmatter".to_string(), 9, None)
            .await;

        assert!(matches!(
            result,
            Err(WhitenoiseError::MarmotSessionUnavailable(account_pubkey))
                if account_pubkey == pubkey
        ));
    }
}
