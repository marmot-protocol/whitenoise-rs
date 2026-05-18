use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use super::aptabase::AptabaseProductAnalyticsClient;
use super::client::ProductAnalyticsClient;
use super::{
    PRODUCT_ANALYTICS_MAX_BATCH_SIZE, ProductAnalyticsBackend, ProductAnalyticsConfig,
    ProductAnalyticsEvent, ProductAnalyticsEventName, ProductAnalyticsFlushStatus,
    ProductAnalyticsSettings, ProductAnalyticsTrackStatus, utc_now_millis,
};
use crate::whitenoise::Result;
use crate::whitenoise::database::Database;
use crate::whitenoise::error::WhitenoiseError;

const WORKER_QUEUE_SIZE: usize = 100;
const FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone)]
pub(crate) struct ProductAnalytics {
    config: Option<ProductAnalyticsConfig>,
    command_sender: Option<mpsc::Sender<WorkerCommand>>,
    session_id: Arc<Mutex<String>>,
    consent_generation: Arc<AtomicU64>,
    consent_generation_sender: watch::Sender<u64>,
}

impl ProductAnalytics {
    pub(crate) fn new(config: Option<ProductAnalyticsConfig>) -> Self {
        let consent_generation = Arc::new(AtomicU64::new(0));
        let (consent_generation_sender, consent_generation_receiver) = watch::channel(0);
        let command_sender = match &config {
            Some(ProductAnalyticsConfig {
                backend: ProductAnalyticsBackend::Aptabase(aptabase_config),
                ..
            }) => {
                let client = Arc::new(AptabaseProductAnalyticsClient::new(aptabase_config));
                Some(spawn_worker(client, consent_generation_receiver))
            }
            Some(ProductAnalyticsConfig {
                backend: ProductAnalyticsBackend::Disabled,
                ..
            })
            | None => None,
        };

        Self {
            config,
            command_sender,
            session_id: Arc::new(Mutex::new(generate_session_id())),
            consent_generation,
            consent_generation_sender,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_client(
        config: ProductAnalyticsConfig,
        client: Arc<dyn ProductAnalyticsClient>,
    ) -> Self {
        let consent_generation = Arc::new(AtomicU64::new(0));
        let (consent_generation_sender, consent_generation_receiver) = watch::channel(0);
        Self {
            config: Some(config),
            command_sender: Some(spawn_worker(client, consent_generation_receiver)),
            session_id: Arc::new(Mutex::new(generate_session_id())),
            consent_generation,
            consent_generation_sender,
        }
    }

    pub(crate) async fn set_enabled(
        &self,
        database: &Database,
        enabled: bool,
        consent_version: String,
    ) -> Result<ProductAnalyticsSettings> {
        if consent_version.trim().is_empty() || consent_version.len() > 64 {
            return Err(WhitenoiseError::ProductAnalytics(
                "consent version must be non-empty and bounded".to_string(),
            ));
        }

        let current_settings = ProductAnalyticsSettings::find_or_create_default(database).await?;
        let settings = ProductAnalyticsSettings {
            enabled,
            created_at: current_settings.created_at,
            updated_at: utc_now_millis(),
            consent_version,
        };
        settings.save(database).await?;

        if enabled {
            self.rotate_session().await;
            // `track` reads the session id after this await, so the opt-in marker
            // is the first event in the newly rotated analytics session.
            if let Err(e) = self
                .track(
                    database,
                    ProductAnalyticsEvent::new(ProductAnalyticsEventName::AnalyticsEnabled),
                )
                .await
            {
                tracing::warn!(
                    target: "whitenoise::product_analytics",
                    error = %e,
                    "Failed to enqueue analytics_enabled event after consent"
                );
            }
        } else {
            let revoked_generation = self.revoke_delivery_generation();
            self.purge_pending_events(revoked_generation).await;
        }

        Ok(settings)
    }

    pub(crate) async fn track(
        &self,
        database: &Database,
        event: ProductAnalyticsEvent,
    ) -> Result<ProductAnalyticsTrackStatus> {
        let settings = ProductAnalyticsSettings::find_or_create_default(database).await?;
        if !settings.enabled {
            return Ok(ProductAnalyticsTrackStatus::IgnoredDisabled);
        }

        let Some(config) = &self.config else {
            return Ok(ProductAnalyticsTrackStatus::IgnoredUnconfigured);
        };
        let Some(command_sender) = &self.command_sender else {
            return Ok(ProductAnalyticsTrackStatus::IgnoredUnconfigured);
        };

        let prepared = PreparedProductAnalyticsEvent {
            timestamp: Utc::now(),
            session_id: self.session_id.lock().await.clone(),
            event_name: event.name.as_str().to_string(),
            system_props: config.system_props(),
            props: event.validated_props()?,
            consent_generation: self.consent_generation.load(Ordering::SeqCst),
        };

        match command_sender.try_send(WorkerCommand::Track(Box::new(prepared))) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!(
                    target: "whitenoise::product_analytics",
                    "Dropping product analytics event because the in-memory queue is full"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(WhitenoiseError::ProductAnalytics(
                    "analytics worker is unavailable".to_string(),
                ));
            }
        }

        Ok(ProductAnalyticsTrackStatus::Queued)
    }

    pub(crate) async fn flush(&self, database: &Database) -> Result<ProductAnalyticsFlushStatus> {
        let settings = ProductAnalyticsSettings::find_or_create_default(database).await?;
        if !settings.enabled {
            return Ok(ProductAnalyticsFlushStatus::Disabled);
        }

        let Some(command_sender) = &self.command_sender else {
            return Ok(ProductAnalyticsFlushStatus::Unconfigured);
        };

        let (reply_sender, reply_receiver) = oneshot::channel();
        match command_sender.try_send(WorkerCommand::Flush(reply_sender)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                return Ok(ProductAnalyticsFlushStatus::TimedOut);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(WhitenoiseError::ProductAnalytics(
                    "analytics worker is unavailable".to_string(),
                ));
            }
        }

        match tokio::time::timeout(FLUSH_TIMEOUT, reply_receiver).await {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(_)) => Err(WhitenoiseError::ProductAnalytics(
                "analytics worker dropped flush response".to_string(),
            )),
            Err(_) => Ok(ProductAnalyticsFlushStatus::TimedOut),
        }
    }

    async fn purge_pending_events(&self, revoked_generation: u64) {
        if let Some(command_sender) = &self.command_sender
            && command_sender
                .send(WorkerCommand::Purge { revoked_generation })
                .await
                .is_err()
        {
            tracing::debug!(
                target: "whitenoise::product_analytics",
                "Analytics worker was unavailable while purging pending events"
            );
        }
    }

    async fn rotate_session(&self) {
        *self.session_id.lock().await = generate_session_id();
    }

    fn revoke_delivery_generation(&self) -> u64 {
        let revoked_generation = self.consent_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.consent_generation_sender.send(revoked_generation);
        revoked_generation
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreparedProductAnalyticsEvent {
    pub timestamp: DateTime<Utc>,
    pub session_id: String,
    pub event_name: String,
    pub system_props: SystemProps,
    pub props: Map<String, Value>,
    #[serde(skip)]
    pub(crate) consent_generation: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SystemProps {
    pub locale: String,
    pub os_name: String,
    pub is_debug: bool,
    pub bundle_identifier: String,
    pub device_class: String,
    pub app_version: String,
    pub sdk_version: String,
}

enum WorkerCommand {
    Track(Box<PreparedProductAnalyticsEvent>),
    Flush(oneshot::Sender<ProductAnalyticsFlushStatus>),
    Purge { revoked_generation: u64 },
}

fn spawn_worker(
    client: Arc<dyn ProductAnalyticsClient>,
    consent_generation_receiver: watch::Receiver<u64>,
) -> mpsc::Sender<WorkerCommand> {
    let (command_sender, command_receiver) = mpsc::channel(WORKER_QUEUE_SIZE);
    tokio::spawn(run_worker(
        command_receiver,
        client,
        consent_generation_receiver,
    ));
    command_sender
}

async fn run_worker(
    mut command_receiver: mpsc::Receiver<WorkerCommand>,
    client: Arc<dyn ProductAnalyticsClient>,
    mut consent_generation_receiver: watch::Receiver<u64>,
) {
    let mut queue = VecDeque::new();
    while let Some(command) = command_receiver.recv().await {
        match command {
            WorkerCommand::Track(event) => {
                queue.push_back(*event);
                let pending_flushes = drain_ready_commands(&mut command_receiver, &mut queue);
                let status =
                    send_queued_batches(&mut queue, &client, &mut consent_generation_receiver)
                        .await;
                for reply_sender in pending_flushes {
                    let _ = reply_sender.send(status);
                }
            }
            WorkerCommand::Flush(reply_sender) => {
                let status =
                    send_queued_batches(&mut queue, &client, &mut consent_generation_receiver)
                        .await;
                let _ = reply_sender.send(status);
            }
            WorkerCommand::Purge { revoked_generation } => {
                drop_revoked_events(&mut queue, revoked_generation);
            }
        }
    }
}

fn drain_ready_commands(
    command_receiver: &mut mpsc::Receiver<WorkerCommand>,
    queue: &mut VecDeque<PreparedProductAnalyticsEvent>,
) -> Vec<oneshot::Sender<ProductAnalyticsFlushStatus>> {
    let mut pending_flushes = Vec::new();
    loop {
        match command_receiver.try_recv() {
            Ok(WorkerCommand::Track(event)) => queue.push_back(*event),
            Ok(WorkerCommand::Flush(reply_sender)) => pending_flushes.push(reply_sender),
            Ok(WorkerCommand::Purge { revoked_generation }) => {
                drop_revoked_events(queue, revoked_generation);
            }
            Err(mpsc::error::TryRecvError::Empty) => break,
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        }
    }
    pending_flushes
}

fn drop_revoked_events(queue: &mut VecDeque<PreparedProductAnalyticsEvent>, generation: u64) {
    queue.retain(|event| event.consent_generation >= generation);
}

async fn send_queued_batches(
    queue: &mut VecDeque<PreparedProductAnalyticsEvent>,
    client: &Arc<dyn ProductAnalyticsClient>,
    consent_generation_receiver: &mut watch::Receiver<u64>,
) -> ProductAnalyticsFlushStatus {
    let current_generation = *consent_generation_receiver.borrow();
    drop_revoked_events(queue, current_generation);
    if queue.is_empty() {
        return ProductAnalyticsFlushStatus::NothingToFlush;
    }

    let mut sent_any = false;
    while !queue.is_empty() {
        let current_generation = *consent_generation_receiver.borrow();
        drop_revoked_events(queue, current_generation);

        let mut batch = Vec::with_capacity(PRODUCT_ANALYTICS_MAX_BATCH_SIZE);
        while batch.len() < PRODUCT_ANALYTICS_MAX_BATCH_SIZE {
            let Some(event) = queue.pop_front() else {
                break;
            };
            if event.consent_generation >= current_generation {
                batch.push(event);
            }
        }
        if batch.is_empty() {
            continue;
        }

        let send_result = tokio::select! {
            result = client.send_events(&batch) => result,
            changed = consent_generation_receiver.changed() => {
                match changed {
                    Ok(()) => {
                        let current_generation = *consent_generation_receiver.borrow();
                        drop_revoked_events(queue, current_generation);
                        return if sent_any {
                            ProductAnalyticsFlushStatus::Flushed
                        } else {
                            ProductAnalyticsFlushStatus::NothingToFlush
                        };
                    }
                    Err(_) => client.send_events(&batch).await,
                }
            }
        };
        sent_any = true;

        if let Err(e) = send_result {
            tracing::warn!(
                target: "whitenoise::product_analytics",
                error = %e,
                event_count = batch.len(),
                "Dropping failed analytics batch"
            );
        }
    }

    if sent_any {
        ProductAnalyticsFlushStatus::Flushed
    } else {
        ProductAnalyticsFlushStatus::NothingToFlush
    }
}

fn generate_session_id() -> String {
    let mut bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut bytes);
    let suffix = u32::from_le_bytes(bytes) % 100_000_000;
    format!("{}{:08}", Utc::now().timestamp(), suffix)
}

#[cfg(test)]
mod tests {
    use std::future;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use tokio::sync::Mutex as TokioMutex;
    use tokio::sync::Notify;

    use super::*;
    use crate::whitenoise::database::Database;
    use crate::whitenoise::product_analytics::{
        AptabaseAnalyticsConfig, PRODUCT_ANALYTICS_CONSENT_VERSION, ProductAnalyticsBackend,
        ProductAnalyticsDeviceClass,
    };

    #[derive(Default)]
    struct RecordingClient {
        batches: TokioMutex<Vec<Vec<PreparedProductAnalyticsEvent>>>,
        failures_remaining: AtomicUsize,
    }

    struct BlockingClient {
        started: Notify,
    }

    struct CancellationAwareBlockingClient {
        started: Notify,
        send_dropped: Arc<Notify>,
    }

    struct NotifyOnDrop(Arc<Notify>);

    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            self.0.notify_waiters();
        }
    }

    #[async_trait]
    impl ProductAnalyticsClient for RecordingClient {
        async fn send_events(&self, events: &[PreparedProductAnalyticsEvent]) -> Result<()> {
            if self.failures_remaining.load(Ordering::SeqCst) > 0 {
                self.failures_remaining.fetch_sub(1, Ordering::SeqCst);
                return Err(WhitenoiseError::ProductAnalytics("boom".to_string()));
            }
            self.batches.lock().await.push(events.to_vec());
            Ok(())
        }
    }

    #[async_trait]
    impl ProductAnalyticsClient for BlockingClient {
        async fn send_events(&self, _events: &[PreparedProductAnalyticsEvent]) -> Result<()> {
            self.started.notify_one();
            future::pending::<Result<()>>().await
        }
    }

    #[async_trait]
    impl ProductAnalyticsClient for CancellationAwareBlockingClient {
        async fn send_events(&self, _events: &[PreparedProductAnalyticsEvent]) -> Result<()> {
            self.started.notify_one();
            let _guard = NotifyOnDrop(self.send_dropped.clone());
            future::pending::<Result<()>>().await
        }
    }

    fn test_config() -> ProductAnalyticsConfig {
        ProductAnalyticsConfig {
            backend: ProductAnalyticsBackend::Aptabase(AptabaseAnalyticsConfig {
                app_key: "A-TEST".to_string(),
                host: "http://127.0.0.1:12345".to_string(),
            }),
            app_version: "1.0.0".to_string(),
            bundle_identifier: "dev.ipf.whitenoise.staging".to_string(),
            device_class: ProductAnalyticsDeviceClass::Phone,
            os_name: "iOS".to_string(),
            locale: "en-US".to_string(),
            is_debug: true,
        }
    }

    fn prepared_event(event_name: &str) -> PreparedProductAnalyticsEvent {
        let mut props = Map::new();
        props.insert("schema_version".to_string(), Value::from(1));
        PreparedProductAnalyticsEvent {
            timestamp: Utc::now(),
            session_id: "171351624706652714".to_string(),
            event_name: event_name.to_string(),
            system_props: test_config().system_props(),
            props,
            consent_generation: 0,
        }
    }

    fn analytics_with_sender(command_sender: mpsc::Sender<WorkerCommand>) -> ProductAnalytics {
        let consent_generation = Arc::new(AtomicU64::new(0));
        let (consent_generation_sender, _consent_generation_receiver) = watch::channel(0);
        ProductAnalytics {
            config: Some(test_config()),
            command_sender: Some(command_sender),
            session_id: Arc::new(Mutex::new("171351624706652714".to_string())),
            consent_generation,
            consent_generation_sender,
        }
    }

    async fn test_db() -> (Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(dir.path().join("analytics.sqlite"))
            .await
            .unwrap();
        (db, dir)
    }

    async fn enable_settings(database: &Database) {
        let mut settings = ProductAnalyticsSettings::find_or_create_default(database)
            .await
            .unwrap();
        settings.enabled = true;
        settings.save(database).await.unwrap();
    }

    #[tokio::test]
    async fn new_with_aptabase_backend_starts_worker() {
        let analytics = ProductAnalytics::new(Some(test_config()));

        assert!(analytics.command_sender.is_some());
    }

    #[tokio::test]
    async fn disabled_tracking_ignores_event() {
        let (db, _dir) = test_db().await;
        let client = Arc::new(RecordingClient::default());
        let analytics = ProductAnalytics::with_client(test_config(), client.clone());

        let status = analytics
            .track(
                &db,
                ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted),
            )
            .await
            .unwrap();

        assert_eq!(status, ProductAnalyticsTrackStatus::IgnoredDisabled);
        assert!(client.batches.lock().await.is_empty());
    }

    #[tokio::test]
    async fn enabled_tracking_sends_approved_event_with_system_props() {
        let (db, _dir) = test_db().await;
        let client = Arc::new(RecordingClient::default());
        let analytics = ProductAnalytics::with_client(test_config(), client.clone());
        analytics
            .set_enabled(&db, true, PRODUCT_ANALYTICS_CONSENT_VERSION.to_string())
            .await
            .unwrap();

        let status = analytics
            .track(
                &db,
                ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                    .with_string_prop("platform", "ios"),
            )
            .await
            .unwrap();
        assert_eq!(status, ProductAnalyticsTrackStatus::Queued);

        let flush_status = analytics.flush(&db).await.unwrap();
        assert!(matches!(
            flush_status,
            ProductAnalyticsFlushStatus::Flushed | ProductAnalyticsFlushStatus::NothingToFlush
        ));

        let batches = client.batches.lock().await;
        let sent_app_started = batches
            .iter()
            .flatten()
            .find(|event| event.event_name == "app_started")
            .expect("app_started event sent");
        assert_eq!(
            sent_app_started.system_props.bundle_identifier,
            "dev.ipf.whitenoise.staging"
        );
        assert_eq!(sent_app_started.system_props.device_class, "phone");
    }

    #[tokio::test]
    async fn worker_batches_at_most_twenty_five_events() {
        let (db, _dir) = test_db().await;
        let client = Arc::new(RecordingClient::default());
        let analytics = ProductAnalytics::with_client(test_config(), client.clone());
        analytics
            .set_enabled(&db, true, PRODUCT_ANALYTICS_CONSENT_VERSION.to_string())
            .await
            .unwrap();

        for _ in 0..30 {
            analytics
                .track(
                    &db,
                    ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                        .with_string_prop("platform", "ios"),
                )
                .await
                .unwrap();
        }
        let _ = analytics.flush(&db).await.unwrap();

        let batches = client.batches.lock().await;
        assert!(
            batches
                .iter()
                .all(|batch| batch.len() <= PRODUCT_ANALYTICS_MAX_BATCH_SIZE)
        );
    }

    #[tokio::test]
    async fn tracking_drops_instead_of_blocking_when_queue_is_full() {
        let (db, _dir) = test_db().await;
        let client = Arc::new(BlockingClient {
            started: Notify::new(),
        });
        let analytics = ProductAnalytics::with_client(test_config(), client.clone());
        analytics
            .set_enabled(&db, true, PRODUCT_ANALYTICS_CONSENT_VERSION.to_string())
            .await
            .unwrap();
        client.started.notified().await;

        for _ in 0..WORKER_QUEUE_SIZE {
            analytics
                .track(
                    &db,
                    ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                        .with_string_prop("platform", "ios"),
                )
                .await
                .unwrap();
        }

        let status = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            analytics.track(
                &db,
                ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                    .with_string_prop("platform", "ios"),
            ),
        )
        .await
        .expect("track should not wait for queue capacity")
        .unwrap();

        assert_eq!(status, ProductAnalyticsTrackStatus::Queued);
    }

    #[tokio::test]
    async fn tracking_reports_unconfigured_when_backend_disabled() {
        let (db, _dir) = test_db().await;
        let mut config = test_config();
        config.backend = ProductAnalyticsBackend::Disabled;
        let analytics = ProductAnalytics::new(Some(config));
        enable_settings(&db).await;

        let status = analytics
            .track(
                &db,
                ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted),
            )
            .await
            .unwrap();

        assert_eq!(status, ProductAnalyticsTrackStatus::IgnoredUnconfigured);
    }

    #[tokio::test]
    async fn tracking_drops_event_when_custom_queue_is_full() {
        let (db, _dir) = test_db().await;
        let (sender, _receiver) = mpsc::channel(1);
        sender
            .try_send(WorkerCommand::Track(Box::new(prepared_event("queued"))))
            .unwrap();
        let analytics = analytics_with_sender(sender);
        enable_settings(&db).await;

        let status = analytics
            .track(
                &db,
                ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted),
            )
            .await
            .unwrap();

        assert_eq!(status, ProductAnalyticsTrackStatus::Queued);
    }

    #[tokio::test]
    async fn tracking_reports_error_when_worker_channel_is_closed() {
        let (db, _dir) = test_db().await;
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let analytics = analytics_with_sender(sender);
        enable_settings(&db).await;

        let err = analytics
            .track(
                &db,
                ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, WhitenoiseError::ProductAnalytics(_)));
    }

    #[tokio::test]
    async fn set_enabled_succeeds_when_marker_event_cannot_be_queued() {
        let (db, _dir) = test_db().await;
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let analytics = analytics_with_sender(sender);

        let settings = analytics
            .set_enabled(&db, true, PRODUCT_ANALYTICS_CONSENT_VERSION.to_string())
            .await
            .unwrap();

        assert!(settings.enabled);
    }

    #[tokio::test]
    async fn disabling_consent_persists_setting_and_purges_worker() {
        let (db, _dir) = test_db().await;
        let client = Arc::new(RecordingClient::default());
        let analytics = ProductAnalytics::with_client(test_config(), client);
        enable_settings(&db).await;

        let settings = analytics
            .set_enabled(&db, false, PRODUCT_ANALYTICS_CONSENT_VERSION.to_string())
            .await
            .unwrap();

        assert!(!settings.enabled);
        assert_eq!(settings.consent_version, PRODUCT_ANALYTICS_CONSENT_VERSION);
    }

    #[tokio::test]
    async fn disabling_consent_cancels_active_batch_delivery() {
        let (db, _dir) = test_db().await;
        let send_dropped = Arc::new(Notify::new());
        let client = Arc::new(CancellationAwareBlockingClient {
            started: Notify::new(),
            send_dropped: send_dropped.clone(),
        });
        let analytics = ProductAnalytics::with_client(test_config(), client.clone());
        analytics
            .set_enabled(&db, true, PRODUCT_ANALYTICS_CONSENT_VERSION.to_string())
            .await
            .unwrap();
        client.started.notified().await;

        analytics
            .set_enabled(&db, false, PRODUCT_ANALYTICS_CONSENT_VERSION.to_string())
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), send_dropped.notified())
            .await
            .expect("opt-out should cancel active analytics delivery");
    }

    #[tokio::test]
    async fn disabling_consent_waits_until_purge_is_delivered_when_queue_is_full() {
        let (db, _dir) = test_db().await;
        let (sender, mut receiver) = mpsc::channel(1);
        sender
            .try_send(WorkerCommand::Track(Box::new(prepared_event("queued"))))
            .unwrap();
        let analytics = analytics_with_sender(sender);
        enable_settings(&db).await;

        let disable =
            analytics.set_enabled(&db, false, PRODUCT_ANALYTICS_CONSENT_VERSION.to_string());
        tokio::pin!(disable);

        tokio::select! {
            result = &mut disable => panic!("purge should wait for queue capacity: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
        }

        assert!(matches!(
            receiver.recv().await.unwrap(),
            WorkerCommand::Track(_)
        ));

        let settings = tokio::time::timeout(std::time::Duration::from_secs(1), disable)
            .await
            .unwrap()
            .unwrap();

        assert!(!settings.enabled);
        assert!(matches!(
            receiver.recv().await.unwrap(),
            WorkerCommand::Purge {
                revoked_generation: 1
            }
        ));
    }

    #[tokio::test]
    async fn flush_reports_unconfigured_when_enabled_without_worker() {
        let (db, _dir) = test_db().await;
        let mut config = test_config();
        config.backend = ProductAnalyticsBackend::Disabled;
        let analytics = ProductAnalytics::new(Some(config));
        enable_settings(&db).await;

        let status = analytics.flush(&db).await.unwrap();

        assert_eq!(status, ProductAnalyticsFlushStatus::Unconfigured);
    }

    #[tokio::test]
    async fn flush_reports_nothing_to_flush_for_empty_worker_queue() {
        let (db, _dir) = test_db().await;
        let client = Arc::new(RecordingClient::default());
        let analytics = ProductAnalytics::with_client(test_config(), client.clone());
        enable_settings(&db).await;

        let status = analytics.flush(&db).await.unwrap();

        assert_eq!(status, ProductAnalyticsFlushStatus::NothingToFlush);
        assert!(client.batches.lock().await.is_empty());
    }

    #[tokio::test]
    async fn flush_reports_timed_out_when_command_queue_is_full() {
        let (db, _dir) = test_db().await;
        let client = Arc::new(BlockingClient {
            started: Notify::new(),
        });
        let analytics = ProductAnalytics::with_client(test_config(), client.clone());
        analytics
            .set_enabled(&db, true, PRODUCT_ANALYTICS_CONSENT_VERSION.to_string())
            .await
            .unwrap();
        client.started.notified().await;

        for _ in 0..WORKER_QUEUE_SIZE {
            analytics
                .track(
                    &db,
                    ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                        .with_string_prop("platform", "ios"),
                )
                .await
                .unwrap();
        }

        let status = analytics.flush(&db).await.unwrap();

        assert_eq!(status, ProductAnalyticsFlushStatus::TimedOut);
    }

    #[tokio::test]
    async fn flush_reports_error_when_worker_channel_is_closed() {
        let (db, _dir) = test_db().await;
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let analytics = analytics_with_sender(sender);
        enable_settings(&db).await;

        let err = analytics.flush(&db).await.unwrap_err();

        assert!(matches!(err, WhitenoiseError::ProductAnalytics(_)));
    }

    #[tokio::test]
    async fn flush_reports_error_when_worker_drops_reply() {
        let (db, _dir) = test_db().await;
        let (sender, mut receiver) = mpsc::channel(1);
        let analytics = analytics_with_sender(sender);
        enable_settings(&db).await;
        let handle = tokio::spawn(async move {
            if let Some(WorkerCommand::Flush(reply_sender)) = receiver.recv().await {
                drop(reply_sender);
            }
        });

        let err = analytics.flush(&db).await.unwrap_err();
        handle.await.unwrap();

        assert!(matches!(err, WhitenoiseError::ProductAnalytics(_)));
    }

    #[tokio::test]
    async fn run_worker_accepts_purge_as_primary_command() {
        let (sender, receiver) = mpsc::channel(1);
        let (_generation_sender, generation_receiver) = watch::channel(0);
        let client = Arc::new(RecordingClient::default());
        let handle = tokio::spawn(run_worker(receiver, client, generation_receiver));

        sender
            .send(WorkerCommand::Purge {
                revoked_generation: 1,
            })
            .await
            .unwrap();
        drop(sender);
        handle.await.unwrap();
    }

    #[test]
    fn drain_ready_commands_handles_disconnected_receiver() {
        let (sender, mut receiver) = mpsc::channel(1);
        drop(sender);
        let mut queue = VecDeque::new();

        let pending_flushes = drain_ready_commands(&mut receiver, &mut queue);

        assert!(pending_flushes.is_empty());
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn set_enabled_rejects_invalid_consent_version() {
        let (db, _dir) = test_db().await;
        let client = Arc::new(RecordingClient::default());
        let analytics = ProductAnalytics::with_client(test_config(), client);

        assert!(
            analytics
                .set_enabled(&db, true, " ".to_string())
                .await
                .is_err()
        );
        assert!(
            analytics
                .set_enabled(&db, true, "x".repeat(65))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn send_queued_batches_drops_failed_batches() {
        let recording_client = Arc::new(RecordingClient::default());
        recording_client
            .failures_remaining
            .store(1, Ordering::SeqCst);
        let client: Arc<dyn ProductAnalyticsClient> = recording_client.clone();
        let mut queue = VecDeque::from([prepared_event("app_started")]);
        let (_generation_sender, mut generation_receiver) = watch::channel(0);

        let status = send_queued_batches(&mut queue, &client, &mut generation_receiver).await;

        assert_eq!(status, ProductAnalyticsFlushStatus::Flushed);
        assert!(queue.is_empty());
        assert!(recording_client.batches.lock().await.is_empty());
    }

    #[tokio::test]
    async fn drain_ready_commands_collects_flushes_and_honors_purge() {
        let (sender, mut receiver) = mpsc::channel(4);
        let (reply_sender, reply_receiver) = oneshot::channel();
        sender
            .try_send(WorkerCommand::Track(Box::new(prepared_event(
                "app_started",
            ))))
            .unwrap();
        sender.try_send(WorkerCommand::Flush(reply_sender)).unwrap();
        sender
            .try_send(WorkerCommand::Purge {
                revoked_generation: 1,
            })
            .unwrap();
        drop(sender);
        let mut queue = VecDeque::from([prepared_event("login_started")]);

        let pending_flushes = drain_ready_commands(&mut receiver, &mut queue);

        assert_eq!(pending_flushes.len(), 1);
        assert!(queue.is_empty());
        drop(pending_flushes);
        assert!(reply_receiver.await.is_err());
    }

    #[tokio::test]
    async fn run_worker_replies_to_flush_drained_after_track() {
        let (sender, receiver) = mpsc::channel(4);
        let (_generation_sender, generation_receiver) = watch::channel(0);
        let recording_client = Arc::new(RecordingClient::default());
        let client: Arc<dyn ProductAnalyticsClient> = recording_client.clone();
        let (reply_sender, reply_receiver) = oneshot::channel();
        sender
            .try_send(WorkerCommand::Track(Box::new(prepared_event(
                "app_started",
            ))))
            .unwrap();
        sender.try_send(WorkerCommand::Flush(reply_sender)).unwrap();
        drop(sender);

        run_worker(receiver, client, generation_receiver).await;

        assert_eq!(
            reply_receiver.await.unwrap(),
            ProductAnalyticsFlushStatus::Flushed
        );
        let batches = recording_client.batches.lock().await;
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0][0].event_name, "app_started");
    }
}
