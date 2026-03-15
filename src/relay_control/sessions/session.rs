use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use futures::future::join_all;
use futures::stream::{FuturesUnordered, StreamExt};
use nostr_sdk::prelude::*;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc::Sender};

use super::{RelaySessionConfig, RelaySessionRelayPolicy, notifications::RelayNotification};
use crate::{
    nostr_manager::{NostrManagerError, Result},
    perf_instrument,
    relay_control::{
        SubscriptionContext, SubscriptionStream,
        observability::{RelayTelemetry, RelayTelemetryKind},
        router::RelayRouter,
    },
    types::ProcessableEvent,
    types::{RelaySessionRelayStateSnapshot, RelaySessionStateSnapshot},
};

#[derive(Debug)]
struct RelaySessionState {
    notification_handler_registered: AtomicBool,
    publish_lock: Mutex<()>,
    subscription_relays: RwLock<HashMap<SubscriptionId, Vec<RelayUrl>>>,
}

#[derive(Debug)]
struct PreparedRelayUrls {
    usable_relay_urls: Vec<RelayUrl>,
    failed_relays: Vec<(RelayUrl, String)>,
}

#[derive(Debug)]
struct RelaySetupOutcome {
    usable_relay_urls: Vec<RelayUrl>,
    failed_relays: Vec<(RelayUrl, String)>,
    added_new_relay: bool,
    last_error: Option<NostrManagerError>,
}

/// Reusable single-client relay session used by relay planes.
#[derive(Debug, Clone)]
pub(crate) struct RelaySession {
    client: Client,
    config: RelaySessionConfig,
    telemetry_sender: broadcast::Sender<RelayTelemetry>,
    router: Arc<RelayRouter>,
    state: Arc<RelaySessionState>,
}

impl RelaySession {
    pub(crate) fn new(config: RelaySessionConfig, event_sender: Sender<ProcessableEvent>) -> Self {
        let opts = ClientOptions::default().verify_subscriptions(true);
        let client = Client::builder().opts(opts).build();
        let (telemetry_sender, _) = broadcast::channel(256);

        let session = Self {
            client,
            config,
            telemetry_sender,
            router: Arc::new(RelayRouter::default()),
            state: Arc::new(RelaySessionState {
                notification_handler_registered: AtomicBool::new(false),
                publish_lock: Mutex::new(()),
                subscription_relays: RwLock::new(HashMap::new()),
            }),
        };

        session.spawn_notification_handler(event_sender);
        session
    }

    #[allow(dead_code)]
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    #[allow(dead_code)]
    pub(crate) fn config(&self) -> &RelaySessionConfig {
        &self.config
    }

    pub(crate) fn telemetry(&self) -> broadcast::Receiver<RelayTelemetry> {
        self.telemetry_sender.subscribe()
    }

    pub(crate) fn notification_handler_registered(&self) -> bool {
        self.state
            .notification_handler_registered
            .load(Ordering::Relaxed)
    }

    pub(crate) async fn set_signer(&self, signer: impl NostrSigner + 'static) {
        self.client.set_signer(signer).await;
    }

    pub(crate) async fn unset_signer(&self) {
        self.client.unset_signer().await;
    }

    pub(crate) async fn has_any_relay_connected(&self, relay_urls: &[RelayUrl]) -> bool {
        for relay_url in relay_urls {
            if let Ok(relay) = self.client.relay(relay_url).await {
                match relay.status() {
                    RelayStatus::Connected | RelayStatus::Connecting => return true,
                    _ => {}
                }
            }
        }

        false
    }

    #[perf_instrument("relay")]
    pub(crate) async fn ensure_relays_connected(&self, relay_urls: &[RelayUrl]) -> Result<()> {
        self.prepare_relay_urls(relay_urls).await?;
        Ok(())
    }

