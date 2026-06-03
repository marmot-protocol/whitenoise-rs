use std::sync::Arc;

use cgka_traits::{
    TransportEndpoint,
    engine::GroupEvent,
    ingest::{IngestOutcome, StaleReason},
    transport::{TransportEnvelope, TransportMessage},
    types::{GroupId as MarmotGroupId, MemberId},
};
use nostr_sdk::prelude::*;
use transport_nostr_peeler::NostrTransportEvent;

use crate::marmot::{
    GroupId, Message as MarmotMessage, MessageState,
    publish::{
        MarmotMessagePublisher, MarmotPendingResolution, MarmotPublishedEffects, publish_effects,
    },
    session::{MarmotIngestEffects, MarmotSessionEffects, PublishWork},
    transport::{MarmotPublishRoutes, MarmotRelayControlPublisher},
};
#[cfg(test)]
use crate::whitenoise::database::aggregated_messages::PaginationOptions;
use crate::{
    perf_instrument,
    whitenoise::{
        Whitenoise,
        accounts::Account,
        accounts_groups::AccountGroup,
        aggregated_message::AggregatedMessage,
        chat_list_streaming::ChatListUpdateTrigger,
        database::marmot_messages::MarmotMessageProjection,
        error::{Result, WhitenoiseError},
        media_files::{MediaFile, MediaFiles},
        message_aggregator::processor::{extract_deletion_target_ids, extract_reaction_target_id},
        message_aggregator::{ChatMessage, emoji_utils, reaction_handler},
        message_streaming::{MessageUpdate, UpdateTrigger},
        push_notifications::is_push_group_message_kind,
        relays::Relay,
        session::AccountSession,
        users::User,
    },
};
fn marmot_transport_message_from_event(event: &Event) -> Result<TransportMessage> {
    let transport_event = NostrTransportEvent::from_nostr_event(event).map_err(|error| {
        WhitenoiseError::InvalidEvent(format!("invalid Darkmatter Nostr event: {error}"))
    })?;
    transport_event.to_transport_message().map_err(|error| {
        WhitenoiseError::InvalidEvent(format!("invalid Darkmatter transport event: {error}"))
    })
}

enum MarmotMlsIngestDecision {
    Processed(MarmotIngestEffects),
    Ignored,
}

