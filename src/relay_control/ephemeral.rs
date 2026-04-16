use std::{sync::Arc, time::Duration};

use mdk_core::prelude::GroupId;
use nostr_sdk::prelude::*;
use tokio::sync::mpsc::Sender;

use super::{
    ephemeral_executor::{EphemeralExecutor, EphemeralExecutorConfig},
    observability::RelayObservability,
    sessions::{RelaySessionAuthPolicy, RelaySessionReconnectPolicy},
};
use crate::{
    RelayType,
    nostr_manager::utils::{is_event_timestamp_valid, is_relay_list_tag_for_event_kind},
    nostr_manager::{NostrManagerError, Result},
    perf_instrument,
    types::{EphemeralPlaneStateSnapshot, ProcessableEvent},
    whitenoise::{
        accounts::Account,
        aggregated_message::AggregatedMessage,
        database::{Database, published_events::PublishedEvent},
        key_packages::{
            MLS_KEY_PACKAGE_KIND, MLS_KEY_PACKAGE_KIND_LEGACY, REQUIRED_MLS_CIPHERSUITE_TAG,
            is_key_package_kind, validate_marmot_key_package_tags,
        },
        message_aggregator::DeliveryStatus,
        message_streaming::{MessageStreamManager, MessageUpdate, UpdateTrigger},
    },
};

fn build_follow_list_event_builder(
    follow_list: &[PublicKey],
    created_at: Option<Timestamp>,
) -> EventBuilder {
    let tags: Vec<Tag> = follow_list
        .iter()
        .map(|pubkey| Tag::custom(TagKind::p(), [pubkey.to_hex()]))
        .collect();

    let builder = EventBuilder::new(Kind::ContactList, "").tags(tags);
    if let Some(created_at) = created_at {
        builder.custom_created_at(created_at)
    } else {
        builder
    }
}

/// Configuration for short-lived, targeted relay work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EphemeralPlaneConfig {
    pub(crate) timeout: Duration,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
    pub(crate) auth_policy: RelaySessionAuthPolicy,
    pub(crate) max_publish_attempts: u32,
    pub(crate) ad_hoc_relay_ttl: Duration,
}

impl Default for EphemeralPlaneConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            reconnect_policy: RelaySessionReconnectPolicy::Disabled,
            auth_policy: RelaySessionAuthPolicy::Disabled,
            max_publish_attempts: 3,
            ad_hoc_relay_ttl: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EphemeralPlane {
    config: EphemeralPlaneConfig,
    database: Arc<Database>,
    executor: EphemeralExecutor,
}

#[derive(Debug, Clone)]
pub(crate) struct EphemeralScope {
    plane: EphemeralPlane,
    scope_account_pubkey: Option<PublicKey>,
}

#[derive(Debug)]
pub(crate) enum MessagePublishResult {
    Published(Output<EventId>),
    Failed,
}

#[derive(Debug, Clone)]
pub(crate) enum KeyPackageLookup {
    Found(Event),
    Incompatible { event: Event, reason: String },
    NotFound,
}

impl MessagePublishResult {
    pub(crate) fn output(&self) -> Option<&Output<EventId>> {
        match self {
            Self::Published(output) => Some(output),
            Self::Failed => None,
        }
    }

    pub(crate) fn succeeded(&self) -> bool {
        self.output().is_some()
    }
}