    #[perf_instrument("relay")]
    async fn prepare_relay_urls(&self, relay_urls: &[RelayUrl]) -> Result<PreparedRelayUrls> {
        if relay_urls.is_empty() {
            return Ok(PreparedRelayUrls {
                usable_relay_urls: Vec::new(),
                failed_relays: Vec::new(),
            });
        }

        let relay_futures = relay_urls
            .iter()
            .map(|relay_url| self.ensure_relay_in_client(relay_url));
        let results = futures::future::join_all(relay_futures).await;

        let RelaySetupOutcome {
            usable_relay_urls,
            failed_relays,
            added_new_relay,
            last_error,
        } = Self::partition_relay_setup_results(relay_urls, results);

        if usable_relay_urls.is_empty() {
            return Err(last_error.unwrap_or(NostrManagerError::NoRelayConnections));
        }

        if !failed_relays.is_empty() {
            let failed_relay_urls = failed_relays
                .iter()
                .map(|(relay_url, _)| relay_url.to_string())
                .collect::<Vec<_>>();
            tracing::warn!(
                target: "whitenoise::relay_control::session",
                "Partial relay connection failure — {} of {} relays failed: [{}]",
                failed_relay_urls.len(),
                relay_urls.len(),
                failed_relay_urls.join(", "),
            );
        }

        let mut relays_to_wait = Vec::new();
        for relay_url in &usable_relay_urls {
            let relay = match self.client.relay(relay_url).await {
                Ok(relay) => relay,
                Err(error) => {
                    return Err(NostrManagerError::WhitenoiseInstance(format!(
                        "relay {relay_url} disappeared from session client: {error}"
                    )));
                }
            };

            match relay.status() {
                RelayStatus::Connected => {}
                RelayStatus::Connecting => {
                    relays_to_wait.push(relay);
                }
                RelayStatus::Pending => {
                    // Pending relays have can_connect() == false, so relay.connect()
                    // is a no-op. Terminate first to reset to a connectable state.
                    relay.disconnect();
                    relay.connect();
                    relays_to_wait.push(relay);
                }
                RelayStatus::Disconnected => {
                    relay.disconnect();
                    relay.connect();
                    relays_to_wait.push(relay);
                }
                RelayStatus::Banned => {
                    return Err(NostrManagerError::WhitenoiseInstance(format!(
                        "relay {relay_url} is banned and cannot be used"
                    )));
                }
                _ => {
                    relay.connect();
                    relays_to_wait.push(relay);
                }
            }
        }

        if added_new_relay || !relays_to_wait.is_empty() {
            let connect_timeout = self.config.connect_timeout;
            let already_connected = usable_relay_urls.len() - relays_to_wait.len();

            match self.config.min_connected_relays {
                Some(target) if already_connected < target => {
                    let needed = target.min(usable_relay_urls.len()) - already_connected;
                    let mut futs: FuturesUnordered<_> = relays_to_wait
                        .into_iter()
                        .map(|relay| async move {
                            relay.wait_for_connection(connect_timeout).await;
                            relay
                        })
                        .collect();

                    let mut newly_connected = 0;
                    while let Some(relay) = futs.next().await {
                        if matches!(relay.status(), RelayStatus::Connected) {
                            newly_connected += 1;
                        }
                        if newly_connected >= needed {
                            break;
                        }
                    }
                    // Remaining futures are dropped. The underlying connections
                    // continue in background and nostr-sdk auto-resubscribes
                    // when they come online.
                }
                Some(_) => {
                    // Already have enough connected relays, skip waiting.
                }
                None => {
                    join_all(relays_to_wait.into_iter().map(|relay| async move {
                        relay.wait_for_connection(connect_timeout).await;
                    }))
                    .await;
                }
            }
        }

        let mut connected_relays = 0usize;
        let mut disconnected_after_wait = Vec::new();

        for relay_url in &usable_relay_urls {
            let relay = match self.client.relay(relay_url).await {
                Ok(relay) => relay,
                Err(error) => {
                    return Err(NostrManagerError::WhitenoiseInstance(format!(
                        "relay {relay_url} disappeared from session client after connect wait: {error}"
                    )));
                }
            };

            match relay.status() {
                RelayStatus::Connected => connected_relays += 1,
                status => disconnected_after_wait.push(format!("{relay_url} ({status:?})")),
            }
        }

        if connected_relays == 0 {
            if !disconnected_after_wait.is_empty() {
                tracing::warn!(
                    target: "whitenoise::relay_control::session",
                    "No requested relays connected after waiting {}ms: [{}]",
                    self.config.connect_timeout.as_millis(),
                    disconnected_after_wait.join(", "),
                );
            }

            return Err(last_error.unwrap_or(NostrManagerError::NoRelayConnections));
        }

        Ok(PreparedRelayUrls {
            usable_relay_urls,
            failed_relays,
        })
    }