fn ignore_darkmatter_route_miss(event_id: EventId, reason: &str) -> MarmotMlsIngestDecision {
    tracing::debug!(
        target: "whitenoise::event_processor::handle_mls_message",
        event_id = %event_id.to_hex(),
        reason,
        "Ignoring Darkmatter MLS message with no local route"
    );
    MarmotMlsIngestDecision::Ignored
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

        self.try_handle_marmot_mls_message(session, account, &event)
            .await?;
        Ok(())
    }

    async fn try_handle_marmot_mls_message(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        event: &Event,
    ) -> Result<()> {
        let ingested = match self.ingest_marmot_mls_message(session, event).await? {
            MarmotMlsIngestDecision::Processed(ingested) => ingested,
            MarmotMlsIngestDecision::Ignored => return Ok(()),
        };

        let routes = self
            .marmot_publish_routes_for_session_effects(session, account, &ingested.effects)
            .await?;
        let publisher = MarmotRelayControlPublisher::new(&session.ephemeral, routes);
        self.finish_marmot_mls_message_with_publisher(
            session, account, event.id, ingested, &publisher,
        )
        .await?;

        Ok(())
    }

    #[cfg(test)]
    async fn try_handle_marmot_mls_message_with_publisher<P>(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        event: &Event,
        publisher: &P,
    ) -> Result<()>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        let ingested = match self.ingest_marmot_mls_message(session, event).await? {
            MarmotMlsIngestDecision::Processed(ingested) => ingested,
            MarmotMlsIngestDecision::Ignored => return Ok(()),
        };

        self.finish_marmot_mls_message_with_publisher(
            session, account, event.id, ingested, publisher,
        )
        .await?;

        Ok(())
    }

    async fn ingest_marmot_mls_message(
        &self,
        session: &Arc<AccountSession>,
        event: &Event,
    ) -> Result<MarmotMlsIngestDecision> {
        let transport_message = match marmot_transport_message_from_event(event) {
            Ok(message) => message,
            Err(error) => {
                tracing::debug!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    event_id = %event.id.to_hex(),
                    error = %error,
                    "Event is not a Darkmatter Nostr transport message"
                );
                return Err(error);
            }
        };
        let marmot_session =
            session
                .marmot
                .clone()
                .ok_or(WhitenoiseError::MarmotSessionUnavailable(
                    session.account_pubkey,
                ))?;

        let ingested = {
            let mut marmot = marmot_session.lock().await;
            if !marmot.recognizes_transport_message(&transport_message)? {
                return Ok(ignore_darkmatter_route_miss(
                    event.id,
                    "unrecognized transport route",
                ));
            }
            marmot.ingest(transport_message).await?
        };

        match &ingested.outcome {
            IngestOutcome::Stale {
                reason:
                    reason @ (StaleReason::UnknownGroup
                    | StaleReason::PeelFailed
                    | StaleReason::NotForThisClient),
            } => {
                return Ok(ignore_darkmatter_route_miss(
                    event.id,
                    &format!("{reason:?}"),
                ));
            }
            IngestOutcome::Stale { reason } => {
                tracing::debug!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    event_id = %event.id.to_hex(),
                    reason = ?reason,
                    "Darkmatter classified MLS message as stale"
                );
            }
            IngestOutcome::Buffered { group_id, epoch } => {
                tracing::debug!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    event_id = %event.id.to_hex(),
                    group_id = %hex::encode(group_id.as_slice()),
                    epoch = epoch.0,
                    "Darkmatter buffered MLS message until the group returns to stable"
                );
            }
            IngestOutcome::Processed => {}
        }

        Ok(MarmotMlsIngestDecision::Processed(ingested))
    }

    async fn finish_marmot_mls_message_with_publisher<P>(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        wrapper_event_id: EventId,
        ingested: MarmotIngestEffects,
        publisher: &P,
    ) -> Result<()>
    where
        P: MarmotMessagePublisher + Sync + ?Sized,
    {
        let marmot_session =
            session
                .marmot
                .clone()
                .ok_or(WhitenoiseError::MarmotSessionUnavailable(
                    session.account_pubkey,
                ))?;
        let published = publish_effects(marmot_session, publisher, ingested.effects).await?;
        Self::ensure_marmot_inbound_publish_succeeded(&published)?;

        for failure in &published.failures {
            tracing::warn!(
                target: "whitenoise::event_processor::handle_mls_message",
                account = %account.pubkey.to_hex(),
                message_id = %hex::encode(failure.message_id.as_slice()),
                reason = %failure.reason,
                "Darkmatter inbound auxiliary publish failed after confirmed state transition"
            );
        }

        for event_effect in published.events {
            self.handle_marmot_group_event(session, account, wrapper_event_id, event_effect)
                .await?;
        }

        Ok(())
    }

    pub(crate) fn ensure_marmot_inbound_publish_succeeded(
        published: &MarmotPublishedEffects,
    ) -> Result<()> {
        if published
            .pending
            .iter()
            .any(|resolution| matches!(resolution, MarmotPendingResolution::RolledBack { .. }))
        {
            let reason = published
                .failures
                .first()
                .map(|failure| failure.reason.clone())
                .unwrap_or_else(|| "Darkmatter inbound pending state was rolled back".to_string());
            return Err(WhitenoiseError::MarmotPublishFailed(reason));
        }

        if published.pending.is_empty()
            && let Some(failure) = published.failures.first()
        {
            return Err(WhitenoiseError::MarmotPublishFailed(failure.reason.clone()));
        }

        Ok(())
    }

    pub(crate) async fn marmot_publish_routes_for_session_effects(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        effects: &MarmotSessionEffects,
    ) -> Result<MarmotPublishRoutes> {
        let mut transport_group_ids = Vec::new();
        let mut inbox_recipients = Vec::new();
        for work in &effects.publish {
            Self::collect_marmot_publish_work_routes(
                work,
                &mut transport_group_ids,
                &mut inbox_recipients,
            );
        }

        let mut routes = MarmotPublishRoutes::new();
        if !transport_group_ids.is_empty() {
            let marmot_session =
                session
                    .marmot
                    .clone()
                    .ok_or(WhitenoiseError::MarmotSessionUnavailable(
                        session.account_pubkey,
                    ))?;
            let session = marmot_session.lock().await;
            for transport_group_id in transport_group_ids {
                routes = routes.with_group_publish_route(
                    session.group_publish_route_for_transport_group_id(&transport_group_id)?,
                );
            }
        }

        for recipient in inbox_recipients {
            let member_pubkey = PublicKey::from_slice(recipient.as_slice()).map_err(|error| {
                WhitenoiseError::InvalidInput(format!(
                    "invalid Marmot welcome recipient identity: {error}"
                ))
            })?;
            let (user, _) =
                User::find_or_create_by_pubkey(&member_pubkey, &self.shared.database).await?;
            let relays = self
                .shared
                .resolve_member_delivery_relays(
                    &user,
                    account,
                    "whitenoise::event_processor::handle_mls_message::marmot_inbound",
                )
                .await?;
            let endpoints = Relay::urls(&relays)
                .into_iter()
                .map(|relay| TransportEndpoint(relay.to_string()))
                .collect();
            routes = routes.with_inbox_route(recipient, endpoints);
        }

        Ok(routes)
    }

    fn collect_marmot_publish_work_routes(
        work: &PublishWork,
        transport_group_ids: &mut Vec<Vec<u8>>,
        inbox_recipients: &mut Vec<MemberId>,
    ) {
        match work {
            PublishWork::ApplicationMessage { msg }
            | PublishWork::Proposal { msg }
            | PublishWork::AutoPublish { msg, .. } => {
                Self::collect_marmot_message_route(msg, transport_group_ids, inbox_recipients);
            }
            PublishWork::GroupEvolution { msg, welcomes, .. } => {
                Self::collect_marmot_message_route(msg, transport_group_ids, inbox_recipients);
                for welcome in welcomes {
                    Self::collect_marmot_message_route(
                        welcome,
                        transport_group_ids,
                        inbox_recipients,
                    );
                }
            }
            PublishWork::GroupCreated { welcomes, .. } => {
                for welcome in welcomes {
                    Self::collect_marmot_message_route(
                        welcome,
                        transport_group_ids,
                        inbox_recipients,
                    );
                }
            }
        }
    }

    fn collect_marmot_message_route(
        message: &TransportMessage,
        transport_group_ids: &mut Vec<Vec<u8>>,
        inbox_recipients: &mut Vec<MemberId>,
    ) {
        match &message.envelope {
            TransportEnvelope::GroupMessage { transport_group_id } => {
                if !transport_group_ids.contains(transport_group_id) {
                    transport_group_ids.push(transport_group_id.clone());
                }
            }
            TransportEnvelope::Welcome { recipient } => {
                if !inbox_recipients.contains(recipient) {
                    inbox_recipients.push(recipient.clone());
                }
            }
        }
    }

    pub(crate) async fn handle_marmot_group_event(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        wrapper_event_id: EventId,
        event: GroupEvent,
    ) -> Result<()> {
        match event {
            GroupEvent::MessageReceived {
                group_id,
                sender,
                payload,
            } => {
                self.handle_marmot_application_message(
                    session,
                    account,
                    group_id,
                    sender,
                    payload,
                    wrapper_event_id,
                )
                .await
            }
            GroupEvent::MemberAdded { group_id, .. }
            | GroupEvent::EpochChanged { group_id, .. }
            | GroupEvent::ForkRecovered { group_id, .. } => {
                self.refresh_marmot_group_projection_after_event(session, account, &group_id)
                    .await
            }
            GroupEvent::MemberRemoved { group_id, member } => {
                self.handle_marmot_member_removed(session, account, group_id, member)
                    .await
            }
            GroupEvent::GroupCreated { group_id } | GroupEvent::GroupJoined { group_id, .. } => {
                self.refresh_marmot_group_projection_after_event(session, account, &group_id)
                    .await
            }
            GroupEvent::AppMessageInvalidated {
                group_id,
                message_id,
                ..
            } => {
                tracing::debug!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    group_id = %hex::encode(group_id.as_slice()),
                    message_id = %hex::encode(message_id.as_slice()),
                    "Darkmatter app-message invalidation has no WhiteNoise projection yet"
                );
                Ok(())
            }
            GroupEvent::GroupUnrecoverable { group_id } => {
                tracing::warn!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    account = %account.pubkey.to_hex(),
                    group_id = %hex::encode(group_id.as_slice()),
                    "Darkmatter group became unrecoverable"
                );
                Ok(())
            }
        }
    }

    async fn handle_marmot_member_removed(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        group_id: MarmotGroupId,
        member: MemberId,
    ) -> Result<()> {
        let legacy_group_id = GroupId::from_slice(group_id.as_slice());
        let local_member = {
            let marmot_session =
                session
                    .marmot
                    .clone()
                    .ok_or(WhitenoiseError::MarmotSessionUnavailable(
                        session.account_pubkey,
                    ))?;
            let session = marmot_session.lock().await;
            session.self_id()
        };
        if member == local_member {
            session
                .membership()
                .for_group(&legacy_group_id)
                .mark_as_removed()
                .await?;
        }

        self.refresh_marmot_group_projection_after_event(session, account, &group_id)
            .await
    }

    async fn refresh_marmot_group_projection_after_event(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        group_id: &MarmotGroupId,
    ) -> Result<()> {
        let legacy_group_id = GroupId::from_slice(group_id.as_slice());
        let has_account_group = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &legacy_group_id,
            &session.account_db.inner.pool,
        )
        .await?
        .is_some();

        if !has_account_group {
            tracing::info!(
                target: "whitenoise::event_processor::handle_mls_message",
                account = %account.pubkey.to_hex(),
                group_id = %hex::encode(group_id.as_slice()),
                "Skipping Darkmatter group projection refresh: no AccountGroup exists"
            );
            return Ok(());
        }

        let marmot_session =
            session
                .marmot
                .clone()
                .ok_or(WhitenoiseError::MarmotSessionUnavailable(
                    session.account_pubkey,
                ))?;
        let projection = {
            let session = marmot_session.lock().await;
            session.group_projection(group_id)?
        };
        session.marmot_storage.put_group_projection(&projection)?;
        if let Err(error) = session
            .push()
            .reconcile_group_tokens_for_active_leaves(&legacy_group_id)
            .await
        {
            tracing::warn!(
                target: "whitenoise::event_processor::handle_mls_message",
                account = %account.pubkey.to_hex(),
                group_id = %hex::encode(group_id.as_slice()),
                error = %error,
                "Failed to reconcile group push tokens after Darkmatter projection refresh"
            );
        }

        self.background_refresh_account_group_subscriptions(account);
        self.background_sync_group_image_cache_if_needed(account, &legacy_group_id);

        Ok(())
    }

    async fn handle_marmot_application_message(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        group_id: MarmotGroupId,
        sender: cgka_traits::types::MemberId,
        payload: Vec<u8>,
        wrapper_event_id: EventId,
    ) -> Result<()> {
        let legacy_group_id = GroupId::from_slice(group_id.as_slice());
        let has_account_group = AccountGroup::find_by_account_and_group(
            &account.pubkey,
            &legacy_group_id,
            &session.account_db.inner.pool,
        )
        .await?
        .is_some();

        if !has_account_group {
            tracing::info!(
                target: "whitenoise::event_processor::event_handlers::handle_mls_message",
                group_id = %hex::encode(group_id.as_slice()),
                account = %account.pubkey.to_hex(),
                "Skipping Darkmatter app message: no AccountGroup exists \
                 (group may have been deleted)"
            );
            return Ok(());
        }

        let epoch = match &session.marmot {
            Some(marmot_session) => {
                let marmot_session = marmot_session.lock().await;
                Some(marmot_session.group_epoch(&group_id)?)
            }
            None => None,
        };
        let marmot_message = MarmotMessage::from_app_payload(
            legacy_group_id,
            &payload,
            wrapper_event_id,
            epoch,
            MessageState::Processed,
            &sender,
        )?;

        if is_push_group_message_kind(marmot_message.kind) {
            let sender_leaf_index = self
                .marmot_sender_push_leaf_index(session, &group_id, &sender)
                .await?;
            if let Err(error) = session
                .push()
                .handle_received_push_group_message(&marmot_message, sender_leaf_index)
                .await
            {
                tracing::warn!(
                    target: "whitenoise::event_processor::event_handlers::handle_mls_message",
                    group_id = %hex::encode(group_id.as_slice()),
                    account = %account.pubkey.to_hex(),
                    error = %error,
                    "Failed to handle Darkmatter push group message"
                );
            }
            return Ok(());
        }
        self.handle_standard_marmot_application_message(session, account, marmot_message)
            .await
    }

    async fn marmot_sender_push_leaf_index(
        &self,
        session: &Arc<AccountSession>,
        group_id: &MarmotGroupId,
        sender: &MemberId,
    ) -> Result<Option<u32>> {
        let sender_pubkey = PublicKey::from_slice(sender.as_slice()).map_err(|error| {
            WhitenoiseError::InvalidInput(format!("invalid Darkmatter push sender pubkey: {error}"))
        })?;
        let marmot_session =
            session
                .marmot
                .clone()
                .ok_or(WhitenoiseError::MarmotSessionUnavailable(
                    session.account_pubkey,
                ))?;
        let member_index = {
            let marmot_session = marmot_session.lock().await;
            marmot_session.push_member_index_map(group_id)?
        };

        Ok(member_index
            .into_iter()
            .find_map(|(index, pubkey)| (pubkey == sender_pubkey).then_some(index)))
    }

    async fn handle_standard_marmot_application_message(
        &self,
        session: &Arc<AccountSession>,
        account: &Account,
        marmot_message: MarmotMessage,
    ) -> Result<()> {
        let group_id = marmot_message.mls_group_id.clone();
        let media_files = MediaFiles::new(
            &self.shared.storage,
            &self.shared.database,
            &session.account_db.inner.pool,
        );
        let parsed_references = media_files.parse_imeta_tags_from_event(&marmot_message.event)?;
        media_files
            .store_parsed_media_references(&group_id, &account.pubkey, parsed_references)
            .await?;

        MarmotMessageProjection::upsert(&marmot_message, &session.account_db.inner).await?;
        match marmot_message.kind {
            Kind::ChatMessage => {
                let msg = self
                    .cache_chat_message(&account.pubkey, &group_id, &marmot_message)
                    .await?;
                let group_name = session.groups().get(&group_id).ok().map(|group| group.name);
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
                    .cache_reaction(&account.pubkey, &group_id, &marmot_message)
                    .await?
                {
                    self.emit_message_update(&group_id, UpdateTrigger::ReactionAdded, target);
                }
            }
            Kind::EventDeletion => {
                self.handle_deletion_application_message(
                    &account.pubkey,
                    &group_id,
                    &marmot_message,
                )
                .await?;
            }
            _ => {
                tracing::debug!(
                    target: "whitenoise::event_processor::handle_mls_message",
                    "Ignoring Darkmatter app message kind {:?} for cache",
                    marmot_message.kind
                );
            }
        }

        Ok(())
    }

    async fn handle_deletion_application_message(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        message: &MarmotMessage,
    ) -> Result<()> {
        // Capture the pre-deletion last message so authorized deletes can emit
        // LastMessageDeleted after cache_deletion mutates the target row.
        let last_message_id = self.get_last_message_id(account_pubkey, group_id).await;
        let updates = self
            .cache_deletion(account_pubkey, group_id, message)
            .await?;
        let deleted_last_message = last_message_id.as_ref().is_some_and(|last_message_id| {
            updates.iter().any(|(trigger, msg)| {
                matches!(trigger, UpdateTrigger::MessageDeleted) && &msg.id == last_message_id
            })
        });

        for (trigger, msg) in updates {
            self.emit_message_update(group_id, trigger, msg);
        }

        if deleted_last_message {
            self.emit_chat_list_update_for_group(
                group_id,
                ChatListUpdateTrigger::LastMessageDeleted,
            )
            .await;
        }

        Ok(())
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
        message: &MarmotMessage,
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
            .process_single_message(message, media_files)
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
        message: &MarmotMessage,
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
        reaction: &MarmotMessage,
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
        message: &MarmotMessage,
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
        deletion: &MarmotMessage,
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
                    target_id = %target_id,
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
                    target_id = %target_id,
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
            &message.author,
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

        // Apply orphaned deletions through the same path used for live deletes.
        // `find_orphaned_deletions` has already filtered to matching authors.
        for deletion in orphaned_deletions {
            if let Some((UpdateTrigger::MessageDeleted, deleted_message)) = self
                .apply_single_deletion(
                    account_pubkey,
                    &deletion.author,
                    &message.id,
                    &deletion.event_id,
                    group_id,
                )
                .await?
            {
                message = deleted_message;
            }
        }

        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use cgka_traits::app_components::{
        AppComponentData, NOSTR_ROUTING_COMPONENT_ID, NostrRoutingV1, encode_nostr_routing_v1,
    };
    use cgka_traits::app_event::MarmotAppEvent;
    use cgka_traits::engine::{CreateGroupRequest, KeyPackage};
    use cgka_traits::transport::{TransportEnvelope, TransportMessage};
    use cgka_traits::types::MessageId;
    use transport_nostr_peeler::NostrTransportEvent;

    use super::*;
    use crate::marmot::publish::{MarmotMessagePublisher, MarmotPublishOutcome};
    use crate::marmot::push::{
        ENCRYPTED_TOKEN_LEN, EncryptedToken, LeafTokenTag, NotificationPlatform, TokenTag,
        build_token_list_response_rumor, build_token_removal_rumor, build_token_request_rumor,
        push_token_fingerprint,
    };
    use crate::whitenoise::{
        aggregated_message::AggregatedMessage,
        database::group_push_tokens::GroupPushTokenUpsert,
        group_information::GroupType,
        message_aggregator::DeliveryStatus,
        push_notifications::{GroupPushToken, PushPlatform},
        test_utils::*,
    };

    #[derive(Clone, Default)]
    struct RecordingMarmotPublisher {
        messages: Arc<Mutex<Vec<TransportMessage>>>,
    }

    #[async_trait::async_trait]
    impl MarmotMessagePublisher for RecordingMarmotPublisher {
        async fn publish(&self, message: TransportMessage) -> MarmotPublishOutcome {
            self.messages.lock().unwrap().push(message);
            MarmotPublishOutcome::Published { accepted_count: 1 }
        }
    }

    impl RecordingMarmotPublisher {
        fn welcome_for(&self, recipient: PublicKey) -> TransportMessage {
            self.messages
                .lock()
                .unwrap()
                .iter()
                .find(|message| {
                    matches!(
                        &message.envelope,
                        TransportEnvelope::Welcome { recipient: member }
                            if member.as_slice() == recipient.as_bytes()
                    )
                })
                .cloned()
                .expect("expected a welcome message for recipient")
        }

        fn group_messages(&self) -> Vec<TransportMessage> {
            self.messages
                .lock()
                .unwrap()
                .iter()
                .filter(|message| {
                    matches!(message.envelope, TransportEnvelope::GroupMessage { .. })
                })
                .cloned()
                .collect()
        }
    }

    async fn create_local_marmot_test_session(
        whitenoise: &Arc<Whitenoise>,
    ) -> (Account, Keys, Arc<AccountSession>) {
        let keys = Keys::generate();
        let (account, _) = Account::new(whitenoise, Some(keys.clone())).await.unwrap();
        whitenoise
            .shared
            .secrets_store
            .store_private_key(&keys)
            .unwrap();
        let account = account.save(&whitenoise.shared.database).await.unwrap();
        let session = Arc::new(
            AccountSession::from_account(&account, whitenoise)
                .await
                .unwrap(),
        );
        whitenoise.account_manager.insert_session(session.clone());

        (account, keys, session)
    }

    async fn setup_two_member_marmot_group_without_relay_publish(
        creator_session: &Arc<AccountSession>,
        member_session: &Arc<AccountSession>,
        creator_pubkey: PublicKey,
        member_pubkey: PublicKey,
    ) -> GroupId {
        setup_marmot_group_without_relay_publish(
            creator_session,
            creator_pubkey,
            &[(member_session, member_pubkey)],
            GroupType::Group,
        )
        .await
    }

    async fn setup_marmot_group_without_relay_publish(
        creator_session: &Arc<AccountSession>,
        creator_pubkey: PublicKey,
        members: &[(&Arc<AccountSession>, PublicKey)],
        group_type: GroupType,
    ) -> GroupId {
        let mut member_key_packages = Vec::with_capacity(members.len());
        for (index, (member_session, _member_pubkey)) in members.iter().enumerate() {
            let marker = 0xD1_u8.saturating_add(index as u8);
            let marmot = member_session
                .marmot
                .clone()
                .expect("Marmot session exists");
            let mut marmot = marmot.lock().await;
            member_key_packages.push(key_package_with_source_event_id(
                marmot.fresh_key_package().await.unwrap(),
                marker,
            ));
        }

        let routing =
            NostrRoutingV1::new([0xD2; 32], vec!["ws://localhost:8080/".to_string()]).unwrap();
        let created = {
            let marmot = creator_session
                .marmot
                .clone()
                .expect("Marmot session exists");
            let mut marmot = marmot.lock().await;
            marmot
                .create_group(CreateGroupRequest {
                    name: "Darkmatter test".to_string(),
                    description: "Darkmatter test group".to_string(),
                    members: member_key_packages,
                    required_features: Vec::new(),
                    app_components: vec![AppComponentData {
                        component_id: NOSTR_ROUTING_COMPONENT_ID,
                        data: encode_nostr_routing_v1(&routing).unwrap(),
                    }],
                    initial_admins: Vec::new(),
                })
                .await
                .unwrap()
        };
        let PublishWork::GroupCreated {
            pending, welcomes, ..
        } = &created.effects.publish[0]
        else {
            panic!("Darkmatter group creation must produce pending publish work");
        };
        let projection = {
            let marmot = creator_session
                .marmot
                .clone()
                .expect("Marmot session exists");
            let mut marmot = marmot.lock().await;
            marmot.confirm_published(*pending).await.unwrap();
            marmot.group_projection(&created.group_id).unwrap()
        };

        let member_pubkeys = members
            .iter()
            .map(|(_member_session, member_pubkey)| *member_pubkey)
            .collect::<Vec<_>>();
        let group_id = creator_session
            .groups()
            .project_created_marmot_group(&projection, &member_pubkeys, Some(group_type.clone()))
            .await
            .unwrap();

        for (member_session, member_pubkey) in members {
            let welcome = welcomes
                .iter()
                .find(|message| {
                    matches!(
                        &message.envelope,
                        TransportEnvelope::Welcome { recipient }
                            if recipient.as_slice() == member_pubkey.as_bytes()
                    )
                })
                .cloned()
                .expect("group creation should produce a welcome for every member");
            let member_group_id = project_marmot_welcome_for_account(
                member_session,
                welcome,
                creator_pubkey,
                group_type.clone(),
            )
            .await;
            assert_eq!(member_group_id, group_id);
        }

        group_id
    }

    fn key_package_with_source_event_id(key_package: KeyPackage, marker: u8) -> KeyPackage {
        KeyPackage::with_source_event_id(
            key_package.bytes().to_vec(),
            MessageId::new(vec![marker; 32]),
        )
    }

    async fn project_marmot_welcome_for_account(
        session: &Arc<AccountSession>,
        welcome: TransportMessage,
        welcomer_pubkey: PublicKey,
        group_type: GroupType,
    ) -> GroupId {
        let marmot = session.marmot.clone().expect("Marmot session exists");
        let projection = {
            let mut session = marmot.lock().await;
            let joined = session.ingest(welcome).await.unwrap();
            let group_id = joined
                .effects
                .events
                .iter()
                .find_map(|event| match event {
                    cgka_traits::engine::GroupEvent::GroupJoined { group_id, .. } => {
                        Some(group_id.clone())
                    }
                    _ => None,
                })
                .expect("welcome ingest should join the Marmot group");
            session.group_projection(&group_id).unwrap()
        };
        let group_id = session
            .groups()
            .project_joined_marmot_group(&projection, Some(group_type), welcomer_pubkey)
            .await
            .unwrap();
        session
            .membership()
            .for_group(&group_id)
            .accept()
            .await
            .unwrap();

        group_id
    }

    async fn setup_two_member_marmot_group(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_account: &Account,
    ) -> GroupId {
        wait_for_key_package_publication(whitenoise, &[member_account]).await;

        let creator_session = whitenoise
            .require_session(&creator_account.pubkey)
            .expect("creator must have an active session");
        let member_session = whitenoise
            .require_session(&member_account.pubkey)
            .expect("member must have an active session");
        let group_publisher = RecordingMarmotPublisher::default();
        let group = creator_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![member_account.pubkey],
                darkmatter_test_group_config(vec![creator_account.pubkey]),
                Some(GroupType::Group),
                &group_publisher,
            )
            .await
            .unwrap();

        let member_group_id = project_marmot_welcome_for_account(
            &member_session,
            group_publisher.welcome_for(member_account.pubkey),
            creator_account.pubkey,
            GroupType::Group,
        )
        .await;
        assert_eq!(member_group_id, group.mls_group_id);

        group.mls_group_id
    }

    fn darkmatter_test_group_config(admins: Vec<PublicKey>) -> crate::marmot::GroupConfig {
        crate::marmot::GroupConfig::new(
            "Darkmatter test".to_string(),
            "Darkmatter test group".to_string(),
            None,
            None,
            None,
            vec![RelayUrl::parse("ws://localhost:8080/").unwrap()],
            admins,
            None,
        )
    }

    fn marmot_app_payload_from_unsigned_event(
        inner_event: &UnsignedEvent,
        expected_event_id: EventId,
    ) -> Vec<u8> {
        let tags = inner_event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect();
        let app_event = MarmotAppEvent::new(
            inner_event.pubkey.to_hex(),
            inner_event.created_at.as_secs(),
            u64::from(inner_event.kind.as_u16()),
            tags,
            inner_event.content.clone(),
        );

        assert_eq!(
            app_event.id,
            expected_event_id.to_hex(),
            "test app payload must preserve the canonical Nostr event id"
        );

        app_event.encode().unwrap()
    }

    fn marmot_message_from_unsigned_event(
        group_id: &GroupId,
        mut inner_event: UnsignedEvent,
    ) -> (MarmotMessage, EventId) {
        inner_event.ensure_id();
        let message_id = inner_event.id.expect("ensure_id sets the id");
        let message = MarmotMessage::from_unsigned_app_event(
            group_id.clone(),
            inner_event,
            EventId::all_zeros(),
            Some(0),
            MessageState::Processed,
        )
        .unwrap();

        (message, message_id)
    }

    async fn darkmatter_app_message_event(
        session: &Arc<AccountSession>,
        group_id: &GroupId,
        mut inner_event: UnsignedEvent,
    ) -> (Event, EventId) {
        inner_event.ensure_id();
        let message_id = inner_event.id.expect("ensure_id sets the id");
        let payload = marmot_app_payload_from_unsigned_event(&inner_event, message_id);
        let effects = {
            let marmot = session.marmot.clone().expect("Marmot session exists");
            let mut session = marmot.lock().await;
            session
                .send_app_message(MarmotGroupId::new(group_id.as_slice().to_vec()), payload)
                .await
                .unwrap()
        };
        let transport_message = effects
            .publish
            .into_iter()
            .find_map(|work| match work {
                PublishWork::ApplicationMessage { msg } => Some(msg),
                _ => None,
            })
            .expect("send should produce an application transport message");
        let nostr_event = NostrTransportEvent::from_transport_message(&transport_message)
            .unwrap()
            .to_verified_nostr_event()
            .unwrap();

        (nostr_event, message_id)
    }

    async fn marmot_push_leaf_index(
        session: &Arc<AccountSession>,
        group_id: &GroupId,
        pubkey: PublicKey,
    ) -> u32 {
        let marmot = session.marmot.clone().expect("Marmot session exists");
        let session = marmot.lock().await;
        session
            .push_member_index_map(&MarmotGroupId::new(group_id.as_slice().to_vec()))
            .unwrap()
            .into_iter()
            .find_map(|(leaf_index, member_pubkey)| (member_pubkey == pubkey).then_some(leaf_index))
            .expect("member should have a Darkmatter push leaf index")
    }

    fn make_token_tag(seed: u8) -> TokenTag {
        TokenTag {
            encrypted_token: EncryptedToken::from([seed; ENCRYPTED_TOKEN_LEN]),
            platform: None,
            token_fingerprint: None,
            server_pubkey: Keys::generate().public_key(),
            relay_hint: RelayUrl::parse("wss://push.example.com").unwrap(),
        }
    }

    fn make_darkmatter_token_tag(seed: u8) -> TokenTag {
        TokenTag {
            encrypted_token: EncryptedToken::from([seed; ENCRYPTED_TOKEN_LEN]),
            platform: Some(NotificationPlatform::Fcm),
            token_fingerprint: Some(push_token_fingerprint(
                NotificationPlatform::Fcm,
                format!("firebase-token-{seed}").as_bytes(),
            )),
            server_pubkey: Keys::generate().public_key(),
            relay_hint: RelayUrl::parse("wss://push.example.com").unwrap(),
        }
    }

    async fn assert_cached_peer_tokens(
        session: &Arc<AccountSession>,
        group_id: &GroupId,
        expected_tokens: &[(&Account, u32, &TokenTag)],
    ) {
        let stored = GroupPushToken::find_by_account_and_group(
            &session.account_pubkey,
            group_id,
            &session.account_db.inner.pool,
        )
        .await
        .unwrap();

        assert_eq!(
            stored.len(),
            expected_tokens.len(),
            "receiver should cache exactly the other participants' push tokens"
        );

        for (account, leaf_index, token_tag) in expected_tokens {
            let cached_token = stored
                .iter()
                .find(|token| token.member_pubkey == account.pubkey)
                .expect("expected participant token should be cached");
            assert_eq!(cached_token.account_pubkey, session.account_pubkey);
            assert_eq!(cached_token.mls_group_id, *group_id);
            assert_eq!(cached_token.leaf_index, *leaf_index);
            assert_eq!(cached_token.server_pubkey, token_tag.server_pubkey);
            assert_eq!(cached_token.relay_hint, Some(token_tag.relay_hint.clone()));
            assert_eq!(
                cached_token.encrypted_token,
                token_tag.encrypted_token.to_base64()
            );
        }
    }

    #[tokio::test]
    async fn test_handle_mls_message_accepts_darkmatter_app_message() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let alice_account = whitenoise.create_identity().await.unwrap();
        let bob_account = whitenoise.create_identity().await.unwrap();
        wait_for_key_package_publication(&whitenoise, &[&bob_account]).await;

        let alice_session = whitenoise.require_session(&alice_account.pubkey).unwrap();
        let bob_session = whitenoise.require_session(&bob_account.pubkey).unwrap();
        let group_publisher = RecordingMarmotPublisher::default();
        let group = alice_session
            .groups()
            .create_marmot_group_with_publisher(
                vec![bob_account.pubkey],
                darkmatter_test_group_config(vec![alice_account.pubkey]),
                Some(GroupType::Group),
                &group_publisher,
            )
            .await
            .unwrap();

        let welcome = group_publisher.welcome_for(bob_account.pubkey);
        let bob_marmot = bob_session.marmot.clone().expect("Marmot session exists");
        let bob_projection = {
            let mut session = bob_marmot.lock().await;
            let joined = session.ingest(welcome).await.unwrap();
            assert!(
                joined.effects.events.iter().any(|event| matches!(
                    event,
                    cgka_traits::engine::GroupEvent::GroupJoined { .. }
                )),
                "welcome ingest should join the Marmot group"
            );
            session
                .group_projection(&cgka_traits::types::GroupId::new(
                    group.mls_group_id.as_slice().to_vec(),
                ))
                .unwrap()
        };
        bob_session
            .groups()
            .project_created_marmot_group(
                &bob_projection,
                &[alice_account.pubkey],
                Some(GroupType::Group),
            )
            .await
            .unwrap();

        let original_hash = [0x11; 32];
        let encrypted_hash = [0x22; 32];
        let nonce = [0x33; 12];
        let imeta_tag = Tag::custom(
            TagKind::Custom("imeta".into()),
            [
                format!(
                    "url http://localhost:3000/{}.png",
                    hex::encode(encrypted_hash)
                ),
                "m image/png".to_string(),
                "filename darkmatter-media.png".to_string(),
                format!("x {}", hex::encode(original_hash)),
                format!("n {}", hex::encode(nonce)),
                "v mip04-v2".to_string(),
            ],
        );
        let app_event = MarmotAppEvent::new(
            alice_account.pubkey.to_hex(),
            Timestamp::now().as_secs(),
            9,
            vec![imeta_tag.as_slice().to_vec()],
            "hello through darkmatter ingest",
        );
        let sent = {
            let alice_marmot = alice_session.marmot.clone().expect("Marmot session exists");
            let mut session = alice_marmot.lock().await;
            session
                .send_app_message(
                    cgka_traits::types::GroupId::new(group.mls_group_id.as_slice().to_vec()),
                    app_event.encode().unwrap(),
                )
                .await
                .unwrap()
        };
        let transport_message = sent
            .publish
            .into_iter()
            .find_map(|work| match work {
                crate::marmot::session::PublishWork::ApplicationMessage { msg } => Some(msg),
                _ => None,
            })
            .expect("send should produce an application transport message");
        let mls_event = NostrTransportEvent::from_transport_message(&transport_message)
            .unwrap()
            .to_verified_nostr_event()
            .unwrap();

        whitenoise
            .handle_mls_message(&bob_session, &bob_account, mls_event)
            .await
            .unwrap();

        let cached = AggregatedMessage::find_by_id(
            &app_event.id,
            &group.mls_group_id,
            &bob_account.pubkey,
            &bob_session.account_db.inner,
        )
        .await
        .unwrap()
        .expect("Darkmatter app message should be cached for the recipient");
        assert_eq!(cached.content, "hello through darkmatter ingest");
        assert_eq!(cached.author, alice_account.pubkey);

        let media_files = MediaFile::find_by_group(
            &bob_session.account_db.inner.pool,
            &whitenoise.shared.database,
            &bob_account.pubkey,
            &group.mls_group_id,
        )
        .await
        .unwrap();
        let media_file = media_files
            .iter()
            .find(|file| file.original_file_hash.as_deref() == Some(original_hash.as_slice()))
            .expect("Darkmatter imeta reference should be stored for the recipient");
        assert_eq!(media_file.encrypted_file_hash, encrypted_hash);
        assert_eq!(media_file.mime_type, "image/png");
        assert_eq!(media_file.media_type, "chat_media");
        assert_eq!(
            media_file.nonce.as_deref(),
            Some(hex::encode(nonce).as_str())
        );
    }

    #[tokio::test]
    async fn test_handle_mls_message_publishes_darkmatter_selfremove_auto_commit() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (alice_account, _alice_keys, alice_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (bob_account, _bob_keys, bob_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (carol_account, _carol_keys, carol_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let group_id = setup_marmot_group_without_relay_publish(
            &alice_session,
            alice_account.pubkey,
            &[
                (&bob_session, bob_account.pubkey),
                (&carol_session, carol_account.pubkey),
            ],
            GroupType::Group,
        )
        .await;

        let bob_leave_publisher = RecordingMarmotPublisher::default();
        bob_session
            .groups()
            .leave_marmot_group_with_publisher(&group_id, &bob_leave_publisher)
            .await
            .unwrap();
        let bob_leave_messages = bob_leave_publisher.group_messages();
        let [self_remove_proposal] = bob_leave_messages.as_slice() else {
            panic!("Bob leave should publish one SelfRemove proposal");
        };
        let proposal_event = NostrTransportEvent::from_transport_message(self_remove_proposal)
            .unwrap()
            .to_verified_nostr_event()
            .unwrap();

        let alice_inbound_publisher = RecordingMarmotPublisher::default();
        whitenoise
            .try_handle_marmot_mls_message_with_publisher(
                &alice_session,
                &alice_account,
                &proposal_event,
                &alice_inbound_publisher,
            )
            .await
            .expect("Alice should handle Bob's SelfRemove proposal as a Darkmatter message");

        let alice_inbound_messages = alice_inbound_publisher.group_messages();
        let [auto_commit] = alice_inbound_messages.as_slice() else {
            panic!("Alice should publish one Darkmatter auto-commit for Bob's SelfRemove");
        };
        assert!(matches!(
            auto_commit.envelope,
            TransportEnvelope::GroupMessage { .. }
        ));

        let members = alice_session.groups().members(&group_id).unwrap();
        assert!(!members.contains(&bob_account.pubkey));
        assert!(members.contains(&alice_account.pubkey));
        assert!(members.contains(&carol_account.pubkey));
    }

    #[tokio::test]
    async fn test_handle_mls_message_emits_removed_from_group_after_darkmatter_removal_commit() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (alice_account, _alice_keys, alice_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (bob_account, _bob_keys, bob_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (carol_account, _carol_keys, carol_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let group_id = setup_marmot_group_without_relay_publish(
            &alice_session,
            alice_account.pubkey,
            &[
                (&bob_session, bob_account.pubkey),
                (&carol_session, carol_account.pubkey),
            ],
            GroupType::Group,
        )
        .await;

        let mut group_state_updates = whitenoise
            .subscribe_to_group_state(&carol_account.pubkey, &group_id)
            .await
            .unwrap()
            .updates;

        let removal_publisher = RecordingMarmotPublisher::default();
        alice_session
            .groups()
            .remove_marmot_members_with_publisher(
                &group_id,
                vec![carol_account.pubkey],
                &removal_publisher,
            )
            .await
            .unwrap();

        let group_messages = removal_publisher.group_messages();
        let [removal_commit] = group_messages.as_slice() else {
            panic!("Alice removal should publish one Darkmatter group evolution commit");
        };
        let removal_event = NostrTransportEvent::from_transport_message(removal_commit)
            .unwrap()
            .to_verified_nostr_event()
            .unwrap();

        whitenoise
            .try_handle_marmot_mls_message_with_publisher(
                &carol_session,
                &carol_account,
                &removal_event,
                &RecordingMarmotPublisher::default(),
            )
            .await
            .expect("Carol should handle Alice's removal commit as a Darkmatter message");

        let update = tokio::time::timeout(Duration::from_secs(1), group_state_updates.recv())
            .await
            .expect("removed member should receive a group-state update")
            .expect("group-state stream should remain open");

        assert_eq!(
            update,
            crate::whitenoise::group_state_streaming::GroupStateUpdate::RemovedFromGroup
        );
    }

    /// Test handling of different MLS message types: regular messages, reactions, and deletions
    #[tokio::test]
    async fn test_handle_mls_message_different_types() {
        // Arrange: Setup whitenoise and create a group
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let group_id =
            setup_two_member_marmot_group(&whitenoise, &creator_account, &member_account).await;

        // Test 1: Regular message (Kind 9)
        let inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Test message".to_string(),
        );
        let (message_event, message_id) =
            darkmatter_app_message_event(&creator_session, &group_id, inner).await;

        let result = whitenoise
            .handle_mls_message(&member_session, &member_account, message_event)
            .await;
        assert!(result.is_ok(), "Failed to handle regular message");

        // Verify message was cached
        let cached_msg = AggregatedMessage::find_by_id(
            &message_id.to_string(),
            &group_id,
            &member_account.pubkey,
            &member_session.account_db.inner,
        )
        .await
        .unwrap();
        assert!(cached_msg.is_some(), "Message should be cached");

        // Test 2: Reaction message (Kind 7)
        let reaction_inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Reaction,
            vec![Tag::parse(vec!["e", &message_id.to_string()]).unwrap()],
            "👍".to_string(),
        );
        let (reaction_event, _) =
            darkmatter_app_message_event(&creator_session, &group_id, reaction_inner).await;

        let result = whitenoise
            .handle_mls_message(&member_session, &member_account, reaction_event)
            .await;
        assert!(result.is_ok(), "Failed to handle reaction");

        // Verify reaction was applied to cached message
        let cached_msg = AggregatedMessage::find_by_id(
            &message_id.to_string(),
            &group_id,
            &member_account.pubkey,
            &member_session.account_db.inner,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            !cached_msg.reactions.by_emoji.is_empty(),
            "Reaction should be applied"
        );

        // Test 3: Deletion message (Kind 5)
        let deletion_inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::EventDeletion,
            vec![Tag::parse(vec!["e", &message_id.to_string()]).unwrap()],
            String::new(),
        );
        let (deletion_event, _) =
            darkmatter_app_message_event(&creator_session, &group_id, deletion_inner).await;

        let result = whitenoise
            .handle_mls_message(&member_session, &member_account, deletion_event)
            .await;
        assert!(result.is_ok(), "Failed to handle deletion");

        // Verify message was marked as deleted
        let cached_msg = AggregatedMessage::find_by_id(
            &message_id.to_string(),
            &group_id,
            &member_account.pubkey,
            &member_session.account_db.inner,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(cached_msg.is_deleted, "Message should be marked as deleted");
    }

    #[tokio::test]
    async fn test_handle_darkmatter_duplicate_message_is_idempotent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let group_id =
            setup_two_member_marmot_group(&whitenoise, &creator_account, &member_account).await;

        let inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Idempotent duplicate".to_string(),
        );
        let (message_event, message_id) =
            darkmatter_app_message_event(&creator_session, &group_id, inner).await;

        whitenoise
            .handle_mls_message(&member_session, &member_account, message_event.clone())
            .await
            .unwrap();
        whitenoise
            .handle_mls_message(&member_session, &member_account, message_event)
            .await
            .unwrap();

        let cached_msg = AggregatedMessage::find_by_id(
            &message_id.to_string(),
            &group_id,
            &member_account.pubkey,
            &member_session.account_db.inner,
        )
        .await
        .unwrap();
        assert!(cached_msg.is_some(), "Message should remain cached once");
    }

    #[tokio::test]
    async fn test_handle_mls_message_rejects_cross_author_deletion() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (admin_account, _admin_keys, admin_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (receiver_account, _receiver_keys, receiver_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (attacker_account, _attacker_keys, attacker_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let group_id = setup_marmot_group_without_relay_publish(
            &admin_session,
            admin_account.pubkey,
            &[
                (&receiver_session, receiver_account.pubkey),
                (&attacker_session, attacker_account.pubkey),
            ],
            GroupType::Group,
        )
        .await;

        let message_inner = UnsignedEvent::new(
            admin_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Protected admin message".to_string(),
        );
        let (message_event, message_id) =
            darkmatter_app_message_event(&admin_session, &group_id, message_inner).await;

        whitenoise
            .handle_mls_message(&receiver_session, &receiver_account, message_event)
            .await
            .unwrap();

        let deletion_inner = UnsignedEvent::new(
            attacker_account.pubkey,
            Timestamp::now(),
            Kind::EventDeletion,
            vec![Tag::parse(vec!["e", &message_id.to_string()]).unwrap()],
            String::new(),
        );
        let (deletion_event, _) =
            darkmatter_app_message_event(&attacker_session, &group_id, deletion_inner).await;

        whitenoise
            .handle_mls_message(&receiver_session, &receiver_account, deletion_event)
            .await
            .unwrap();

        let cached_msg = AggregatedMessage::find_by_id(
            &message_id.to_string(),
            &group_id,
            &receiver_account.pubkey,
            &receiver_session.account_db.inner,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!cached_msg.is_deleted);
    }

    #[tokio::test]
    async fn test_handle_mls_message_rejects_cross_author_orphaned_deletion() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (admin_account, _admin_keys, admin_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (receiver_account, _receiver_keys, receiver_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (attacker_account, _attacker_keys, attacker_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let group_id = setup_marmot_group_without_relay_publish(
            &admin_session,
            admin_account.pubkey,
            &[
                (&receiver_session, receiver_account.pubkey),
                (&attacker_session, attacker_account.pubkey),
            ],
            GroupType::Group,
        )
        .await;

        let mut message_inner = UnsignedEvent::new(
            admin_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Late protected admin message".to_string(),
        );
        message_inner.ensure_id();
        let future_message_id = message_inner.id.unwrap();

        let deletion_inner = UnsignedEvent::new(
            attacker_account.pubkey,
            Timestamp::now(),
            Kind::EventDeletion,
            vec![Tag::parse(vec!["e", &future_message_id.to_string()]).unwrap()],
            String::new(),
        );
        let (deletion_event, _) =
            darkmatter_app_message_event(&attacker_session, &group_id, deletion_inner).await;

        whitenoise
            .handle_mls_message(&receiver_session, &receiver_account, deletion_event)
            .await
            .unwrap();

        let (message_event, _) =
            darkmatter_app_message_event(&admin_session, &group_id, message_inner).await;

        whitenoise
            .handle_mls_message(&receiver_session, &receiver_account, message_event)
            .await
            .unwrap();

        let cached_msg = AggregatedMessage::find_by_id(
            &future_message_id.to_string(),
            &group_id,
            &receiver_account.pubkey,
            &receiver_session.account_db.inner,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!cached_msg.is_deleted);
    }

    #[tokio::test]
    async fn test_handle_mls_message_rejects_cross_author_reaction_deletion() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (admin_account, _admin_keys, admin_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (receiver_account, _receiver_keys, receiver_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (attacker_account, _attacker_keys, attacker_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let group_id = setup_marmot_group_without_relay_publish(
            &admin_session,
            admin_account.pubkey,
            &[
                (&receiver_session, receiver_account.pubkey),
                (&attacker_session, attacker_account.pubkey),
            ],
            GroupType::Group,
        )
        .await;

        let message_inner = UnsignedEvent::new(
            admin_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Reaction parent".to_string(),
        );
        let (message_event, message_id) =
            darkmatter_app_message_event(&admin_session, &group_id, message_inner).await;

        whitenoise
            .handle_mls_message(&receiver_session, &receiver_account, message_event)
            .await
            .unwrap();

        let reaction_inner = UnsignedEvent::new(
            admin_account.pubkey,
            Timestamp::now(),
            Kind::Reaction,
            vec![Tag::parse(vec!["e", &message_id.to_string()]).unwrap()],
            "+".to_string(),
        );
        let (reaction_event, reaction_id) =
            darkmatter_app_message_event(&admin_session, &group_id, reaction_inner).await;

        whitenoise
            .handle_mls_message(&receiver_session, &receiver_account, reaction_event)
            .await
            .unwrap();

        let deletion_inner = UnsignedEvent::new(
            attacker_account.pubkey,
            Timestamp::now(),
            Kind::EventDeletion,
            vec![Tag::parse(vec!["e", &reaction_id.to_string()]).unwrap()],
            String::new(),
        );
        let (deletion_event, _) =
            darkmatter_app_message_event(&attacker_session, &group_id, deletion_inner).await;

        whitenoise
            .handle_mls_message(&receiver_session, &receiver_account, deletion_event)
            .await
            .unwrap();

        let cached_msg = AggregatedMessage::find_by_id(
            &message_id.to_string(),
            &group_id,
            &receiver_account.pubkey,
            &receiver_session.account_db.inner,
        )
        .await
        .unwrap()
        .unwrap();
        let total_reactions: usize = cached_msg
            .reactions
            .by_emoji
            .values()
            .map(|reaction| reaction.count)
            .sum();
        assert_eq!(total_reactions, 1);

        let cached_reaction = AggregatedMessage::find_reaction_by_id(
            &reaction_id.to_string(),
            &group_id,
            &receiver_session.account_db.inner,
        )
        .await
        .unwrap();
        assert!(cached_reaction.is_some());
    }

    #[tokio::test]
    async fn test_cache_chat_message_preserves_existing_delivery_status() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let group_id =
            setup_two_member_marmot_group(&whitenoise, &creator_account, &member_account).await;

        let inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Echo status preservation".to_string(),
        );
        let (message, message_id) = marmot_message_from_unsigned_event(&group_id, inner);

        // Initial cache pass creates the row without delivery status.
        let first = whitenoise
            .cache_chat_message(&member_account.pubkey, &group_id, &message)
            .await
            .unwrap();
        assert_eq!(first.delivery_status, None);

        // Simulate background publish completion updating delivery status.
        AggregatedMessage::update_delivery_status(
            &message_id.to_string(),
            &group_id,
            &member_account.pubkey,
            &DeliveryStatus::Sent(1),
            &member_session.account_db.inner,
        )
        .await
        .unwrap();

        // Relay echo reprocess should preserve the existing status.
        let second = whitenoise
            .cache_chat_message(&member_account.pubkey, &group_id, &message)
            .await
            .unwrap();
        assert_eq!(second.delivery_status, Some(DeliveryStatus::Sent(1)));

        let persisted = AggregatedMessage::find_by_id(
            &message_id.to_string(),
            &group_id,
            &member_account.pubkey,
            &member_session.account_db.inner,
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
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let group_id =
            setup_two_member_marmot_group(&whitenoise, &creator_account, &member_account).await;

        let inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Valid message".to_string(),
        );
        let (valid_event, _) =
            darkmatter_app_message_event(&creator_session, &group_id, inner).await;

        // Corrupt the event by changing its kind. The Darkmatter peeler should
        // reject it, and Darkmatter-only accounts must not create legacy MLS
        // storage just to fail in the fallback path.
        let mut bad_event = valid_event;
        bad_event.kind = Kind::TextNote;

        let result = whitenoise
            .handle_mls_message(&member_session, &member_account, bad_event)
            .await;

        assert!(result.is_err(), "Expected error for corrupted event");
        match result.err().unwrap() {
            WhitenoiseError::InvalidEvent(_) => {}
            other => panic!("Expected InvalidEvent, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_handle_mls_message_darkmatter_event_without_marmot_does_not_fall_back_to_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (creator_account, _creator_keys, creator_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (member_account, member_keys, member_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let group_id = setup_two_member_marmot_group_without_relay_publish(
            &creator_session,
            &member_session,
            creator_account.pubkey,
            member_account.pubkey,
        )
        .await;

        let inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Valid Darkmatter message".to_string(),
        );
        let (mls_event, _) = darkmatter_app_message_event(&creator_session, &group_id, inner).await;
        let artifacts = remove_obsolete_mls_artifacts(&member_account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);
        let member_session_without_marmot = Arc::new(
            AccountSession::new(
                member_account.pubkey,
                whitenoise.shared.clone(),
                Arc::downgrade(&whitenoise),
                Some(Arc::new(member_keys)),
                None,
            )
            .await
            .unwrap(),
        );

        let result = whitenoise
            .handle_mls_message(&member_session_without_marmot, &member_account, mls_event)
            .await;

        match result {
            Err(WhitenoiseError::MarmotSessionUnavailable(pubkey))
                if pubkey == member_account.pubkey => {}
            other => panic!("Expected MarmotSessionUnavailable, got: {other:?}"),
        }
        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    #[tokio::test]
    async fn test_handle_mls_message_darkmatter_route_miss_does_not_fall_back_to_obsolete_mls_storage()
     {
        let (whitenoise, data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (creator_account, _creator_keys, creator_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (member_account, _member_keys, member_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (unrelated_account, _unrelated_keys, unrelated_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let group_id = setup_two_member_marmot_group_without_relay_publish(
            &creator_session,
            &member_session,
            creator_account.pubkey,
            member_account.pubkey,
        )
        .await;

        let inner = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Valid Darkmatter message for another local account".to_string(),
        );
        let (mls_event, _) = darkmatter_app_message_event(&creator_session, &group_id, inner).await;
        let artifacts = remove_obsolete_mls_artifacts(&unrelated_account, data_temp.path());
        assert_obsolete_mls_artifacts_absent(&artifacts);

        whitenoise
            .handle_mls_message(&unrelated_session, &unrelated_account, mls_event)
            .await
            .expect("Darkmatter route misses should be ignored without legacy fallback");

        assert_obsolete_mls_artifacts_absent(&artifacts);
    }

    /// Test orphaned reactions and deletions are applied when target message arrives later
    #[tokio::test]
    async fn test_handle_mls_message_orphaned_reactions_and_deletions() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let creator_account = whitenoise.create_identity().await.unwrap();
        let creator_session = whitenoise.require_session(&creator_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let group_id =
            setup_two_member_marmot_group(&whitenoise, &creator_account, &member_account).await;

        let mut actual_message = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![],
            "Late message".to_string(),
        );
        actual_message.ensure_id();
        let future_message_id = actual_message.id.expect("ensure_id sets the id");

        let orphaned_reaction = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Reaction,
            vec![Tag::parse(vec!["e", &future_message_id.to_string()]).unwrap()],
            "+".to_string(), // Use simple emoji that won't be normalized
        );
        let (reaction_event, _) =
            darkmatter_app_message_event(&creator_session, &group_id, orphaned_reaction).await;

        let result = whitenoise
            .handle_mls_message(&member_session, &member_account, reaction_event)
            .await;
        assert!(result.is_ok(), "Orphaned reaction should be stored");

        // Verify orphaned reaction is stored
        let orphaned_reactions = AggregatedMessage::find_orphaned_reactions(
            &future_message_id.to_string(),
            &group_id,
            &member_session.account_db.inner,
        )
        .await
        .unwrap();
        assert_eq!(
            orphaned_reactions.len(),
            1,
            "Should have one orphaned reaction"
        );

        let (message_event, _) =
            darkmatter_app_message_event(&creator_session, &group_id, actual_message).await;

        let result = whitenoise
            .handle_mls_message(&member_session, &member_account, message_event)
            .await;
        assert!(
            result.is_ok(),
            "Message with orphaned reaction should succeed"
        );

        // Verify the orphaned reaction was applied
        let cached_msg = AggregatedMessage::find_by_id(
            &future_message_id.to_string(),
            &group_id,
            &member_account.pubkey,
            &member_session.account_db.inner,
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
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let group_id =
            setup_two_member_marmot_group(&whitenoise, &creator_account, &member_account).await;

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
        let valid_reaction = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Reaction,
            vec![Tag::parse(vec!["e", &future_message_id.to_string()]).unwrap()],
            "👍".to_string(),
        );
        let (valid_event, _) =
            darkmatter_app_message_event(&creator_session, &group_id, valid_reaction).await;

        whitenoise
            .handle_mls_message(&member_session, &member_account, valid_event)
            .await
            .unwrap();

        // Send an INVALID orphaned reaction (empty content - not a valid emoji)
        let invalid_reaction = UnsignedEvent::new(
            creator_account.pubkey,
            Timestamp::now(),
            Kind::Reaction,
            vec![Tag::parse(vec!["e", &future_message_id.to_string()]).unwrap()],
            "".to_string(), // Empty content is invalid
        );
        let (invalid_event, _) =
            darkmatter_app_message_event(&creator_session, &group_id, invalid_reaction).await;

        whitenoise
            .handle_mls_message(&member_session, &member_account, invalid_event)
            .await
            .unwrap();

        let (message_event, _) =
            darkmatter_app_message_event(&creator_session, &group_id, actual_message).await;

        let result = whitenoise
            .handle_mls_message(&member_session, &member_account, message_event)
            .await;

        // The critical assertion: message processing should succeed
        assert!(
            result.is_ok(),
            "Message processing should succeed despite invalid orphaned reaction"
        );

        // Verify only the valid reaction was applied
        let cached_msg = AggregatedMessage::find_by_id(
            &future_message_id.to_string(),
            &group_id,
            &member_account.pubkey,
            &member_session.account_db.inner,
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

    /// Test helper methods: extract_reaction_target_id, extract_deletion_target_ids, etc.
    #[tokio::test]
    async fn test_helper_methods() {
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
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let group_id =
            setup_two_member_marmot_group(&whitenoise, &creator_account, &member_account).await;

        // Send multiple messages
        for i in 1..=3 {
            let inner = UnsignedEvent::new(
                creator_account.pubkey,
                Timestamp::now(),
                Kind::Custom(9),
                vec![],
                format!("Message {}", i),
            );
            let (event, _) = darkmatter_app_message_event(&creator_session, &group_id, inner).await;

            whitenoise
                .handle_mls_message(&member_session, &member_account, event)
                .await
                .unwrap();
        }

        // Verify all messages are in cache
        let messages = AggregatedMessage::find_messages_by_group(
            &group_id,
            &member_account.pubkey,
            None,
            &member_session.account_db.inner,
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
                &member_account.pubkey,
                &group_id,
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
        let admin_session = whitenoise.require_session(&admin_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let group_id =
            setup_two_member_marmot_group(&whitenoise, &admin_account, &member_account).await;
        let admin_leaf_index =
            marmot_push_leaf_index(&member_session, &group_id, admin_account.pubkey).await;

        let token_tag = make_token_tag(1);
        let request = build_token_request_rumor(
            admin_account.pubkey,
            Timestamp::now(),
            vec![token_tag.clone()],
        )
        .unwrap();
        let (event, _) = darkmatter_app_message_event(&admin_session, &group_id, request).await;

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
    async fn test_handle_mls_message_updates_token_request_cache_inside_response_cooldown() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let admin_session = whitenoise.require_session(&admin_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let group_id =
            setup_two_member_marmot_group(&whitenoise, &admin_account, &member_account).await;
        let admin_leaf_index =
            marmot_push_leaf_index(&member_session, &group_id, admin_account.pubkey).await;

        let first_token = make_token_tag(10);
        let first_request =
            build_token_request_rumor(admin_account.pubkey, Timestamp::now(), vec![first_token])
                .unwrap();
        let first_request_id = first_request.id.expect("447 rumor must have an event id");
        let (first_event, _) =
            darkmatter_app_message_event(&admin_session, &group_id, first_request).await;

        whitenoise
            .handle_mls_message(&member_session, &member_account, first_event)
            .await
            .unwrap();

        let replacement_token = make_token_tag(11);
        let expected_encrypted_token = replacement_token.encrypted_token.to_base64();
        let replacement_request = build_token_request_rumor(
            admin_account.pubkey,
            Timestamp::now(),
            vec![replacement_token],
        )
        .unwrap();
        let replacement_request_id = replacement_request
            .id
            .expect("447 replacement rumor must have an event id");
        let (replacement_event, _) =
            darkmatter_app_message_event(&admin_session, &group_id, replacement_request).await;

        whitenoise
            .handle_mls_message(&member_session, &member_account, replacement_event)
            .await
            .unwrap();

        let stored = GroupPushToken::find_by_account_and_group(
            &member_account.pubkey,
            &group_id,
            &member_session.account_db.inner.pool,
        )
        .await
        .unwrap();
        let admin_token = stored
            .iter()
            .find(|token| token.leaf_index == admin_leaf_index)
            .expect("admin token should stay cached");
        assert_eq!(
            admin_token.encrypted_token, expected_encrypted_token,
            "rapid re-gossip should refresh cached encrypted token state"
        );

        assert!(
            member_session
                .pending_push_token_responses
                .contains_key(&(group_id.clone(), first_request_id)),
            "the first token request should still schedule one token-list response"
        );
        assert!(
            !member_session
                .pending_push_token_responses
                .contains_key(&(group_id.clone(), replacement_request_id)),
            "a rate-limited re-gossip must not schedule another token-list response"
        );
    }

    #[tokio::test]
    async fn test_handle_mls_message_gossips_token_requests_to_all_other_participants() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (admin_account, _admin_keys, admin_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (member_one, _member_one_keys, member_one_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (member_two, _member_two_keys, member_two_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let group_id = setup_marmot_group_without_relay_publish(
            &admin_session,
            admin_account.pubkey,
            &[
                (&member_one_session, member_one.pubkey),
                (&member_two_session, member_two.pubkey),
            ],
            GroupType::Group,
        )
        .await;

        let admin_leaf_index =
            marmot_push_leaf_index(&admin_session, &group_id, admin_account.pubkey).await;
        let member_one_leaf_index =
            marmot_push_leaf_index(&admin_session, &group_id, member_one.pubkey).await;
        let member_two_leaf_index =
            marmot_push_leaf_index(&admin_session, &group_id, member_two.pubkey).await;

        let admin_token = make_token_tag(30);
        let admin_request = build_token_request_rumor(
            admin_account.pubkey,
            Timestamp::now(),
            vec![admin_token.clone()],
        )
        .unwrap();
        let (admin_event, _) =
            darkmatter_app_message_event(&admin_session, &group_id, admin_request).await;
        whitenoise
            .handle_mls_message(&member_one_session, &member_one, admin_event.clone())
            .await
            .unwrap();
        whitenoise
            .handle_mls_message(&member_two_session, &member_two, admin_event)
            .await
            .unwrap();

        let member_one_token = make_token_tag(31);
        let member_one_request = build_token_request_rumor(
            member_one.pubkey,
            Timestamp::now(),
            vec![member_one_token.clone()],
        )
        .unwrap();
        let (member_one_event, _) =
            darkmatter_app_message_event(&member_one_session, &group_id, member_one_request).await;
        whitenoise
            .handle_mls_message(&admin_session, &admin_account, member_one_event.clone())
            .await
            .unwrap();
        whitenoise
            .handle_mls_message(&member_two_session, &member_two, member_one_event)
            .await
            .unwrap();

        let member_two_token = make_token_tag(32);
        let member_two_request = build_token_request_rumor(
            member_two.pubkey,
            Timestamp::now(),
            vec![member_two_token.clone()],
        )
        .unwrap();
        let (member_two_event, _) =
            darkmatter_app_message_event(&member_two_session, &group_id, member_two_request).await;
        whitenoise
            .handle_mls_message(&admin_session, &admin_account, member_two_event.clone())
            .await
            .unwrap();
        whitenoise
            .handle_mls_message(&member_one_session, &member_one, member_two_event)
            .await
            .unwrap();

        assert_cached_peer_tokens(
            &admin_session,
            &group_id,
            &[
                (&member_one, member_one_leaf_index, &member_one_token),
                (&member_two, member_two_leaf_index, &member_two_token),
            ],
        )
        .await;
        assert_cached_peer_tokens(
            &member_one_session,
            &group_id,
            &[
                (&admin_account, admin_leaf_index, &admin_token),
                (&member_two, member_two_leaf_index, &member_two_token),
            ],
        )
        .await;
        assert_cached_peer_tokens(
            &member_two_session,
            &group_id,
            &[
                (&admin_account, admin_leaf_index, &admin_token),
                (&member_one, member_one_leaf_index, &member_one_token),
            ],
        )
        .await;

        admin_session.cancel();
        member_one_session.cancel();
        member_two_session.cancel();
    }

    #[tokio::test]
    async fn test_handle_darkmatter_mls_message_merges_token_list_response_into_group_push_cache() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (admin_account, _admin_keys, admin_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (member_account, _member_keys, member_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let group_id = setup_two_member_marmot_group_without_relay_publish(
            &admin_session,
            &member_session,
            admin_account.pubkey,
            member_account.pubkey,
        )
        .await;
        let admin_leaf_index =
            marmot_push_leaf_index(&member_session, &group_id, admin_account.pubkey).await;
        let member_leaf_index =
            marmot_push_leaf_index(&member_session, &group_id, member_account.pubkey).await;

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
                    leaf_index: admin_leaf_index,
                },
                LeafTokenTag {
                    token_tag: leaf_one.clone(),
                    leaf_index: member_leaf_index,
                },
                LeafTokenTag {
                    token_tag: inactive_leaf,
                    leaf_index: 99,
                },
            ],
        )
        .unwrap();
        let (event, _) = darkmatter_app_message_event(&admin_session, &group_id, response).await;

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
        let admin_token = stored
            .iter()
            .find(|token| token.leaf_index == admin_leaf_index)
            .expect("admin token should be cached by Darkmatter leaf index");
        assert_eq!(admin_token.member_pubkey, admin_account.pubkey);
        assert_eq!(
            admin_token.encrypted_token,
            leaf_zero.encrypted_token.to_base64()
        );
        let member_token = stored
            .iter()
            .find(|token| token.leaf_index == member_leaf_index)
            .expect("member token should be cached by Darkmatter leaf index");
        assert_eq!(member_token.member_pubkey, member_account.pubkey);
        assert_eq!(
            member_token.encrypted_token,
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
        let admin_session = whitenoise.require_session(&admin_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let group_id =
            setup_two_member_marmot_group(&whitenoise, &admin_account, &member_account).await;
        let admin_leaf_index =
            marmot_push_leaf_index(&member_session, &group_id, admin_account.pubkey).await;
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
        let (event, _) = darkmatter_app_message_event(&admin_session, &group_id, removal).await;

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
    async fn test_handle_darkmatter_mls_message_token_request_schedules_token_list_response() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (admin_account, _admin_keys, admin_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (member_account, _member_keys, member_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let group_id = setup_two_member_marmot_group_without_relay_publish(
            &admin_session,
            &member_session,
            admin_account.pubkey,
            member_account.pubkey,
        )
        .await;
        let member_leaf_index =
            marmot_push_leaf_index(&member_session, &group_id, member_account.pubkey).await;
        let member_token = make_darkmatter_token_tag(8);
        let member_token_fingerprint = member_token
            .token_fingerprint
            .clone()
            .expect("Darkmatter token fixture should include a fingerprint");
        member_session
            .repos
            .group_push_tokens
            .upsert_with_metadata(GroupPushTokenUpsert {
                mls_group_id: &group_id,
                member_pubkey: &member_account.pubkey,
                leaf_index: member_leaf_index,
                server_pubkey: &member_token.server_pubkey,
                relay_hint: Some(&member_token.relay_hint),
                encrypted_token: &member_token.encrypted_token.to_base64(),
                platform: Some(PushPlatform::Fcm),
                token_fingerprint: Some(&member_token_fingerprint),
            })
            .await
            .unwrap();
        let before_count = count_published_events_for_account(&whitenoise, &member_account).await;

        let token_tag = make_token_tag(5);
        let request =
            build_token_request_rumor(admin_account.pubkey, Timestamp::now(), vec![token_tag])
                .unwrap();
        let request_event_id = request.id.expect("447 rumor must have an event id");
        let (event, _) = darkmatter_app_message_event(&admin_session, &group_id, request).await;

        whitenoise
            .handle_mls_message(&member_session, &member_account, event)
            .await
            .unwrap();

        assert!(
            whitenoise
                .session(&member_account.pubkey)
                .unwrap()
                .pending_push_token_responses
                .contains_key(&(group_id.clone(), request_event_id))
        );

        let dispatched = whitenoise
            .require_session(&member_account.pubkey)
            .unwrap()
            .push()
            .dispatch_pending_token_response(&group_id, request_event_id)
            .await
            .unwrap();
        assert!(dispatched);

        let after_count =
            wait_for_published_event_count(&whitenoise, &member_account, before_count).await;
        assert!(after_count > before_count);
    }

    #[tokio::test]
    async fn test_handle_darkmatter_mls_message_matching_token_list_response_suppresses_pending_reply()
     {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (admin_account, _admin_keys, admin_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (member_account, _member_keys, member_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let group_id = setup_two_member_marmot_group_without_relay_publish(
            &admin_session,
            &member_session,
            admin_account.pubkey,
            member_account.pubkey,
        )
        .await;
        let admin_leaf_index =
            marmot_push_leaf_index(&member_session, &group_id, admin_account.pubkey).await;

        let request = build_token_request_rumor(
            admin_account.pubkey,
            Timestamp::now(),
            vec![make_token_tag(6)],
        )
        .unwrap();
        let request_event_id = request.id.expect("447 rumor must have an event id");
        let (request_event, _) =
            darkmatter_app_message_event(&admin_session, &group_id, request).await;

        whitenoise
            .handle_mls_message(&member_session, &member_account, request_event)
            .await
            .unwrap();

        assert!(
            whitenoise
                .session(&member_account.pubkey)
                .unwrap()
                .pending_push_token_responses
                .contains_key(&(group_id.clone(), request_event_id))
        );

        let response = build_token_list_response_rumor(
            admin_account.pubkey,
            Timestamp::now(),
            request_event_id,
            vec![LeafTokenTag {
                token_tag: make_token_tag(7),
                leaf_index: admin_leaf_index,
            }],
        )
        .unwrap();
        let (response_event, _) =
            darkmatter_app_message_event(&admin_session, &group_id, response).await;

        whitenoise
            .handle_mls_message(&member_session, &member_account, response_event)
            .await
            .unwrap();

        assert!(
            !whitenoise
                .session(&member_account.pubkey)
                .unwrap()
                .pending_push_token_responses
                .contains_key(&(group_id.clone(), request_event_id))
        );

        let dispatched = whitenoise
            .require_session(&member_account.pubkey)
            .unwrap()
            .push()
            .dispatch_pending_token_response(&group_id, request_event_id)
            .await
            .unwrap();
        assert!(!dispatched);
    }

    #[tokio::test]
    async fn test_handle_darkmatter_mls_message_commit_prunes_inactive_leaf_tokens() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (admin_account, _admin_keys, admin_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (member_one, _member_one_keys, member_one_session) =
            create_local_marmot_test_session(&whitenoise).await;
        let (member_two, _member_two_keys, member_two_session) =
            create_local_marmot_test_session(&whitenoise).await;

        let group_id = setup_marmot_group_without_relay_publish(
            &admin_session,
            admin_account.pubkey,
            &[
                (&member_one_session, member_one.pubkey),
                (&member_two_session, member_two.pubkey),
            ],
            GroupType::Group,
        )
        .await;
        let removed_leaf_index =
            marmot_push_leaf_index(&member_two_session, &group_id, member_one.pubkey).await;
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

        let removal_publisher = RecordingMarmotPublisher::default();
        admin_session
            .groups()
            .remove_marmot_members_with_publisher(
                &group_id,
                vec![member_one.pubkey],
                &removal_publisher,
            )
            .await
            .unwrap();
        let removal_message = removal_publisher
            .group_messages()
            .into_iter()
            .next()
            .expect("member removal should publish a group message");
        let removal_event = NostrTransportEvent::from_transport_message(&removal_message)
            .unwrap()
            .to_verified_nostr_event()
            .unwrap();

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

    #[tokio::test]
    async fn test_scheduled_token_response_task_runs_and_clears_pending_entry() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let admin_account = whitenoise.create_identity().await.unwrap();
        let admin_session = whitenoise.require_session(&admin_account.pubkey).unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();
        let member_session = whitenoise.require_session(&member_account.pubkey).unwrap();
        let group_id =
            setup_two_member_marmot_group(&whitenoise, &admin_account, &member_account).await;

        let token_tag = make_token_tag(22);
        let request =
            build_token_request_rumor(admin_account.pubkey, Timestamp::now(), vec![token_tag])
                .unwrap();
        let request_event_id = request.id.expect("447 rumor must have an event id");
        let (event, _) = darkmatter_app_message_event(&admin_session, &group_id, request).await;

        whitenoise
            .handle_mls_message(&member_session, &member_account, event)
            .await
            .unwrap();

        assert!(
            whitenoise
                .session(&member_account.pubkey)
                .unwrap()
                .pending_push_token_responses
                .contains_key(&(group_id.clone(), request_event_id))
        );

        // In tests the spawn delay is 0ms; wait for the spawned task to clear the entry.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !whitenoise
                    .session(&member_account.pubkey)
                    .unwrap()
                    .pending_push_token_responses
                    .contains_key(&(group_id.clone(), request_event_id))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for spawned token-response task to complete");
    }
}