impl EphemeralPlane {
    pub(crate) fn new(
        config: EphemeralPlaneConfig,
        database: Arc<Database>,
        event_sender: Sender<ProcessableEvent>,
        observability: RelayObservability,
    ) -> Self {
        Self {
            executor: EphemeralExecutor::new(
                EphemeralExecutorConfig {
                    timeout: config.timeout,
                    reconnect_policy: config.reconnect_policy,
                    auth_policy: config.auth_policy,
                    ad_hoc_relay_ttl: config.ad_hoc_relay_ttl,
                },
                database.clone(),
                event_sender,
                observability,
            ),
            config,
            database,
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn warm_relays(&self, relays: &[RelayUrl]) -> Result<()> {
        self.executor.warm_relays(relays).await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn warm_relays_for_account(
        &self,
        account_pubkey: PublicKey,
        relays: &[RelayUrl],
    ) -> Result<()> {
        self.executor
            .warm_relays_for_account(account_pubkey, relays)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn unwarm_relays(&self, relays: &[RelayUrl]) -> Result<()> {
        self.executor.unwarm_relays(relays).await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn remove_account_scope(&self, account_pubkey: &PublicKey) {
        self.executor.remove_account_scope(account_pubkey).await;
    }

    #[perf_instrument("relay")]
    pub(crate) async fn remove_all_scopes(&self) {
        self.executor.remove_all_scopes().await;
    }

    #[perf_instrument("relay")]
    pub(crate) async fn snapshot(&self) -> EphemeralPlaneStateSnapshot {
        let (anonymous, accounts) = self.executor.snapshot_scopes().await;

        EphemeralPlaneStateSnapshot {
            max_publish_attempts: self.config.max_publish_attempts,
            ad_hoc_relay_ttl_ms: self
                .config
                .ad_hoc_relay_ttl
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            anonymous,
            account_scope_count: accounts.len(),
            accounts,
        }
    }

    pub(crate) fn anonymous_scope(&self) -> EphemeralScope {
        EphemeralScope {
            plane: self.clone(),
            scope_account_pubkey: None,
        }
    }

    pub(crate) fn account_scope(&self, account_pubkey: PublicKey) -> EphemeralScope {
        EphemeralScope {
            plane: self.clone(),
            scope_account_pubkey: Some(account_pubkey),
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_metadata_from(
        &self,
        relays: &[RelayUrl],
        pubkey: PublicKey,
    ) -> Result<Option<Event>> {
        self.anonymous_scope()
            .fetch_metadata_from(relays, pubkey)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_user_relays(
        &self,
        pubkey: PublicKey,
        relay_type: RelayType,
        relays: &[RelayUrl],
    ) -> Result<Option<Event>> {
        self.anonymous_scope()
            .fetch_user_relays(pubkey, relay_type, relays)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_user_key_package(
        &self,
        pubkey: PublicKey,
        relays: &[RelayUrl],
    ) -> Result<Option<Event>> {
        match self.fetch_user_key_package_lookup(pubkey, relays).await? {
            KeyPackageLookup::Found(event) => Ok(Some(event)),
            KeyPackageLookup::Incompatible { .. } | KeyPackageLookup::NotFound => Ok(None),
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_user_key_package_lookup(
        &self,
        pubkey: PublicKey,
        relays: &[RelayUrl],
    ) -> Result<KeyPackageLookup> {
        self.anonymous_scope()
            .fetch_user_key_package_lookup(pubkey, relays)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_gift_wrap_to(
        &self,
        receiver: &PublicKey,
        rumor: UnsignedEvent,
        extra_tags: &[Tag],
        account_pubkey: PublicKey,
        relays: &[RelayUrl],
        signer: Arc<dyn NostrSigner>,
    ) -> Result<Output<EventId>> {
        let wrapped_event =
            EventBuilder::gift_wrap(&signer, receiver, rumor, extra_tags.to_vec()).await?;

        self.publish_event_to(wrapped_event, &account_pubkey, relays)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_metadata_with_signer(
        &self,
        metadata: &Metadata,
        relays: &[RelayUrl],
        signer: Arc<dyn NostrSigner>,
    ) -> Result<Output<EventId>> {
        self.publish_event_builder_with_signer(EventBuilder::metadata(metadata), relays, signer)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_relay_list_with_signer(
        &self,
        relay_list: &[RelayUrl],
        relay_type: RelayType,
        target_relays: &[RelayUrl],
        signer: Arc<dyn NostrSigner>,
    ) -> Result<()> {
        let tags: Vec<Tag> = match relay_type {
            RelayType::Nip65 => relay_list
                .iter()
                .map(|relay| Tag::reference(relay.to_string()))
                .collect(),
            RelayType::Inbox | RelayType::KeyPackage => relay_list
                .iter()
                .map(|relay| Tag::custom(TagKind::Relay, [relay.to_string()]))
                .collect(),
        };
        let event_builder = EventBuilder::new(relay_type.into(), "").tags(tags);
        let account_pubkey = signer.get_public_key().await?;
        let event = event_builder.sign(&signer).await?;

        self.publish_event_with_quorum(event, &account_pubkey, target_relays, 2)
            .await?;

        Ok(())
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_follow_list_with_signer(
        &self,
        follow_list: &[PublicKey],
        target_relays: &[RelayUrl],
        signer: Arc<dyn NostrSigner>,
    ) -> Result<()> {
        self.publish_follow_list_with_signer_at(follow_list, target_relays, None, signer)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_follow_list_with_signer_at(
        &self,
        follow_list: &[PublicKey],
        target_relays: &[RelayUrl],
        created_at: Option<Timestamp>,
        signer: Arc<dyn NostrSigner>,
    ) -> Result<()> {
        let event_builder = build_follow_list_event_builder(follow_list, created_at);
        self.publish_event_builder_with_signer(event_builder, target_relays, signer)
            .await?;

        Ok(())
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_key_package_with_signer(
        &self,
        kind: Kind,
        encoded_key_package: &str,
        relays: &[RelayUrl],
        tags: &[Tag],
        signer: Arc<dyn NostrSigner>,
    ) -> Result<Output<EventId>> {
        let event_builder = EventBuilder::new(kind, encoded_key_package).tags(tags.to_vec());

        self.publish_event_builder_with_signer(event_builder, relays, signer)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_event_deletion_with_signer(
        &self,
        event_id: &EventId,
        relays: &[RelayUrl],
        signer: Arc<dyn NostrSigner>,
    ) -> Result<Output<EventId>> {
        let event_builder = EventBuilder::delete(EventDeletionRequest::new().id(*event_id));

        self.publish_event_builder_with_signer(event_builder, relays, signer)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_batch_event_deletion_with_signer(
        &self,
        event_ids: &[EventId],
        relays: &[RelayUrl],
        signer: Arc<dyn NostrSigner>,
    ) -> Result<Output<EventId>> {
        if event_ids.is_empty() {
            return Err(NostrManagerError::WhitenoiseInstance(
                "Cannot publish batch deletion with empty event_ids list".to_string(),
            ));
        }

        let event_builder =
            EventBuilder::delete(EventDeletionRequest::new().ids(event_ids.iter().copied()));

        self.publish_event_builder_with_signer(event_builder, relays, signer)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_event_to(
        &self,
        event: Event,
        account_pubkey: &PublicKey,
        relays: &[RelayUrl],
    ) -> Result<Output<EventId>> {
        self.publish_event_to_scope(event, account_pubkey, relays, Some(*account_pubkey))
            .await
    }

    #[perf_instrument("relay")]
    async fn publish_event_to_scope(
        &self,
        event: Event,
        account_pubkey: &PublicKey,
        relays: &[RelayUrl],
        scope_account_pubkey: Option<PublicKey>,
    ) -> Result<Output<EventId>> {
        let mut last_error: Option<NostrManagerError> = None;

        for attempt in 0..self.config.max_publish_attempts {
            if attempt > 0 {
                let delay = Duration::from_secs(1 << attempt);
                tracing::debug!(
                    target: "whitenoise::relay_control::ephemeral",
                    account_pubkey = %account_pubkey,
                    attempt = attempt + 1,
                    max_attempts = self.config.max_publish_attempts,
                    ?delay,
                    "Retrying ephemeral publish after failure"
                );
                tokio::time::sleep(delay).await;
            }

            let result = self
                .executor
                .publish_event_to_scope(scope_account_pubkey, relays, &event)
                .await;

            match result {
                Ok(output) if !output.success.is_empty() => {
                    if let Err(error) = self
                        .track_published_event(output.id(), account_pubkey)
                        .await
                    {
                        tracing::warn!(
                            target: "whitenoise::relay_control::ephemeral",
                            account_pubkey = %account_pubkey,
                            event_id = %output.id(),
                            "Ephemeral publish succeeded but event tracking failed: {error}"
                        );
                    }

                    return Ok(output);
                }
                Ok(output) => {
                    last_error = Some(NostrManagerError::NoRelayAccepted);
                    tracing::warn!(
                        target: "whitenoise::relay_control::ephemeral",
                        account_pubkey = %account_pubkey,
                        attempt = attempt + 1,
                        max_attempts = self.config.max_publish_attempts,
                        failed_relays = ?output.failed.keys().collect::<Vec<_>>(),
                        "Ephemeral publish completed but no relay accepted the event"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        target: "whitenoise::relay_control::ephemeral",
                        account_pubkey = %account_pubkey,
                        attempt = attempt + 1,
                        max_attempts = self.config.max_publish_attempts,
                        "Ephemeral publish attempt failed: {error}"
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or(NostrManagerError::NoRelayConnections))
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_event_with_quorum(
        &self,
        event: Event,
        account_pubkey: &PublicKey,
        relays: &[RelayUrl],
        early_return_threshold: usize,
    ) -> Result<Output<EventId>> {
        let result = self
            .executor
            .publish_event_to_scope_quorum(
                Some(*account_pubkey),
                relays,
                &event,
                early_return_threshold,
            )
            .await?;

        if result.output.success.is_empty() {
            return Err(NostrManagerError::NoRelayAccepted);
        }

        if let Err(error) = self
            .track_published_event(result.output.id(), account_pubkey)
            .await
        {
            tracing::warn!(
                target: "whitenoise::relay_control::ephemeral",
                account_pubkey = %account_pubkey,
                event_id = %result.output.id(),
                "Quorum publish succeeded but event tracking failed: {error}"
            );
        }

        let retry_relays: Vec<RelayUrl> = result
            .pending
            .into_iter()
            .chain(result.output.failed.keys().cloned())
            .collect();

        if !retry_relays.is_empty() {
            let executor = self.executor.clone();
            let account_pubkey = *account_pubkey;

            tokio::spawn(async move {
                Self::retry_failed_relays(executor, event, account_pubkey, retry_relays).await;
            });
        }

        Ok(result.output)
    }

    async fn retry_failed_relays(
        executor: EphemeralExecutor,
        event: Event,
        account_pubkey: PublicKey,
        mut remaining_relays: Vec<RelayUrl>,
    ) {
        const MAX_RETRIES: u32 = 4;

        for attempt in 0..MAX_RETRIES {
            let delay = Duration::from_secs(2u64.pow(attempt + 1)); // 2s, 4s, 8s, 16s
            tokio::time::sleep(delay).await;

            match executor
                .publish_event_to_scope(Some(account_pubkey), &remaining_relays, &event)
                .await
            {
                Ok(output) => {
                    remaining_relays.retain(|u| !output.success.contains(u));

                    if remaining_relays.is_empty() {
                        tracing::debug!(
                            target: "whitenoise::relay_control::ephemeral",
                            account_pubkey = %account_pubkey,
                            event_id = %event.id,
                            attempt = attempt + 1,
                            "Background retry: all relays succeeded"
                        );
                        return;
                    }

                    tracing::debug!(
                        target: "whitenoise::relay_control::ephemeral",
                        account_pubkey = %account_pubkey,
                        event_id = %event.id,
                        attempt = attempt + 1,
                        succeeded = output.success.len(),
                        remaining = remaining_relays.len(),
                        "Background retry: partial progress"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::relay_control::ephemeral",
                        account_pubkey = %account_pubkey,
                        event_id = %event.id,
                        attempt = attempt + 1,
                        max_attempts = MAX_RETRIES,
                        "Background relay retry failed: {e}"
                    );
                }
            }
        }

        if !remaining_relays.is_empty() {
            tracing::warn!(
                target: "whitenoise::relay_control::ephemeral",
                account_pubkey = %account_pubkey,
                event_id = %event.id,
                relays = ?remaining_relays,
                "Background retry exhausted: {} relays never accepted the event",
                remaining_relays.len()
            );
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_message_event(
        &self,
        event: Event,
        account_pubkey: &PublicKey,
        relays: &[RelayUrl],
        event_id: &str,
        group_id: &GroupId,
        database: &Database,
        stream_manager: &Arc<MessageStreamManager>,
    ) -> MessagePublishResult {
        match self.publish_event_to(event, account_pubkey, relays).await {
            Ok(output) => {
                // Defensive boundary: publish_event_to currently returns
                // Err(NoRelayAccepted) for zero accepted relays, but keep the
                // delivery-status mapping correct if that contract changes.
                if output.success.is_empty() {
                    let status =
                        DeliveryStatus::Failed("No relay accepted the message".to_string());
                    Self::update_and_emit_delivery_status(
                        event_id,
                        group_id,
                        &status,
                        database,
                        stream_manager,
                    )
                    .await;
                    return MessagePublishResult::Failed;
                }

                let status = DeliveryStatus::Sent(output.success.len());
                Self::update_and_emit_delivery_status(
                    event_id,
                    group_id,
                    &status,
                    database,
                    stream_manager,
                )
                .await;
                MessagePublishResult::Published(output)
            }
            Err(error) => {
                let failure_message = match &error {
                    NostrManagerError::NoRelayAccepted => {
                        tracing::warn!(
                            target: "whitenoise::messages::delivery",
                            "Publish failed after bounded retries because no relay accepted the message"
                        );
                        "No relay accepted the message".to_string()
                    }
                    _ => {
                        tracing::warn!(
                            target: "whitenoise::messages::delivery",
                            "Publish failed after bounded retries: {error}"
                        );
                        error.to_string()
                    }
                };
                let status = DeliveryStatus::Failed(failure_message);
                Self::update_and_emit_delivery_status(
                    event_id,
                    group_id,
                    &status,
                    database,
                    stream_manager,
                )
                .await;
                MessagePublishResult::Failed
            }
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_events_from(
        &self,
        relays: &[RelayUrl],
        filter: Filter,
    ) -> Result<Events> {
        self.fetch_events_from_scope(None, relays, filter).await
    }

    #[perf_instrument("relay")]
    async fn fetch_events_from_scope(
        &self,
        scope_account_pubkey: Option<PublicKey>,
        relays: &[RelayUrl],
        filter: Filter,
    ) -> Result<Events> {
        self.executor
            .fetch_events_from_scope(scope_account_pubkey, relays, filter)
            .await
    }

    #[perf_instrument("relay")]
    async fn publish_event_builder_with_signer(
        &self,
        event_builder: EventBuilder,
        relays: &[RelayUrl],
        signer: Arc<dyn NostrSigner>,
    ) -> Result<Output<EventId>> {
        let account_pubkey = signer.get_public_key().await?;
        let event = event_builder.sign(&signer).await?;

        self.account_scope(account_pubkey)
            .publish_event_to(event, relays)
            .await
    }

    #[perf_instrument("relay")]
    async fn track_published_event(
        &self,
        event_id: &EventId,
        account_pubkey: &PublicKey,
    ) -> Result<()> {
        let account = Account::find_by_pubkey(account_pubkey, &self.database)
            .await
            .map_err(|error| NostrManagerError::FailedToTrackPublishedEvent(error.to_string()))?;
        let account_id = account.id.ok_or_else(|| {
            NostrManagerError::FailedToTrackPublishedEvent(
                "Account missing id while tracking ephemeral publish".to_string(),
            )
        })?;

        PublishedEvent::create(event_id, account_id, &self.database)
            .await
            .map_err(|error| NostrManagerError::FailedToTrackPublishedEvent(error.to_string()))?;

        Ok(())
    }

    #[perf_instrument("relay")]
    async fn update_and_emit_delivery_status(
        event_id: &str,
        group_id: &GroupId,
        status: &DeliveryStatus,
        database: &Database,
        stream_manager: &MessageStreamManager,
    ) {
        match AggregatedMessage::update_delivery_status_with_retry(
            event_id, group_id, status, database,
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

    fn latest_from_events_with_validation<F>(
        events: Events,
        expected_kind: Kind,
        is_semantically_valid: F,
    ) -> Result<Option<Event>>
    where
        F: Fn(&Event) -> bool,
    {
        let timestamp_valid_events: Vec<Event> = events
            .into_iter()
            .filter(|event| event.kind == expected_kind)
            .filter(is_event_timestamp_valid)
            .collect();

        let latest_timestamp_valid = timestamp_valid_events
            .iter()
            .max_by_key(|event| (event.created_at, event.id))
            .cloned();

        let latest_semantically_valid = timestamp_valid_events
            .iter()
            .filter(|event| is_semantically_valid(event))
            .max_by_key(|event| (event.created_at, event.id))
            .cloned();

        if latest_semantically_valid.is_none() && latest_timestamp_valid.is_some() {
            tracing::warn!(
                target: "whitenoise::relay_control::ephemeral",
                expected_kind = %expected_kind,
                "No semantically valid event found after timestamp checks; falling back to latest timestamp-valid event"
            );
        }

        Ok(latest_semantically_valid.or(latest_timestamp_valid))
    }

    fn latest_valid_metadata_from_events(events: Events) -> Option<Event> {
        events
            .into_iter()
            .filter(|event| event.kind == Kind::Metadata)
            .filter(is_event_timestamp_valid)
            .filter(Self::is_metadata_event_semantically_valid)
            .max_by_key(|event| (event.created_at, event.id))
    }

    fn is_metadata_event_semantically_valid(event: &Event) -> bool {
        Metadata::from_json(&event.content).is_ok()
    }

    fn is_relay_event_semantically_valid(event: &Event) -> bool {
        let relay_tags: Vec<&Tag> = event
            .tags
            .iter()
            .filter(|tag| is_relay_list_tag_for_event_kind(tag, event.kind))
            .collect();

        // An empty relay list is a valid authoritative statement only when the event itself
        // carries no tags at all (i.e. the author intentionally published an empty list).
        // If the event has tags but none are relay-list tags the event is malformed.
        if relay_tags.is_empty() {
            return event.tags.is_empty();
        }

        relay_tags.iter().any(|tag| {
            tag.content()
                .and_then(|content| RelayUrl::parse(content).ok())
                .is_some()
        })
    }

    fn is_key_package_event_semantically_valid(event: &Event) -> bool {
        validate_marmot_key_package_tags(event, REQUIRED_MLS_CIPHERSUITE_TAG).is_ok()
    }

    pub(crate) fn key_package_lookup_from_events(events: Events) -> KeyPackageLookup {
        let mut valid_current = Vec::new();
        let mut valid_legacy = Vec::new();
        let mut incompatible = Vec::new();

        for event in events
            .into_iter()
            .filter(|event| is_key_package_kind(event.kind))
            .filter(is_event_timestamp_valid)
        {
            match validate_marmot_key_package_tags(&event, REQUIRED_MLS_CIPHERSUITE_TAG) {
                Ok(()) if event.kind == MLS_KEY_PACKAGE_KIND => valid_current.push(event),
                Ok(()) if event.kind == MLS_KEY_PACKAGE_KIND_LEGACY => valid_legacy.push(event),
                Ok(()) => {}
                Err(error) => incompatible.push((event, error.to_string())),
            }
        }

        if let Some(event) = valid_current
            .into_iter()
            .max_by_key(|event| (event.created_at, event.id))
        {
            return KeyPackageLookup::Found(event);
        }

        if let Some(event) = valid_legacy
            .into_iter()
            .max_by_key(|event| (event.created_at, event.id))
        {
            return KeyPackageLookup::Found(event);
        }

        match incompatible
            .into_iter()
            .max_by_key(|(event, _)| (event.created_at, event.id))
        {
            Some((event, reason)) => KeyPackageLookup::Incompatible { event, reason },
            None => KeyPackageLookup::NotFound,
        }
    }
}

impl EphemeralScope {
    #[perf_instrument("relay")]
    pub(crate) async fn fetch_events_from(
        &self,
        relays: &[RelayUrl],
        filter: Filter,
    ) -> Result<Events> {
        self.plane
            .fetch_events_from_scope(self.scope_account_pubkey, relays, filter)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_metadata_from(
        &self,
        relays: &[RelayUrl],
        pubkey: PublicKey,
    ) -> Result<Option<Event>> {
        let filter = Filter::new().author(pubkey).kind(Kind::Metadata);
        let events = self.fetch_events_from(relays, filter).await?;

        Ok(EphemeralPlane::latest_valid_metadata_from_events(events))
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_user_relays(
        &self,
        pubkey: PublicKey,
        relay_type: RelayType,
        relays: &[RelayUrl],
    ) -> Result<Option<Event>> {
        let filter = Filter::new().author(pubkey).kind(relay_type.into());
        let events = self.fetch_events_from(relays, filter).await?;

        EphemeralPlane::latest_from_events_with_validation(events, relay_type.into(), |event| {
            EphemeralPlane::is_relay_event_semantically_valid(event)
        })
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_user_key_package(
        &self,
        pubkey: PublicKey,
        relays: &[RelayUrl],
    ) -> Result<Option<Event>> {
        match self.fetch_user_key_package_lookup(pubkey, relays).await? {
            KeyPackageLookup::Found(event) => Ok(Some(event)),
            KeyPackageLookup::Incompatible { .. } | KeyPackageLookup::NotFound => Ok(None),
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_user_key_package_lookup(
        &self,
        pubkey: PublicKey,
        relays: &[RelayUrl],
    ) -> Result<KeyPackageLookup> {
        let filter = Filter::new()
            .kinds([MLS_KEY_PACKAGE_KIND, MLS_KEY_PACKAGE_KIND_LEGACY])
            .author(pubkey);
        let events = self.fetch_events_from(relays, filter).await?;

        Ok(EphemeralPlane::key_package_lookup_from_events(events))
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_event_to(
        &self,
        event: Event,
        relays: &[RelayUrl],
    ) -> Result<Output<EventId>> {
        let account_pubkey = self.scope_account_pubkey.ok_or_else(|| {
            NostrManagerError::WhitenoiseInstance(
                "Cannot publish from an anonymous ephemeral scope".to_string(),
            )
        })?;

        self.plane
            .publish_event_to_scope(event, &account_pubkey, relays, self.scope_account_pubkey)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf, time::SystemTime};

    use sqlx::sqlite::SqlitePoolOptions;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        relay_control::{
            RelayPlane,
            observability::{RelayObservabilityConfig, RelayTelemetryKind},
            sessions::{RelaySession, RelaySessionConfig},
        },
        whitenoise::{
            database::{Database, relay_events::RelayEventRecord},
            key_packages::REQUIRED_MLS_PROPOSAL_TAGS,
        },
    };

    async fn setup_test_db() -> Database {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        let database = Database {
            pool,
            path: PathBuf::from(":memory:"),
            last_connected: SystemTime::now(),
        };
        database.migrate_up().await.unwrap();
        database
    }

    fn test_plane(database: Arc<Database>, config: EphemeralPlaneConfig) -> EphemeralPlane {
        let (event_sender, _) = mpsc::channel(16);
        EphemeralPlane::new(
            config,
            database,
            event_sender,
            RelayObservability::new(RelayObservabilityConfig::default()),
        )
    }

    #[test]
    fn test_message_publish_result_helpers() {
        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let event_id = EventId::from_hex(&"11".repeat(32)).unwrap();
        let output = Output {
            val: event_id,
            success: std::collections::HashSet::from([relay_url]),
            failed: HashMap::new(),
        };
        let published = MessagePublishResult::Published(output);

        assert_eq!(published.output().unwrap().id(), &event_id);
        assert!(published.succeeded());
        assert!(MessagePublishResult::Failed.output().is_none());
        assert!(!MessagePublishResult::Failed.succeeded());
    }

    #[tokio::test]
    async fn test_giftwrap_uses_ephemeral_outer_key() {
        let sender_keys = Keys::generate();
        let receiver_keys = Keys::generate();
        let rumor = UnsignedEvent::new(
            sender_keys.public_key(),
            Timestamp::now(),
            Kind::TextNote,
            vec![],
            "security check".to_string(),
        );

        let wrapped_event =
            EventBuilder::gift_wrap(&sender_keys, &receiver_keys.public_key(), rumor, vec![])
                .await
                .unwrap();

        assert_ne!(
            wrapped_event.pubkey,
            sender_keys.public_key(),
            "Giftwrap should use an ephemeral outer key, not the sender key"
        );
    }

    #[tokio::test]
    async fn test_build_follow_list_event_builder_uses_custom_created_at() {
        let keys = Keys::generate();
        let follow_list = vec![Keys::generate().public_key()];
        let created_at = Timestamp::from(1_234_567_890_u64);

        let event = build_follow_list_event_builder(&follow_list, Some(created_at))
            .sign(&keys)
            .await
            .unwrap();

        assert_eq!(event.created_at, created_at);
    }

    #[tokio::test]
    async fn test_executor_reuses_sessions_by_scope() {
        let database = Arc::new(setup_test_db().await);
        let plane = test_plane(database, EphemeralPlaneConfig::default());

        let anonymous_a = plane.executor.session_for_scope(None).await;
        let anonymous_b = plane.executor.session_for_scope(None).await;
        let relay_url = RelayUrl::parse("ws://127.0.0.1:1").unwrap();

        anonymous_a
            .client()
            .add_relay(relay_url.clone())
            .await
            .unwrap();

        assert!(anonymous_b.client().relay(&relay_url).await.is_ok());

        let scoped = plane
            .executor
            .session_for_scope(Some(Keys::generate().public_key()))
            .await;
        assert!(scoped.client().relay(&relay_url).await.is_err());
    }

    #[tokio::test]
    async fn test_ad_hoc_relays_are_evicted_after_ttl() {
        let database = Arc::new(setup_test_db().await);
        let plane = test_plane(
            database,
            EphemeralPlaneConfig {
                timeout: Duration::from_millis(5),
                reconnect_policy: RelaySessionReconnectPolicy::Disabled,
                auth_policy: RelaySessionAuthPolicy::Disabled,
                max_publish_attempts: 1,
                ad_hoc_relay_ttl: Duration::from_millis(20),
            },
        );
        let scope = plane.anonymous_scope();
        let relay_a = RelayUrl::parse("ws://127.0.0.1:1").unwrap();
        let relay_b = RelayUrl::parse("ws://127.0.0.1:2").unwrap();
        let filter = Filter::new().kind(Kind::Metadata);

        let _ = scope
            .fetch_events_from(std::slice::from_ref(&relay_a), filter.clone())
            .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let _ = scope
            .fetch_events_from(std::slice::from_ref(&relay_b), filter)
            .await;

        let anonymous = plane.executor.session_for_scope(None).await;
        assert!(anonymous.client().relay(&relay_a).await.is_err());
        assert!(anonymous.client().relay(&relay_b).await.is_ok());
    }

    #[tokio::test]
    async fn test_pinned_relays_survive_ad_hoc_eviction() {
        let database = Arc::new(setup_test_db().await);
        let plane = test_plane(
            database,
            EphemeralPlaneConfig {
                timeout: Duration::from_millis(5),
                reconnect_policy: RelaySessionReconnectPolicy::Disabled,
                auth_policy: RelaySessionAuthPolicy::Disabled,
                max_publish_attempts: 1,
                ad_hoc_relay_ttl: Duration::from_millis(20),
            },
        );
        let pinned_relay = RelayUrl::parse("ws://127.0.0.1:3").unwrap();
        let ad_hoc_relay = RelayUrl::parse("ws://127.0.0.1:4").unwrap();
        let scope = plane.anonymous_scope();

        let _ = plane.warm_relays(std::slice::from_ref(&pinned_relay)).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let _ = scope
            .fetch_events_from(
                std::slice::from_ref(&ad_hoc_relay),
                Filter::new().kind(Kind::Metadata),
            )
            .await;

        let anonymous = plane.executor.session_for_scope(None).await;
        assert!(anonymous.client().relay(&pinned_relay).await.is_ok());
    }

    // Use a loopback URL so there is no DNS lookup and connection refusal is instant.
    // Time is paused only around the publish call so that retry backoff sleeps
    // complete without burning real seconds, but DB setup runs with real time.
    #[tokio::test]
    async fn test_publish_does_not_mutate_other_session_state() {
        let database = Arc::new(setup_test_db().await);
        let plane = test_plane(database, EphemeralPlaneConfig::default());

        let (event_sender, _) = mpsc::channel(8);
        let long_lived_session =
            RelaySession::new(RelaySessionConfig::new(RelayPlane::Discovery), event_sender);
        let long_lived_relay = RelayUrl::parse("ws://127.0.0.1:1").unwrap();
        long_lived_session
            .client()
            .add_relay(long_lived_relay.clone())
            .await
            .unwrap();

        let before = long_lived_session
            .snapshot(std::slice::from_ref(&long_lived_relay))
            .await;

        let sender_keys = Keys::generate();
        let receiver_keys = Keys::generate();
        let rumor = UnsignedEvent::new(
            sender_keys.public_key(),
            Timestamp::now(),
            Kind::TextNote,
            vec![],
            "ephemeral welcome".to_string(),
        );
        let target_relays = [RelayUrl::parse("ws://127.0.0.1:1").unwrap()];

        tokio::time::pause();
        let _ = plane
            .publish_gift_wrap_to(
                &receiver_keys.public_key(),
                rumor,
                &[],
                sender_keys.public_key(),
                &target_relays,
                Arc::new(sender_keys),
            )
            .await;
        tokio::time::resume();

        let after = long_lived_session
            .snapshot(std::slice::from_ref(&long_lived_relay))
            .await;

        let before_relays = before
            .relays
            .iter()
            .map(|relay| relay.relay_url.clone())
            .collect::<Vec<_>>();
        let after_relays = after
            .relays
            .iter()
            .map(|relay| relay.relay_url.clone())
            .collect::<Vec<_>>();

        assert_eq!(before_relays, after_relays);
        assert_eq!(
            before.registered_subscription_count,
            after.registered_subscription_count
        );

        long_lived_session.shutdown().await;
    }

    // Time is paused only around the publish call so that retry backoff sleeps
    // complete without burning real seconds, but DB setup runs with real time.
    // After the publish we resume real time and wait briefly for the background
    // telemetry-persistor task to flush its records to the database.
    #[tokio::test]
    async fn test_publish_attempts_are_bounded_and_persisted() {
        let database = Arc::new(setup_test_db().await);
        let plane = test_plane(
            database.clone(),
            EphemeralPlaneConfig {
                timeout: Duration::from_millis(10),
                reconnect_policy: RelaySessionReconnectPolicy::Disabled,
                auth_policy: RelaySessionAuthPolicy::Disabled,
                max_publish_attempts: 2,
                ad_hoc_relay_ttl: Duration::from_millis(50),
            },
        );

        let sender_keys = Keys::generate();
        let receiver_keys = Keys::generate();
        let account_pubkey = sender_keys.public_key();
        let relay_url = RelayUrl::parse("ws://127.0.0.1:1").unwrap();
        let rumor = UnsignedEvent::new(
            account_pubkey,
            Timestamp::now(),
            Kind::TextNote,
            vec![],
            "retry test".to_string(),
        );

        tokio::time::pause();
        let _ = plane
            .publish_gift_wrap_to(
                &receiver_keys.public_key(),
                rumor,
                &[],
                account_pubkey,
                std::slice::from_ref(&relay_url),
                Arc::new(sender_keys),
            )
            .await;
        tokio::time::resume();

        // Wait briefly for the background telemetry-persistor task to flush
        // its records to the database before we query it.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let events = RelayEventRecord::list_recent_for_scope(
            &relay_url,
            RelayPlane::Ephemeral,
            Some(account_pubkey),
            10,
            &database,
        )
        .await
        .unwrap();

        // PublishAttempt is no longer persisted to relay_events (Fix 6).
        // Each failed attempt emits PublishFailure instead (Fix 4).
        let publish_failures = events
            .iter()
            .filter(|event| event.kind == RelayTelemetryKind::PublishFailure)
            .count();

        assert_eq!(publish_failures, 2);
    }

    /// Newer incompatible key package must not shadow an older valid one.
    #[test]
    fn test_latest_key_package_prefers_marmot_compatible_over_newer_incompatible() {
        let keys = Keys::generate();
        let now = Timestamp::now();
        let earlier = Timestamp::from(now.as_secs() - 60);

        // Older event: fully Marmot-compatible
        let older_valid = EventBuilder::new(Kind::MlsKeyPackage, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                [REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a", "0xF2EE"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_proposals".into()),
                ["0x000a"],
            ))
            .custom_created_at(earlier)
            .sign_with_keys(&keys)
            .unwrap();

        // Newer event: has encoding tag + non-empty content but wrong ciphersuite
        let newer_incompatible = EventBuilder::new(Kind::MlsKeyPackage, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                ["0x9999"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a", "0xF2EE"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_proposals".into()),
                ["0x000a"],
            ))
            .custom_created_at(now)
            .sign_with_keys(&keys)
            .unwrap();

        assert!(newer_incompatible.created_at > older_valid.created_at);

        let filter = Filter::new()
            .kind(Kind::MlsKeyPackage)
            .author(keys.public_key());
        let mut events = Events::new(&filter);
        events.insert(older_valid.clone());
        events.insert(newer_incompatible);
        let result = EphemeralPlane::latest_from_events_with_validation(
            events,
            Kind::MlsKeyPackage,
            EphemeralPlane::is_key_package_event_semantically_valid,
        )
        .unwrap();

        assert_eq!(
            result.as_ref().map(|e| e.id),
            Some(older_valid.id),
            "Should prefer the older Marmot-compatible event over the newer incompatible one"
        );
    }

    /// When all candidates are Marmot-compatible, the newest wins.
    #[test]
    fn test_latest_key_package_picks_newest_when_all_compatible() {
        let keys = Keys::generate();
        let now = Timestamp::now();
        let earlier = Timestamp::from(now.as_secs() - 60);

        let older = EventBuilder::new(Kind::MlsKeyPackage, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                [REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a", "0xF2EE"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_proposals".into()),
                ["0x000a"],
            ))
            .custom_created_at(earlier)
            .sign_with_keys(&keys)
            .unwrap();

        let newer = EventBuilder::new(Kind::MlsKeyPackage, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                [REQUIRED_MLS_CIPHERSUITE_TAG],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a", "0xF2EE"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_proposals".into()),
                ["0x000a"],
            ))
            .custom_created_at(now)
            .sign_with_keys(&keys)
            .unwrap();

        let filter = Filter::new()
            .kind(Kind::MlsKeyPackage)
            .author(keys.public_key());
        let mut events = Events::new(&filter);
        events.insert(older);
        events.insert(newer.clone());

        let result = EphemeralPlane::latest_from_events_with_validation(
            events,
            Kind::MlsKeyPackage,
            EphemeralPlane::is_key_package_event_semantically_valid,
        )
        .unwrap();

        assert_eq!(
            result.as_ref().map(|e| e.id),
            Some(newer.id),
            "Should pick the newest event when all are Marmot-compatible"
        );
    }

    /// When no candidate is Marmot-compatible, falls back to the latest
    /// timestamp-valid event among the incompatible ones.
    #[test]
    fn test_latest_key_package_falls_back_when_none_compatible() {
        let keys = Keys::generate();
        let now = Timestamp::now();
        let earlier = Timestamp::from(now.as_secs() - 60);

        let older_incompatible = EventBuilder::new(Kind::MlsKeyPackage, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                ["0x9999"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a", "0xF2EE"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_proposals".into()),
                ["0x000a"],
            ))
            .custom_created_at(earlier)
            .sign_with_keys(&keys)
            .unwrap();

        let newer_incompatible = EventBuilder::new(Kind::MlsKeyPackage, "dGVzdF9jb250ZW50")
            .tag(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]))
            .tag(Tag::custom(
                TagKind::Custom("mls_ciphersuite".into()),
                ["0x8888"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_extensions".into()),
                ["0x000a", "0xF2EE"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("mls_proposals".into()),
                ["0x000a"],
            ))
            .custom_created_at(now)
            .sign_with_keys(&keys)
            .unwrap();

        let filter = Filter::new()
            .kind(Kind::MlsKeyPackage)
            .author(keys.public_key());
        let mut events = Events::new(&filter);
        events.insert(older_incompatible);
        events.insert(newer_incompatible.clone());

        let result = EphemeralPlane::latest_from_events_with_validation(
            events,
            Kind::MlsKeyPackage,
            EphemeralPlane::is_key_package_event_semantically_valid,
        )
        .unwrap();

        assert_eq!(
            result.as_ref().map(|e| e.id),
            Some(newer_incompatible.id),
            "Should fall back to the newest timestamp-valid event when none are Marmot-compatible"
        );
    }

    fn key_package_tags(kind: Kind, include_encoding: bool, proposals: &[&str]) -> Vec<Tag> {
        let mut tags = Vec::new();
        if kind == MLS_KEY_PACKAGE_KIND {
            tags.push(Tag::identifier(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ));
        }
        tags.push(Tag::custom(
            TagKind::Custom("mls_ciphersuite".into()),
            [REQUIRED_MLS_CIPHERSUITE_TAG],
        ));
        tags.push(Tag::custom(
            TagKind::Custom("mls_extensions".into()),
            ["0x000a", "0xf2ee"],
        ));
        if !proposals.is_empty() {
            tags.push(Tag::custom(
                TagKind::Custom("mls_proposals".into()),
                proposals.iter().copied(),
            ));
        }
        if include_encoding {
            tags.push(Tag::custom(TagKind::Custom("encoding".into()), ["base64"]));
        }
        tags
    }

    fn signed_key_package_event(
        keys: &Keys,
        kind: Kind,
        created_at: Timestamp,
        include_encoding: bool,
        proposals: &[&str],
    ) -> Event {
        EventBuilder::new(kind, "dGVzdF9jb250ZW50")
            .tags(key_package_tags(kind, include_encoding, proposals))
            .custom_created_at(created_at)
            .sign_with_keys(keys)
            .unwrap()
    }

    fn key_package_events(keys: &Keys, events: Vec<Event>) -> Events {
        let filter = Filter::new()
            .kinds([MLS_KEY_PACKAGE_KIND, MLS_KEY_PACKAGE_KIND_LEGACY])
            .author(keys.public_key());
        let mut event_set = Events::new(&filter);
        for event in events {
            event_set.insert(event);
        }
        event_set
    }

    #[test]
    fn test_key_package_lookup_prefers_valid_current_over_newer_valid_legacy() {
        let keys = Keys::generate();
        let now = Timestamp::now();
        let older = Timestamp::from(now.as_secs() - 60);

        let current = signed_key_package_event(
            &keys,
            MLS_KEY_PACKAGE_KIND,
            older,
            true,
            &REQUIRED_MLS_PROPOSAL_TAGS,
        );
        let legacy = signed_key_package_event(
            &keys,
            MLS_KEY_PACKAGE_KIND_LEGACY,
            now,
            true,
            &REQUIRED_MLS_PROPOSAL_TAGS,
        );

        let lookup = EphemeralPlane::key_package_lookup_from_events(key_package_events(
            &keys,
            vec![current.clone(), legacy],
        ));

        assert!(matches!(lookup, KeyPackageLookup::Found(event) if event.id == current.id));
    }

    #[test]
    fn test_key_package_lookup_accepts_valid_legacy_without_current() {
        let keys = Keys::generate();
        let legacy = signed_key_package_event(
            &keys,
            MLS_KEY_PACKAGE_KIND_LEGACY,
            Timestamp::now(),
            true,
            &REQUIRED_MLS_PROPOSAL_TAGS,
        );

        let lookup = EphemeralPlane::key_package_lookup_from_events(key_package_events(
            &keys,
            vec![legacy.clone()],
        ));

        assert!(matches!(lookup, KeyPackageLookup::Found(event) if event.id == legacy.id));
    }

    #[test]
    fn test_key_package_lookup_ignores_newer_invalid_current() {
        let keys = Keys::generate();
        let now = Timestamp::now();
        let older = Timestamp::from(now.as_secs() - 60);

        let older_valid = signed_key_package_event(
            &keys,
            MLS_KEY_PACKAGE_KIND,
            older,
            true,
            &REQUIRED_MLS_PROPOSAL_TAGS,
        );
        let newer_invalid = signed_key_package_event(&keys, MLS_KEY_PACKAGE_KIND, now, true, &[]);

        let lookup = EphemeralPlane::key_package_lookup_from_events(key_package_events(
            &keys,
            vec![older_valid.clone(), newer_invalid],
        ));

        assert!(matches!(lookup, KeyPackageLookup::Found(event) if event.id == older_valid.id));
    }

    #[test]
    fn test_key_package_lookup_reports_missing_self_remove_as_incompatible() {
        let keys = Keys::generate();
        let incompatible = signed_key_package_event(
            &keys,
            MLS_KEY_PACKAGE_KIND_LEGACY,
            Timestamp::now(),
            true,
            &[],
        );

        let lookup = EphemeralPlane::key_package_lookup_from_events(key_package_events(
            &keys,
            vec![incompatible.clone()],
        ));

        assert!(matches!(
            lookup,
            KeyPackageLookup::Incompatible { event, reason }
                if event.id == incompatible.id && reason.contains("mls_proposals")
        ));
    }

    #[test]
    fn test_key_package_lookup_reports_missing_encoding_as_incompatible() {
        let keys = Keys::generate();
        let incompatible = signed_key_package_event(
            &keys,
            MLS_KEY_PACKAGE_KIND_LEGACY,
            Timestamp::now(),
            false,
            &REQUIRED_MLS_PROPOSAL_TAGS,
        );

        let lookup = EphemeralPlane::key_package_lookup_from_events(key_package_events(
            &keys,
            vec![incompatible.clone()],
        ));

        assert!(matches!(
            lookup,
            KeyPackageLookup::Incompatible { event, reason }
                if event.id == incompatible.id && reason.contains("encoding")
        ));
    }

    #[test]
    fn test_metadata_fetch_prefers_newest_semantically_valid_kind0() {
        let keys = Keys::generate();
        let oldest = Timestamp::from(1_700_000_000u64);
        let newer = Timestamp::from(oldest.as_secs() + 60);
        let newest_invalid = Timestamp::from(newer.as_secs() + 60);

        let older_valid = EventBuilder::metadata(&Metadata::new().name("older"))
            .custom_created_at(oldest)
            .sign_with_keys(&keys)
            .unwrap();
        let newer_valid = EventBuilder::metadata(&Metadata::new().name("newer"))
            .custom_created_at(newer)
            .sign_with_keys(&keys)
            .unwrap();
        let invalid_latest = EventBuilder::new(Kind::Metadata, "{invalid")
            .custom_created_at(newest_invalid)
            .sign_with_keys(&keys)
            .unwrap();

        let filter = Filter::new().kind(Kind::Metadata).author(keys.public_key());
        let mut events = Events::new(&filter);
        events.insert(older_valid);
        events.insert(newer_valid.clone());
        events.insert(invalid_latest);

        let result = EphemeralPlane::latest_valid_metadata_from_events(events);

        assert_eq!(result.as_ref().map(|event| event.id), Some(newer_valid.id));
    }

    #[test]
    fn test_metadata_fetch_returns_none_when_only_invalid_kind0_events_exist() {
        let keys = Keys::generate();
        let now = Timestamp::from(1_700_000_000u64);
        let later = Timestamp::from(now.as_secs() + 60);

        let invalid_a = EventBuilder::new(Kind::Metadata, "{invalid")
            .custom_created_at(now)
            .sign_with_keys(&keys)
            .unwrap();
        let invalid_b = EventBuilder::new(Kind::Metadata, "not json")
            .custom_created_at(later)
            .sign_with_keys(&keys)
            .unwrap();

        let filter = Filter::new().kind(Kind::Metadata).author(keys.public_key());
        let mut events = Events::new(&filter);
        events.insert(invalid_a);
        events.insert(invalid_b);

        let result = EphemeralPlane::latest_valid_metadata_from_events(events);

        assert!(result.is_none());
    }

    #[test]
    fn test_metadata_fetch_does_not_fall_back_to_invalid_latest_event() {
        let keys = Keys::generate();
        let earlier = Timestamp::from(1_700_000_000u64);
        let later = Timestamp::from(earlier.as_secs() + 60);

        let valid_older = EventBuilder::metadata(&Metadata::new().name("valid"))
            .custom_created_at(earlier)
            .sign_with_keys(&keys)
            .unwrap();
        let invalid_latest = EventBuilder::new(Kind::Metadata, "{invalid")
            .custom_created_at(later)
            .sign_with_keys(&keys)
            .unwrap();

        let filter = Filter::new().kind(Kind::Metadata).author(keys.public_key());
        let mut events = Events::new(&filter);
        events.insert(valid_older.clone());
        events.insert(invalid_latest);

        let result = EphemeralPlane::latest_valid_metadata_from_events(events);

        assert_eq!(result.as_ref().map(|event| event.id), Some(valid_older.id));
    }
}