    #[allow(dead_code)]
    #[perf_instrument("relay")]
    pub(crate) async fn fetch_events_from(
        &self,
        relay_urls: &[RelayUrl],
        filter: Filter,
        timeout: std::time::Duration,
    ) -> Result<Events> {
        for relay_url in relay_urls {
            self.emit_telemetry(Self::apply_telemetry_scope(
                self.config.telemetry_account_pubkey,
                RelayTelemetry::new(
                    RelayTelemetryKind::QueryAttempt,
                    self.config.plane,
                    relay_url.clone(),
                ),
            ));
        }

        let prepared_relays = match self.prepare_relay_urls(relay_urls).await {
            Ok(prepared_relays) => prepared_relays,
            Err(error) => {
                for relay_url in relay_urls {
                    self.emit_telemetry(Self::apply_telemetry_scope(
                        self.config.telemetry_account_pubkey,
                        RelayTelemetry::new(
                            RelayTelemetryKind::QueryFailure,
                            self.config.plane,
                            relay_url.clone(),
                        )
                        .with_message(error.to_string()),
                    ));
                }
                return Err(error);
            }
        };

        for (relay_url, message) in &prepared_relays.failed_relays {
            self.emit_telemetry(Self::apply_telemetry_scope(
                self.config.telemetry_account_pubkey,
                RelayTelemetry::new(
                    RelayTelemetryKind::QueryFailure,
                    self.config.plane,
                    relay_url.clone(),
                )
                .with_message(message.clone()),
            ));
        }

        let usable_relay_urls = prepared_relays.usable_relay_urls;

        let result = self
            .client
            .fetch_events_from(&usable_relay_urls, filter, timeout)
            .await;
        match result {
            Ok(events) => {
                for relay_url in &usable_relay_urls {
                    self.emit_telemetry(Self::apply_telemetry_scope(
                        self.config.telemetry_account_pubkey,
                        RelayTelemetry::new(
                            RelayTelemetryKind::QuerySuccess,
                            self.config.plane,
                            relay_url.clone(),
                        ),
                    ));
                }
                Ok(events)
            }
            Err(error) => {
                for relay_url in &usable_relay_urls {
                    self.emit_telemetry(Self::apply_telemetry_scope(
                        self.config.telemetry_account_pubkey,
                        RelayTelemetry::new(
                            RelayTelemetryKind::QueryFailure,
                            self.config.plane,
                            relay_url.clone(),
                        )
                        .with_message(error.to_string()),
                    ));
                }
                Err(error.into())
            }
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_event_to(
        &self,
        relay_urls: &[RelayUrl],
        event: &Event,
    ) -> Result<Output<EventId>> {
        let _publish_guard = self.state.publish_lock.lock().await;

        for relay_url in relay_urls {
            self.emit_telemetry(Self::apply_telemetry_scope(
                self.config.telemetry_account_pubkey,
                RelayTelemetry::new(
                    RelayTelemetryKind::PublishAttempt,
                    self.config.plane,
                    relay_url.clone(),
                ),
            ));
        }

        let prepared_relays = match self.prepare_relay_urls(relay_urls).await {
            Ok(prepared_relays) => prepared_relays,
            Err(error) => {
                for relay_url in relay_urls {
                    self.emit_telemetry(Self::apply_telemetry_scope(
                        self.config.telemetry_account_pubkey,
                        RelayTelemetry::new(
                            RelayTelemetryKind::PublishFailure,
                            self.config.plane,
                            relay_url.clone(),
                        )
                        .with_message(error.to_string()),
                    ));
                }
                return Err(error);
            }
        };

        for (relay_url, message) in &prepared_relays.failed_relays {
            self.emit_telemetry(Self::apply_telemetry_scope(
                self.config.telemetry_account_pubkey,
                RelayTelemetry::new(
                    RelayTelemetryKind::PublishFailure,
                    self.config.plane,
                    relay_url.clone(),
                )
                .with_message(message.clone()),
            ));
        }

        let usable_relay_urls = prepared_relays.usable_relay_urls;

        let result = self.client.send_event_to(&usable_relay_urls, event).await;
        match result {
            Ok(output) => {
                for relay_url in &usable_relay_urls {
                    if output.success.contains(relay_url) {
                        self.emit_telemetry(Self::apply_telemetry_scope(
                            self.config.telemetry_account_pubkey,
                            RelayTelemetry::new(
                                RelayTelemetryKind::PublishSuccess,
                                self.config.plane,
                                relay_url.clone(),
                            ),
                        ));
                    } else if let Some(message) = output.failed.get(relay_url) {
                        self.emit_telemetry(Self::apply_telemetry_scope(
                            self.config.telemetry_account_pubkey,
                            RelayTelemetry::new(
                                RelayTelemetryKind::PublishFailure,
                                self.config.plane,
                                relay_url.clone(),
                            )
                            .with_message(message.clone()),
                        ));
                        tracing::warn!(
                            target: "whitenoise::relay_control::session",
                            plane = self.config.plane.as_str(),
                            relay_url = %relay_url,
                            event_id = %event.id,
                            "Relay rejected publish: {message}"
                        );
                    }
                }

                Ok(output)
            }
            Err(error) => {
                for relay_url in &usable_relay_urls {
                    self.emit_telemetry(Self::apply_telemetry_scope(
                        self.config.telemetry_account_pubkey,
                        RelayTelemetry::new(
                            RelayTelemetryKind::PublishFailure,
                            self.config.plane,
                            relay_url.clone(),
                        )
                        .with_message(error.to_string()),
                    ));
                }

                Err(error.into())
            }
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn remove_relay(&self, relay_url: &RelayUrl) -> Result<()> {
        self.client.force_remove_relay(relay_url.clone()).await?;
        Ok(())
    }

    #[perf_instrument("relay")]
    pub(crate) async fn subscribe_with_id_to(
        &self,
        relay_urls: &[RelayUrl],
        subscription_id: SubscriptionId,
        filter: Filter,
        stream: SubscriptionStream,
        account_pubkey: Option<PublicKey>,
        group_ids: &[String],
    ) -> Result<()> {
        for relay_url in relay_urls {
            let mut telemetry = RelayTelemetry::new(
                RelayTelemetryKind::SubscriptionAttempt,
                self.config.plane,
                relay_url.clone(),
            )
            .with_subscription_id(subscription_id.to_string());
            if let Some(account_pubkey) = account_pubkey {
                telemetry = telemetry.with_account_pubkey(account_pubkey);
            }
            self.emit_telemetry(Self::apply_telemetry_scope(
                self.config.telemetry_account_pubkey,
                telemetry,
            ));
        }

        // Snapshot existing routing state before overwriting it. When a
        // stable subscription ID is being replaced in-place (e.g.
        // DiscoveryPlane::sync reuses "discovery_user_data_0"), the old
        // relay-side subscription is still live until the new REQ is accepted.
        // If the new subscribe call fails we must restore the previous mapping
        // rather than unconditionally delete it, or the old live subscription
        // becomes unroutable and its events are silently dropped.
        let prev_sub_relays: Option<Vec<RelayUrl>> = self
            .state
            .subscription_relays
            .read()
            .await
            .get(&subscription_id)
            .cloned();

        let mut prev_contexts: Vec<(RelayUrl, SubscriptionContext)> = Vec::new();
        if let Some(ref old_urls) = prev_sub_relays {
            for relay_url in old_urls {
                if let Some(ctx) = self
                    .router
                    .subscription_context(relay_url, &subscription_id)
                    .await
                {
                    prev_contexts.push((relay_url.clone(), ctx));
                }
            }
        }

        // Pre-register new routing context BEFORE issuing the subscription to
        // the relay. A relay can deliver the first matching event as soon as it
        // acknowledges the REQ; the router must be populated by then or the
        // notification handler will hit a missing context and silently drop it.
        self.state
            .subscription_relays
            .write()
            .await
            .insert(subscription_id.clone(), relay_urls.to_vec());

        for relay_url in relay_urls {
            self.router
                .record_subscription_context(
                    relay_url.clone(),
                    subscription_id.clone(),
                    SubscriptionContext {
                        plane: self.config.plane,
                        account_pubkey,
                        relay_url: relay_url.clone(),
                        stream,
                        group_ids: group_ids.to_vec(),
                    },
                )
                .await;
        }

        let result = self
            .client
            .subscribe_with_id_to(relay_urls, subscription_id.clone(), filter, None)
            .await;

        match result {
            Ok(_) => {
                for relay_url in relay_urls {
                    let mut telemetry = RelayTelemetry::new(
                        RelayTelemetryKind::SubscriptionSuccess,
                        self.config.plane,
                        relay_url.clone(),
                    )
                    .with_subscription_id(subscription_id.to_string());
                    if let Some(account_pubkey) = account_pubkey {
                        telemetry = telemetry.with_account_pubkey(account_pubkey);
                    }
                    self.emit_telemetry(Self::apply_telemetry_scope(
                        self.config.telemetry_account_pubkey,
                        telemetry,
                    ));
                }
                Ok(())
            }
            Err(error) => {
                // Subscribe failed — restore previous routing state so any
                // still-active relay-side subscription remains routable.
                {
                    let mut map = self.state.subscription_relays.write().await;
                    if let Some(prev) = prev_sub_relays {
                        map.insert(subscription_id.clone(), prev);
                    } else {
                        map.remove(&subscription_id);
                    }
                }
                for relay_url in relay_urls {
                    self.router
                        .remove_subscription_context(relay_url, &subscription_id)
                        .await;
                }
                for (relay_url, old_ctx) in prev_contexts {
                    self.router
                        .record_subscription_context(relay_url, subscription_id.clone(), old_ctx)
                        .await;
                }
                for relay_url in relay_urls {
                    let mut telemetry = RelayTelemetry::new(
                        RelayTelemetryKind::SubscriptionFailure,
                        self.config.plane,
                        relay_url.clone(),
                    )
                    .with_subscription_id(subscription_id.to_string())
                    .with_message(error.to_string());
                    if let Some(account_pubkey) = account_pubkey {
                        telemetry = telemetry.with_account_pubkey(account_pubkey);
                    }
                    self.emit_telemetry(Self::apply_telemetry_scope(
                        self.config.telemetry_account_pubkey,
                        telemetry,
                    ));
                }
                Err(error.into())
            }
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn unsubscribe(&self, subscription_id: &SubscriptionId) {
        if let Some(relay_urls) = self
            .state
            .subscription_relays
            .write()
            .await
            .remove(subscription_id)
        {
            for relay_url in relay_urls {
                self.router
                    .remove_subscription_context(&relay_url, subscription_id)
                    .await;
            }
        }
        self.client.unsubscribe(subscription_id).await;
    }

    #[perf_instrument("relay")]
    pub(crate) async fn shutdown(&self) {
        self.state.subscription_relays.write().await.clear();
        self.router.clear().await;
        self.client.reset().await;
        self.client.shutdown().await;
    }

    #[allow(dead_code)]
    #[perf_instrument("relay")]
    pub(crate) async fn unsubscribe_all(&self) {
        let subscription_ids: Vec<SubscriptionId> = self
            .state
            .subscription_relays
            .read()
            .await
            .keys()
            .cloned()
            .collect();

        for subscription_id in subscription_ids {
            self.unsubscribe(&subscription_id).await;
        }
        self.client.unsubscribe_all().await;
    }

    pub(crate) async fn snapshot(&self, known_relays: &[RelayUrl]) -> RelaySessionStateSnapshot {
        let notification_handler_registered = self.notification_handler_registered();
        let router_context_count = self.router.context_count().await;
        let subscription_relays = self.state.subscription_relays.read().await.clone();

        let mut registered_subscription_ids = subscription_relays
            .keys()
            .map(|subscription_id| subscription_id.to_string())
            .collect::<Vec<_>>();
        registered_subscription_ids.sort_unstable();

        let mut all_relay_urls = BTreeSet::new();
        for relay_url in known_relays {
            all_relay_urls.insert(relay_url.to_string());
        }
        for relay_url in subscription_relays.values().flatten() {
            all_relay_urls.insert(relay_url.to_string());
        }

        let client_relays = self.client.relays().await;
        for relay_url in client_relays.keys() {
            all_relay_urls.insert(relay_url.to_string());
        }

        let mut relay_snapshots = Vec::with_capacity(all_relay_urls.len());
        for relay_url in all_relay_urls {
            let relay_url = RelayUrl::parse(&relay_url)
                .unwrap_or_else(|error| panic!("invalid relay url in session snapshot: {error}"));

            let mut relay_subscription_ids = subscription_relays
                .iter()
                .filter(|(_, relay_urls)| {
                    relay_urls.iter().any(|candidate| candidate == &relay_url)
                })
                .map(|(subscription_id, _)| subscription_id.to_string())
                .collect::<Vec<_>>();
            relay_subscription_ids.sort_unstable();

            let status = client_relays
                .get(&relay_url)
                .map(|relay| format!("{:?}", relay.status()))
                .unwrap_or_else(|| "NotRegistered".to_string());

            relay_snapshots.push(RelaySessionRelayStateSnapshot {
                relay_url: relay_url.to_string(),
                status,
                subscription_ids: relay_subscription_ids,
            });
        }

        RelaySessionStateSnapshot {
            notification_handler_registered,
            router_context_count,
            registered_subscription_count: registered_subscription_ids.len(),
            registered_subscription_ids,
            relays: relay_snapshots,
        }
    }

    /// Spawns the long-lived relay notification handler.
    ///
    /// The returned `JoinHandle` is intentionally not stored. This task is
    /// expected to run for the lifetime of the session; it exits only when the
    /// underlying `Client` shuts down (via `RelayPoolNotification::Shutdown`).
    /// Events queued in the Tokio channel at process-exit time may be dropped —
    /// this is an accepted trade-off for the current phase. A future `shutdown()`
    /// method could cancel the client and `await` this handle to drain in-flight
    /// events gracefully.
    fn spawn_notification_handler(&self, event_sender: Sender<ProcessableEvent>) {
        if self
            .state
            .notification_handler_registered
            .swap(true, Ordering::SeqCst)
        {
            return;
        }

        let client = self.client.clone();
        let telemetry_sender = self.telemetry_sender.clone();
        let router = self.router.clone();
        let plane = self.config.plane;
        let telemetry_account_pubkey = self.config.telemetry_account_pubkey;
        let state = self.state.clone();

        tokio::spawn(async move {
            // Restart the handler on transient errors so a single relay
            // disconnect does not permanently blind the session. The loop
            // exits on a graceful Shutdown notification (Ok return) or when
            // the event_sender channel closes (receiver dropped).
            loop {
                // Clone per-iteration so the `move` closure owns fresh handles.
                let sender_i = event_sender.clone();
                let telemetry_i = telemetry_sender.clone();
                let router_i = router.clone();
                let state_i = state.clone();

                let result = client
                    .handle_notifications(move |notification| {
                        let sender = sender_i.clone();
                        let telemetry_sender = telemetry_i.clone();
                        let router = router_i.clone();
                        let state = state_i.clone();
                        async move {
                            match notification {
                                RelayPoolNotification::Event {
                                    relay_url,
                                    subscription_id,
                                    event,
                                } => {
                                    if let Some(context) = router
                                        .subscription_context(&relay_url, &subscription_id)
                                        .await
                                    {
                                        let target_contexts = match context.stream {
                                            SubscriptionStream::GroupMessages => {
                                                // The shared group plane collapses multiple
                                                // legacy per-account subscriptions onto one
                                                // relay session. After receipt we must rebuild
                                                // the original account-scoped deliveries for
                                                // every local account subscribed to this group.
                                                //
                                                // This fanout does NOT make processing
                                                // group-scoped. Each forwarded event still
                                                // carries a concrete account_pubkey in its
                                                // SubscriptionContext, and the event processor
                                                // runs account-specific MLS handling from there.
                                                match Self::extract_group_id(event.as_ref()) {
                                                    Some(group_id) => {
                                                        let matches = router
                                                            .matching_group_contexts(
                                                                &relay_url,
                                                                &group_id,
                                                            )
                                                            .await;
                                                        if matches.is_empty() {
                                                            tracing::debug!(
                                                                target: "whitenoise::relay_control::sessions",
                                                                relay_url = %relay_url,
                                                                subscription_id = %subscription_id,
                                                                event_id = %event.id,
                                                                group_id,
                                                                "Dropping group message for unknown or stale #h routing context"
                                                            );
                                                            vec![]
                                                        } else {
                                                            matches
                                                        }
                                                    }
                                                    None => {
                                                        tracing::warn!(
                                                            target: "whitenoise::relay_control::sessions",
                                                            relay_url = %relay_url,
                                                            subscription_id = %subscription_id,
                                                            event_id = %event.id,
                                                            "Dropping group message missing #h tag"
                                                        );
                                                        vec![]
                                                    }
                                                }
                                            }
                                            _ => vec![context.clone()],
                                        };

                                        for target_context in
                                            Self::dedupe_contexts(target_contexts)
                                        {
                                            let _ = sender
                                                .send(ProcessableEvent::new_routed_nostr_event(
                                                    event.as_ref().clone(),
                                                    target_context,
                                                ))
                                                .await;
                                        }
                                    } else {
                                        let is_registered = state
                                            .subscription_relays
                                            .read()
                                            .await
                                            .contains_key(&subscription_id);
                                        if is_registered {
                                            tracing::error!(
                                                target: "whitenoise::relay_control::sessions",
                                                relay_url = %relay_url,
                                                subscription_id = %subscription_id,
                                                "Missing routing context for tracked subscription"
                                            );
                                        } else {
                                            tracing::debug!(
                                                target: "whitenoise::relay_control::sessions",
                                                relay_url = %relay_url,
                                                subscription_id = %subscription_id,
                                                "Ignoring event for untracked subscription"
                                            );
                                        }
                                    }
                                    Ok(false)
                                }
                                RelayPoolNotification::Message { relay_url, message } => {
                                    match RelayNotification::from_message(relay_url.clone(), message)
                                    {
                                        Some(notification) => {
                                            Self::process_notification(
                                                notification,
                                                plane,
                                                telemetry_account_pubkey,
                                                &sender,
                                                &telemetry_sender,
                                            )
                                            .await
                                        }
                                        None => Ok(false),
                                    }
                                }
                                RelayPoolNotification::Shutdown => Ok(true),
                            }
                        }
                    })
                    .await;

                match result {
                    Ok(()) => {
                        // Graceful shutdown via Shutdown notification.
                        break;
                    }
                    Err(error) => {
                        tracing::error!(
                            target: "whitenoise::relay_control::session",
                            "Notification handler error: {error}. Restarting in 1s."
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }

            // Clear the flag so spawn_notification_handler can be called
            // again if the session is reused after a clean shutdown.
            state
                .notification_handler_registered
                .store(false, Ordering::SeqCst);
        });
    }

    #[perf_instrument("relay")]
    async fn process_notification(
        notification: RelayNotification,
        plane: crate::relay_control::RelayPlane,
        telemetry_account_pubkey: Option<PublicKey>,
        sender: &Sender<ProcessableEvent>,
        telemetry_sender: &broadcast::Sender<RelayTelemetry>,
    ) -> std::result::Result<bool, Box<dyn std::error::Error>> {
        match notification {
            RelayNotification::Notice {
                relay_url,
                message,
                failure_category,
            } => {
                let _ = sender
                    .send(ProcessableEvent::RelayMessage(
                        relay_url.clone(),
                        "Notice".to_string(),
                    ))
                    .await;
                let mut telemetry = RelayTelemetry::notice(plane, relay_url, &message);
                if let Some(failure_category) = failure_category {
                    telemetry = telemetry.with_failure_category(failure_category);
                }
                let _ = telemetry_sender.send(Self::apply_telemetry_scope(
                    telemetry_account_pubkey,
                    telemetry,
                ));
            }
            RelayNotification::Closed {
                relay_url,
                message,
                failure_category,
            } => {
                let _ = sender
                    .send(ProcessableEvent::RelayMessage(
                        relay_url.clone(),
                        "Closed".to_string(),
                    ))
                    .await;
                let mut telemetry = RelayTelemetry::closed(plane, relay_url, &message);
                if let Some(failure_category) = failure_category {
                    telemetry = telemetry.with_failure_category(failure_category);
                }
                let _ = telemetry_sender.send(Self::apply_telemetry_scope(
                    telemetry_account_pubkey,
                    telemetry,
                ));
            }
            RelayNotification::Auth {
                relay_url,
                challenge,
                failure_category,
            } => {
                let _ = sender
                    .send(ProcessableEvent::RelayMessage(
                        relay_url.clone(),
                        "Auth".to_string(),
                    ))
                    .await;
                let mut telemetry = RelayTelemetry::auth_challenge(plane, relay_url, &challenge);
                if let Some(failure_category) = failure_category {
                    telemetry = telemetry.with_failure_category(failure_category);
                }
                let _ = telemetry_sender.send(Self::apply_telemetry_scope(
                    telemetry_account_pubkey,
                    telemetry,
                ));
            }
            RelayNotification::Connected { relay_url } => {
                let _ = telemetry_sender.send(Self::apply_telemetry_scope(
                    telemetry_account_pubkey,
                    RelayTelemetry::new(RelayTelemetryKind::Connected, plane, relay_url),
                ));
            }
            RelayNotification::Disconnected {
                relay_url,
                failure_category,
            } => {
                let mut telemetry =
                    RelayTelemetry::new(RelayTelemetryKind::Disconnected, plane, relay_url);
                if let Some(failure_category) = failure_category {
                    telemetry = telemetry.with_failure_category(failure_category);
                }
                let _ = telemetry_sender.send(Self::apply_telemetry_scope(
                    telemetry_account_pubkey,
                    telemetry,
                ));
            }
            RelayNotification::Shutdown => return Ok(true),
        }

        Ok(false)
    }

    fn partition_relay_setup_results(
        relay_urls: &[RelayUrl],
        results: Vec<Result<bool>>,
    ) -> RelaySetupOutcome {
        let mut usable_relay_urls = Vec::new();
        let mut failed_relays = Vec::new();
        let mut added_new_relay = false;
        let mut last_error = None;

        for (relay_url, result) in relay_urls.iter().zip(results) {
            match result {
                Ok(was_added) => {
                    usable_relay_urls.push(relay_url.clone());
                    added_new_relay |= was_added;
                }
                Err(error) => {
                    failed_relays.push((relay_url.clone(), error.to_string()));
                    last_error = Some(error);
                }
            }
        }

        RelaySetupOutcome {
            usable_relay_urls,
            failed_relays,
            added_new_relay,
            last_error,
        }
    }

    #[perf_instrument("relay")]
    async fn ensure_relay_in_client(&self, relay_url: &RelayUrl) -> Result<bool> {
        match self.client.relay(relay_url).await {
            Ok(_) => Ok(false),
            Err(_) => match self.config.relay_policy {
                RelaySessionRelayPolicy::Dynamic => {
                    self.client.add_relay(relay_url.clone()).await?;
                    Ok(true)
                }
                RelaySessionRelayPolicy::ExplicitOnly => {
                    Err(NostrManagerError::WhitenoiseInstance(format!(
                        "relay {} is not allowed by explicit-only session policy",
                        relay_url
                    )))
                }
            },
        }
    }

    fn emit_telemetry(&self, telemetry: RelayTelemetry) {
        let _ = self.telemetry_sender.send(telemetry);
    }

    fn extract_group_id(event: &Event) -> Option<String> {
        event.tags.iter().find_map(|tag| match tag.kind() {
            TagKind::SingleLetter(single_letter)
                if single_letter == SingleLetterTag::lowercase(Alphabet::H) =>
            {
                tag.content().map(|content| content.to_string())
            }
            _ => None,
        })
    }

    fn dedupe_contexts(contexts: Vec<SubscriptionContext>) -> Vec<SubscriptionContext> {
        let mut seen = std::collections::HashSet::new();
        let mut deduped = Vec::new();

        for context in contexts {
            // Group fanout already filters to one relay and one stream, so the
            // concrete local account scope is the only meaningful dedupe key.
            let key = context.account_pubkey.map(|pubkey| pubkey.to_hex());

            if seen.insert(key) {
                deduped.push(context);
            }
        }

        deduped
    }

    fn apply_telemetry_scope(
        telemetry_account_pubkey: Option<PublicKey>,
        telemetry: RelayTelemetry,
    ) -> RelayTelemetry {
        match telemetry_account_pubkey {
            Some(account_pubkey) if telemetry.account_pubkey.is_none() => {
                telemetry.with_account_pubkey(account_pubkey)
            }
            _ => telemetry,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::mpsc;

    use super::*;
    use crate::relay_control::{RelayPlane, SubscriptionStream};

    #[tokio::test]
    async fn test_sessions_do_not_share_clients() {
        let (sender, _) = mpsc::channel(8);
        let session_a = RelaySession::new(RelaySessionConfig::new(RelayPlane::Discovery), sender);

        let (sender, _) = mpsc::channel(8);
        let session_b = RelaySession::new(RelaySessionConfig::new(RelayPlane::Discovery), sender);

        let relay = RelayUrl::parse("wss://relay.example.com").unwrap();
        session_a.client().add_relay(relay.clone()).await.unwrap();

        assert!(session_a.client().relay(&relay).await.is_ok());
        assert!(session_b.client().relay(&relay).await.is_err());
    }

    #[tokio::test]
    async fn test_notification_handler_registers_once() {
        let (sender, _) = mpsc::channel(8);
        let session = RelaySession::new(RelaySessionConfig::new(RelayPlane::Discovery), sender);

        assert!(session.notification_handler_registered());
        let clone = session.clone();
        assert!(clone.notification_handler_registered());
        assert!(Arc::ptr_eq(&session.state, &clone.state));
    }

    #[test]
    fn test_partition_relay_setup_results_skips_failed_relay_urls() {
        let relay_a = RelayUrl::parse("wss://relay-a.example.com").unwrap();
        let relay_b = RelayUrl::parse("wss://relay-b.example.com").unwrap();

        let outcome = RelaySession::partition_relay_setup_results(
            &[relay_a.clone(), relay_b.clone()],
            vec![Ok(false), Err(NostrManagerError::NoRelayConnections)],
        );

        assert_eq!(outcome.usable_relay_urls, vec![relay_a]);
        assert_eq!(
            outcome.failed_relays,
            vec![(relay_b, NostrManagerError::NoRelayConnections.to_string())]
        );
        assert!(!outcome.added_new_relay);
        assert!(matches!(
            outcome.last_error,
            Some(NostrManagerError::NoRelayConnections)
        ));
    }

    #[tokio::test]
    async fn test_subscribe_emits_telemetry() {
        let (sender, _) = mpsc::channel(8);
        let session = RelaySession::new(RelaySessionConfig::new(RelayPlane::Group), sender);
        let mut telemetry = session.telemetry();
        let relay = RelayUrl::parse("wss://relay.example.com").unwrap();
        let subscription_id = SubscriptionId::new("group-sub");
        let filter = Filter::new().kind(Kind::MlsGroupMessage);

        let _ = session
            .subscribe_with_id_to(
                &[relay],
                subscription_id,
                filter,
                SubscriptionStream::GroupMessages,
                None,
                &[],
            )
            .await;

        let first = telemetry.recv().await.unwrap();
        assert_eq!(first.kind, RelayTelemetryKind::SubscriptionAttempt);
    }

    #[tokio::test]
    async fn test_session_scope_applies_account_pubkey_to_connection_telemetry() {
        let (sender, _) = mpsc::channel(8);
        let account_pubkey = Keys::generate().public_key();
        let mut config = RelaySessionConfig::new(RelayPlane::AccountInbox);
        config.telemetry_account_pubkey = Some(account_pubkey);
        let session = RelaySession::new(config, sender);

        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let telemetry = RelaySession::apply_telemetry_scope(
            session.config.telemetry_account_pubkey,
            RelayTelemetry::new(
                RelayTelemetryKind::Connected,
                RelayPlane::AccountInbox,
                relay_url,
            ),
        );

        assert_eq!(telemetry.account_pubkey, Some(account_pubkey));
    }

    /// Validates the disconnect-before-connect mechanism used in `prepare_relay_urls`
    /// to recover relays stuck in `Pending` state.
    ///
    /// In nostr-sdk 0.44, `relay.connect()` is a no-op when status is `Pending`
    /// because `can_connect()` returns false for that state. Without intervention,
    /// a Pending relay stays stuck permanently. The fix calls `disconnect()` first
    /// to transition to `Terminated` (where `can_connect()` is true), then `connect()`.
    #[tokio::test]
    async fn test_disconnect_recovers_relay_stuck_in_pending() {
        let (sender, _) = mpsc::channel(8);
        let session = RelaySession::new(RelaySessionConfig::new(RelayPlane::Ephemeral), sender);

        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        session.client().add_relay(relay_url.clone()).await.unwrap();

        let relay = session.client().relay(&relay_url).await.unwrap();
        assert_eq!(relay.status(), RelayStatus::Initialized);

        // Put relay into Pending state (simulates a connection attempt in progress)
        relay.connect();
        assert_eq!(relay.status(), RelayStatus::Pending);

        // Demonstrate the bug: connect() is a no-op when already Pending
        relay.connect();
        assert_eq!(
            relay.status(),
            RelayStatus::Pending,
            "connect() should be a no-op for Pending relays"
        );

        // The fix: disconnect() synchronously sets status to Terminated.
        // No .await between disconnect() and assertion — on current_thread
        // runtime, spawned connection tasks can't interfere.
        relay.disconnect();
        assert_eq!(
            relay.status(),
            RelayStatus::Terminated,
            "disconnect() should transition Pending to Terminated"
        );

        // Now connect() works again — spawns a fresh connection task
        relay.connect();
        assert_eq!(
            relay.status(),
            RelayStatus::Pending,
            "connect() from Terminated should start a fresh connection attempt"
        );
    }

    #[tokio::test]
    async fn test_publish_emits_attempt_and_failure_telemetry() {
        let (sender, _) = mpsc::channel(8);
        let mut config = RelaySessionConfig::new(RelayPlane::Ephemeral);
        config.connect_timeout = std::time::Duration::from_millis(10);
        let session = RelaySession::new(config, sender);
        let mut telemetry = session.telemetry();

        let relay_url = RelayUrl::parse("ws://127.0.0.1:1").unwrap();
        let keys = Keys::generate();
        let event = EventBuilder::text_note("ephemeral publish test")
            .sign_with_keys(&keys)
            .unwrap();

        let _ = session.publish_event_to(&[relay_url], &event).await;

        let mut saw_attempt = false;
        let mut saw_failure = false;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !(saw_attempt && saw_failure) {
                match telemetry.recv().await.unwrap().kind {
                    RelayTelemetryKind::PublishAttempt => saw_attempt = true,
                    RelayTelemetryKind::PublishFailure => saw_failure = true,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
    }
}
