use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use nostr_sdk::prelude::*;
use tokio::sync::{RwLock, broadcast, mpsc::Sender};

use super::{RelaySessionConfig, RelaySessionRelayPolicy, notifications::RelayNotification};
use crate::{
    nostr_manager::{NostrManagerError, Result},
    relay_control::{
        SubscriptionContext, SubscriptionStream,
        observability::{RelayTelemetry, RelayTelemetryKind},
        router::RelayRouter,
    },
    types::ProcessableEvent,
};

#[derive(Debug)]
struct RelaySessionState {
    notification_handler_registered: AtomicBool,
    subscription_relays: RwLock<HashMap<SubscriptionId, Vec<RelayUrl>>>,
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

    #[allow(dead_code)]
    pub(crate) fn telemetry(&self) -> broadcast::Receiver<RelayTelemetry> {
        self.telemetry_sender.subscribe()
    }

    #[allow(dead_code)]
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

    pub(crate) async fn ensure_relays_connected(&self, relay_urls: &[RelayUrl]) -> Result<()> {
        if relay_urls.is_empty() {
            return Ok(());
        }

        let relay_futures = relay_urls
            .iter()
            .map(|relay_url| self.ensure_relay_in_client(relay_url));
        let results = futures::future::join_all(relay_futures).await;

        let mut successful_relays = 0usize;
        let mut last_error: Option<NostrManagerError> = None;

        for result in results {
            match result {
                Ok(_) => successful_relays += 1,
                Err(err) => last_error = Some(err),
            }
        }

        if successful_relays == 0 {
            return Err(last_error.unwrap_or(NostrManagerError::NoRelayConnections));
        }

        self.client.connect().await;

        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) async fn fetch_events_from(
        &self,
        relay_urls: &[RelayUrl],
        filter: Filter,
        timeout: std::time::Duration,
    ) -> Result<Events> {
        for relay_url in relay_urls {
            self.emit_telemetry(RelayTelemetry::new(
                RelayTelemetryKind::QueryAttempt,
                self.config.plane,
                relay_url.clone(),
            ));
        }

        let result = self
            .client
            .fetch_events_from(relay_urls, filter, timeout)
            .await;
        match result {
            Ok(events) => {
                for relay_url in relay_urls {
                    self.emit_telemetry(RelayTelemetry::new(
                        RelayTelemetryKind::QuerySuccess,
                        self.config.plane,
                        relay_url.clone(),
                    ));
                }
                Ok(events)
            }
            Err(error) => {
                for relay_url in relay_urls {
                    self.emit_telemetry(
                        RelayTelemetry::new(
                            RelayTelemetryKind::QueryFailure,
                            self.config.plane,
                            relay_url.clone(),
                        )
                        .with_message(error.to_string()),
                    );
                }
                Err(error.into())
            }
        }
    }

    pub(crate) async fn subscribe_with_id_to(
        &self,
        relay_urls: &[RelayUrl],
        subscription_id: SubscriptionId,
        filter: Filter,
        stream: SubscriptionStream,
        account_pubkey: Option<PublicKey>,
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
            self.emit_telemetry(telemetry);
        }

        let result = self
            .client
            .subscribe_with_id_to(relay_urls, subscription_id.clone(), filter, None)
            .await;

        match result {
            Ok(_) => {
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
                            },
                        )
                        .await;
                }

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
                    self.emit_telemetry(telemetry);
                }
                Ok(())
            }
            Err(error) => {
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
                    self.emit_telemetry(telemetry);
                }
                Err(error.into())
            }
        }
    }

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

    #[allow(dead_code)]
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

        tokio::spawn(async move {
            if let Err(error) = client
                .handle_notifications(move |notification| {
                    let sender = event_sender.clone();
                    let telemetry_sender = telemetry_sender.clone();
                    let router = router.clone();
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
                                    let _ = sender
                                        .send(ProcessableEvent::new_routed_nostr_event(
                                            event.as_ref().clone(),
                                            context,
                                        ))
                                        .await;
                                } else {
                                    tracing::error!(
                                        target: "whitenoise::relay_control::session",
                                        "Missing subscription context for relay {} subscription {}",
                                        relay_url,
                                        subscription_id
                                    );
                                }
                                let _ = telemetry_sender.send(
                                    RelayTelemetry::new(
                                        RelayTelemetryKind::SubscriptionSuccess,
                                        plane,
                                        relay_url,
                                    )
                                    .with_subscription_id(subscription_id.to_string()),
                                );
                                Ok(false)
                            }
                            RelayPoolNotification::Message { relay_url, message } => {
                                let notification =
                                    RelayNotification::from_message(relay_url.clone(), message);
                                Self::process_notification(
                                    notification,
                                    plane,
                                    &sender,
                                    &telemetry_sender,
                                    &router,
                                )
                                .await
                            }
                            RelayPoolNotification::Shutdown => Ok(true),
                        }
                    }
                })
                .await
            {
                tracing::error!(
                    target: "whitenoise::relay_control::session",
                    "Notification handler error: {error}"
                );
            }
        });
    }

    async fn process_notification(
        notification: RelayNotification,
        plane: crate::relay_control::RelayPlane,
        sender: &Sender<ProcessableEvent>,
        telemetry_sender: &broadcast::Sender<RelayTelemetry>,
        router: &RelayRouter,
    ) -> std::result::Result<bool, Box<dyn std::error::Error>> {
        match notification {
            RelayNotification::Event {
                relay_url,
                subscription_id,
                event,
            } => {
                if let Some(context) = router
                    .subscription_context(&relay_url, &subscription_id)
                    .await
                {
                    let _ = sender
                        .send(ProcessableEvent::new_routed_nostr_event(event, context))
                        .await;
                } else {
                    tracing::error!(
                        target: "whitenoise::relay_control::session",
                        "Missing subscription context for relay {} subscription {}",
                        relay_url,
                        subscription_id
                    );
                }
            }
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
                let _ = telemetry_sender.send(telemetry);
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
                let _ = telemetry_sender.send(telemetry);
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
                let _ = telemetry_sender.send(telemetry);
            }
            RelayNotification::Connected { relay_url } => {
                let _ = telemetry_sender.send(RelayTelemetry::new(
                    RelayTelemetryKind::Connected,
                    plane,
                    relay_url,
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
                let _ = telemetry_sender.send(telemetry);
            }
            RelayNotification::Shutdown => return Ok(true),
        }

        Ok(false)
    }

    async fn ensure_relay_in_client(&self, relay_url: &RelayUrl) -> Result<()> {
        match self.client.relay(relay_url).await {
            Ok(_) => Ok(()),
            Err(_) => match self.config.relay_policy {
                RelaySessionRelayPolicy::Dynamic => {
                    self.client.add_relay(relay_url.clone()).await?;
                    Ok(())
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
            )
            .await;

        let first = telemetry.recv().await.unwrap();
        assert_eq!(first.kind, RelayTelemetryKind::SubscriptionAttempt);
    }
}
