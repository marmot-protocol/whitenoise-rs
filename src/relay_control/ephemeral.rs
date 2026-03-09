use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use mdk_core::prelude::GroupId;
use nostr_sdk::prelude::*;
use tokio::sync::{broadcast, mpsc::Sender};

use super::{
    RelayPlane,
    observability::{RelayObservability, RelayTelemetry},
    sessions::{
        RelaySession, RelaySessionAuthPolicy, RelaySessionConfig, RelaySessionReconnectPolicy,
    },
};
use crate::{
    RelayType,
    nostr_manager::utils::{is_event_timestamp_valid, is_relay_list_tag_for_event_kind},
    nostr_manager::{NostrManagerError, Result},
    types::ProcessableEvent,
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
}

impl Default for EphemeralPlaneConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            reconnect_policy: RelaySessionReconnectPolicy::Disabled,
            auth_policy: RelaySessionAuthPolicy::Disabled,
            max_publish_attempts: 3,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EphemeralPlane {
    config: EphemeralPlaneConfig,
    database: Arc<Database>,
    event_sender: Sender<ProcessableEvent>,
    observability: RelayObservability,
    operation_counter: Arc<AtomicU64>,
}

impl EphemeralPlane {
    pub(crate) fn new(
        config: EphemeralPlaneConfig,
        database: Arc<Database>,
        event_sender: Sender<ProcessableEvent>,
        observability: RelayObservability,
    ) -> Self {
        Self {
            config,
            database,
            event_sender,
            observability,
            operation_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) async fn fetch_metadata_from(
        &self,
        relays: &[RelayUrl],
        pubkey: PublicKey,
    ) -> Result<Option<Event>> {
        let filter = Filter::new().author(pubkey).kind(Kind::Metadata);
        let events = self.fetch_events_from(relays, filter).await?;

        Self::latest_from_events_with_validation(events, Kind::Metadata, |event| {
            Self::is_metadata_event_semantically_valid(event)
        })
    }

    pub(crate) async fn fetch_user_relays(
        &self,
        pubkey: PublicKey,
        relay_type: RelayType,
        relays: &[RelayUrl],
    ) -> Result<Option<Event>> {
        let filter = Filter::new().author(pubkey).kind(relay_type.into());
        let events = self.fetch_events_from(relays, filter).await?;

        Self::latest_from_events_with_validation(events, relay_type.into(), |event| {
            Self::is_relay_event_semantically_valid(event)
        })
    }

    pub(crate) async fn fetch_user_key_package(
        &self,
        pubkey: PublicKey,
        relays: &[RelayUrl],
    ) -> Result<Option<Event>> {
        let filter = Filter::new().kind(Kind::MlsKeyPackage).author(pubkey);
        let events = self.fetch_events_from(relays, filter).await?;

        Self::latest_from_events_with_validation(events, Kind::MlsKeyPackage, |event| {
            Self::is_key_package_event_semantically_valid(event)
        })
    }

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

    pub(crate) async fn publish_metadata_with_signer(
        &self,
        metadata: &Metadata,
        relays: &[RelayUrl],
        signer: Arc<dyn NostrSigner>,
    ) -> Result<Output<EventId>> {
        self.publish_event_builder_with_signer(EventBuilder::metadata(metadata), relays, signer)
            .await
    }

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

    pub(crate) async fn publish_event_to(
        &self,
        event: Event,
        account_pubkey: &PublicKey,
        relays: &[RelayUrl],
    ) -> Result<Output<EventId>> {
        // Retry schedule is immediate first attempt, then 2 s and 4 s backoff by default.
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

            let session = self.spawn_session("publish", Some(*account_pubkey));
            let result = session.publish_event_to(relays, &event).await;
            session.shutdown().await;

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

    pub(crate) async fn fetch_events_from(
        &self,
        relays: &[RelayUrl],
        filter: Filter,
    ) -> Result<Events> {
        let session = self.spawn_session("query", None);
        let result = session
            .fetch_events_from(relays, filter, self.config.timeout)
            .await;
        session.shutdown().await;
        result
    }

    async fn publish_event_builder_with_signer(
        &self,
        event_builder: EventBuilder,
        relays: &[RelayUrl],
        signer: Arc<dyn NostrSigner>,
    ) -> Result<Output<EventId>> {
        let account_pubkey = signer.get_public_key().await?;
        let event = event_builder.sign(&signer).await?;

        self.publish_event_to(event, &account_pubkey, relays).await
    }

    fn session_config(&self, account_pubkey: Option<PublicKey>) -> RelaySessionConfig {
        let mut config = RelaySessionConfig::new(RelayPlane::Ephemeral);
        config.telemetry_account_pubkey = account_pubkey;
        config.auth_policy = self.config.auth_policy;
        config.reconnect_policy = self.config.reconnect_policy;
        config.connect_timeout = self.config.timeout;
        config
    }

    fn spawn_session(&self, operation: &str, account_pubkey: Option<PublicKey>) -> RelaySession {
        let session = RelaySession::new(
            self.session_config(account_pubkey),
            self.event_sender.clone(),
        );

        self.spawn_telemetry_persistor(
            &format!(
                "ephemeral:{operation}:{}:{}",
                account_pubkey
                    .map(|pubkey| pubkey.to_hex())
                    .unwrap_or_else(|| "anonymous".to_string()),
                self.next_operation_id()
            ),
            session.telemetry(),
        );

        session
    }

    fn spawn_telemetry_persistor(
        &self,
        task_name: &str,
        mut receiver: broadcast::Receiver<RelayTelemetry>,
    ) {
        let database = self.database.clone();
        let observability = self.observability.clone();
        let task_name = task_name.to_string();

        tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(telemetry) => {
                        if let Err(error) = observability.record(&database, &telemetry).await {
                            tracing::error!(
                                target: "whitenoise::relay_control::observability",
                                task = task_name,
                                plane = telemetry.plane.as_str(),
                                relay_url = %telemetry.relay_url,
                                kind = telemetry.kind.as_str(),
                                "Failed to persist relay telemetry: {error}"
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            target: "whitenoise::relay_control::observability",
                            task = task_name,
                            skipped,
                            "Relay telemetry receiver lagged; dropping oldest samples"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    fn next_operation_id(&self) -> u64 {
        self.operation_counter.fetch_add(1, Ordering::Relaxed) + 1
    }

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
            sessions::RelaySessionConfig,
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
