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
        key_packages::has_encoding_tag,
        message_aggregator::DeliveryStatus,
        message_streaming::{MessageStreamManager, MessageUpdate, UpdateTrigger},
    },
};

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

    #[cfg(feature = "integration-tests")]
    #[perf_instrument("relay")]
    pub(crate) async fn remove_all_account_scopes(&self) {
        self.executor.remove_all_account_scopes().await;
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
        self.anonymous_scope()
            .fetch_user_key_package(pubkey, relays)
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

        self.publish_event_builder_with_signer(event_builder, target_relays, signer)
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
        let tags: Vec<Tag> = follow_list
            .iter()
            .map(|pubkey| Tag::custom(TagKind::p(), [pubkey.to_hex()]))
            .collect();
        let event_builder = EventBuilder::new(Kind::ContactList, "").tags(tags);

        self.publish_event_builder_with_signer(event_builder, target_relays, signer)
            .await?;

        Ok(())
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_key_package_with_signer(
        &self,
        encoded_key_package: &str,
        relays: &[RelayUrl],
        tags: &[Tag],
        signer: Arc<dyn NostrSigner>,
    ) -> Result<Output<EventId>> {
        let event_builder =
            EventBuilder::new(Kind::MlsKeyPackage, encoded_key_package).tags(tags.to_vec());

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
    pub(crate) async fn publish_message_event(
        &self,
        event: Event,
        account_pubkey: &PublicKey,
        relays: &[RelayUrl],
        event_id: &str,
        group_id: &GroupId,
        database: &Database,
        stream_manager: &Arc<MessageStreamManager>,
    ) -> bool {
        match self.publish_event_to(event, account_pubkey, relays).await {
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
                true
            }
            Ok(output) => {
                tracing::warn!(
                    target: "whitenoise::messages::delivery",
                    "Publish completed but no relay accepted the message (failed: {:?})",
                    output.failed.keys().collect::<Vec<_>>(),
                );
                let status = DeliveryStatus::Failed("No relay accepted the message".to_string());
                Self::update_and_emit_delivery_status(
                    event_id,
                    group_id,
                    &status,
                    database,
                    stream_manager,
                )
                .await;
                false
            }
            Err(error) => {
                tracing::warn!(
                    target: "whitenoise::messages::delivery",
                    "Publish failed after bounded retries: {error}"
                );
                let status = DeliveryStatus::Failed(error.to_string());
                Self::update_and_emit_delivery_status(
                    event_id,
                    group_id,
                    &status,
                    database,
                    stream_manager,
                )
                .await;
                false
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
        has_encoding_tag(event) && !event.content.trim().is_empty()
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

        EphemeralPlane::latest_from_events_with_validation(events, Kind::Metadata, |event| {
            EphemeralPlane::is_metadata_event_semantically_valid(event)
        })
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
        let filter = Filter::new().kind(Kind::MlsKeyPackage).author(pubkey);
        let events = self.fetch_events_from(relays, filter).await?;

        EphemeralPlane::latest_from_events_with_validation(events, Kind::MlsKeyPackage, |event| {
            EphemeralPlane::is_key_package_event_semantically_valid(event)
        })
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
    use std::{path::PathBuf, time::SystemTime};

    use sqlx::sqlite::SqlitePoolOptions;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        relay_control::{
            RelayPlane,
            observability::{RelayObservabilityConfig, RelayTelemetryKind},
            sessions::{RelaySession, RelaySessionConfig},
        },
        whitenoise::database::{Database, relay_events::RelayEventRecord},
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
    fn test_default_uses_disabled_auth_and_reconnect() {
        let config = EphemeralPlaneConfig::default();

        assert_eq!(config.timeout, Duration::from_secs(5));
        assert_eq!(config.auth_policy, RelaySessionAuthPolicy::Disabled);
        assert_eq!(
            config.reconnect_policy,
            RelaySessionReconnectPolicy::Disabled
        );
        assert_eq!(config.max_publish_attempts, 3);
        assert_eq!(config.ad_hoc_relay_ttl, Duration::from_secs(300));
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
}
