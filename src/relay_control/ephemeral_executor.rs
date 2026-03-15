use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use nostr_sdk::prelude::*;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc::Sender};

use super::{
    RelayPlane,
    observability::{RelayObservability, RelayTelemetry},
    sessions::{
        RelaySession, RelaySessionAuthPolicy, RelaySessionConfig, RelaySessionReconnectPolicy,
    },
};
use crate::{
    nostr_manager::Result,
    perf_span,
    types::{EphemeralPinnedRelayStateSnapshot, EphemeralScopeStateSnapshot, ProcessableEvent},
    whitenoise::database::Database,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EphemeralExecutorConfig {
    pub(crate) timeout: Duration,
    pub(crate) reconnect_policy: RelaySessionReconnectPolicy,
    pub(crate) auth_policy: RelaySessionAuthPolicy,
    pub(crate) ad_hoc_relay_ttl: Duration,
}

#[derive(Debug, Clone)]
pub(crate) struct EphemeralExecutor {
    config: EphemeralExecutorConfig,
    database: Arc<Database>,
    event_sender: Sender<ProcessableEvent>,
    observability: RelayObservability,
    sessions: Arc<RwLock<HashMap<Option<PublicKey>, Arc<EphemeralSessionEntry>>>>,
}

#[derive(Debug)]
struct EphemeralSessionEntry {
    session: RelaySession,
    tracking: Mutex<SessionRelayTracking>,
}

#[derive(Debug, Default)]
struct SessionRelayTracking {
    pinned_relays: HashMap<RelayUrl, usize>,
    ad_hoc_relays: HashMap<RelayUrl, Instant>,
}

impl EphemeralExecutor {
    pub(crate) fn new(
        config: EphemeralExecutorConfig,
        database: Arc<Database>,
        event_sender: Sender<ProcessableEvent>,
        observability: RelayObservability,
    ) -> Self {
        Self {
            config,
            database,
            event_sender,
            observability,
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub(crate) async fn warm_relays(&self, relays: &[RelayUrl]) -> Result<()> {
        let _span = perf_span!("relay::ephemeral_warm_relays");
        self.pin_relays(None, relays).await
    }

    pub(crate) async fn warm_relays_for_account(
        &self,
        account_pubkey: PublicKey,
        relays: &[RelayUrl],
    ) -> Result<()> {
        let _span = perf_span!("relay::ephemeral_warm_relays_for_account");
        self.pin_relays(Some(account_pubkey), relays).await
    }

    pub(crate) async fn unwarm_relays(&self, relays: &[RelayUrl]) -> Result<()> {
        let _span = perf_span!("relay::ephemeral_unwarm_relays");
        self.unpin_relays(None, relays).await
    }

    pub(crate) async fn fetch_events_from_scope(
        &self,
        account_pubkey: Option<PublicKey>,
        relays: &[RelayUrl],
        filter: Filter,
    ) -> Result<Events> {
        let _span = perf_span!("relay::ephemeral_fetch_events");
        let entry = self.session_entry(account_pubkey).await;
        entry
            .prepare_ad_hoc_relays(relays, self.config.ad_hoc_relay_ttl)
            .await?;
        entry
            .session
            .fetch_events_from(relays, filter, self.config.timeout)
            .await
    }

    pub(crate) async fn publish_event_to_scope(
        &self,
        account_pubkey: Option<PublicKey>,
        relays: &[RelayUrl],
        event: &Event,
    ) -> Result<Output<EventId>> {
        let _span = perf_span!("relay::ephemeral_publish_event");
        let entry = self.session_entry(account_pubkey).await;
        entry
            .prepare_ad_hoc_relays(relays, self.config.ad_hoc_relay_ttl)
            .await?;
        entry.session.publish_event_to(relays, event).await
    }

    pub(crate) async fn remove_account_scope(&self, account_pubkey: &PublicKey) {
        let _span = perf_span!("relay::ephemeral_remove_account_scope");
        let entry = self.sessions.write().await.remove(&Some(*account_pubkey));
        if let Some(entry) = entry {
            entry.session.shutdown().await;
        }
    }

    #[cfg(feature = "integration-tests")]
    pub(crate) async fn remove_all_account_scopes(&self) {
        let _span = perf_span!("relay::ephemeral_remove_all_account_scopes");
        let account_keys = self
            .sessions
            .read()
            .await
            .keys()
            .filter_map(|key| *key)
            .collect::<Vec<_>>();

        for account_pubkey in account_keys {
            self.remove_account_scope(&account_pubkey).await;
        }
    }

    async fn pin_relays(
        &self,
        account_pubkey: Option<PublicKey>,
        relays: &[RelayUrl],
    ) -> Result<()> {
        let _span = perf_span!("relay::ephemeral_pin_relays");
        let entry = self.session_entry(account_pubkey).await;
        entry.pin_relays(relays).await?;
        entry
            .prepare_ad_hoc_relays(&[], self.config.ad_hoc_relay_ttl)
            .await?;
        Ok(())
    }

    async fn unpin_relays(
        &self,
        account_pubkey: Option<PublicKey>,
        relays: &[RelayUrl],
    ) -> Result<()> {
        let _span = perf_span!("relay::ephemeral_unpin_relays");
        let entry = self.sessions.read().await.get(&account_pubkey).cloned();
        match entry {
            Some(entry) => entry.unpin_relays(relays).await,
            None => Ok(()),
        }
    }

    async fn session_entry(&self, account_pubkey: Option<PublicKey>) -> Arc<EphemeralSessionEntry> {
        let _span = perf_span!("relay::ephemeral_session_entry");
        if let Some(session) = self.sessions.read().await.get(&account_pubkey).cloned() {
            return session;
        }

        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get(&account_pubkey).cloned() {
            return session;
        }

        let session = RelaySession::new(
            self.session_config(account_pubkey),
            self.event_sender.clone(),
        );
        self.spawn_telemetry_persistor(account_pubkey, session.telemetry());

        let entry = Arc::new(EphemeralSessionEntry {
            session,
            tracking: Mutex::new(SessionRelayTracking::default()),
        });
        sessions.insert(account_pubkey, entry.clone());

        entry
    }

    fn session_config(&self, account_pubkey: Option<PublicKey>) -> RelaySessionConfig {
        let mut config = RelaySessionConfig::new(RelayPlane::Ephemeral);
        config.telemetry_account_pubkey = account_pubkey;
        config.auth_policy = self.config.auth_policy;
        config.reconnect_policy = self.config.reconnect_policy;
        config.connect_timeout = self.config.timeout;
        config.min_connected_relays = Some(2);
        config
    }

    fn spawn_telemetry_persistor(
        &self,
        account_pubkey: Option<PublicKey>,
        mut receiver: broadcast::Receiver<RelayTelemetry>,
    ) {
        let database = self.database.clone();
        let observability = self.observability.clone();
        let task_name = format!(
            "ephemeral_executor:{}",
            account_pubkey
                .map(|pubkey| pubkey.to_hex())
                .unwrap_or_else(|| "anonymous".to_string())
        );

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

    #[cfg(test)]
    pub(crate) async fn session_for_scope(
        &self,
        account_pubkey: Option<PublicKey>,
    ) -> RelaySession {
        self.session_entry(account_pubkey).await.session.clone()
    }

    pub(crate) async fn snapshot_scopes(
        &self,
    ) -> (
        Option<EphemeralScopeStateSnapshot>,
        Vec<EphemeralScopeStateSnapshot>,
    ) {
        let entries = self
            .sessions
            .read()
            .await
            .iter()
            .map(|(account_pubkey, entry)| (*account_pubkey, entry.clone()))
            .collect::<Vec<_>>();

        let mut anonymous = None;
        let mut accounts = Vec::new();

        for (account_pubkey, entry) in entries {
            let snapshot = entry.snapshot(account_pubkey).await;
            match account_pubkey {
                Some(_) => accounts.push(snapshot),
                None => anonymous = Some(snapshot),
            }
        }

        accounts.sort_unstable_by(|left, right| {
            left.scope_account_pubkey.cmp(&right.scope_account_pubkey)
        });

        (anonymous, accounts)
    }
}

impl EphemeralSessionEntry {
    async fn pin_relays(&self, relays: &[RelayUrl]) -> Result<()> {
        let unique_relays: HashSet<RelayUrl> = relays.iter().cloned().collect();

        {
            let mut tracking = self.tracking.lock().await;
            for relay_url in &unique_relays {
                *tracking.pinned_relays.entry(relay_url.clone()).or_insert(0) += 1;
                tracking.ad_hoc_relays.remove(relay_url);
            }
        }

        let relay_list: Vec<_> = unique_relays.into_iter().collect();
        self.session.ensure_relays_connected(&relay_list).await
    }

    async fn unpin_relays(&self, relays: &[RelayUrl]) -> Result<()> {
        let unique_relays: HashSet<RelayUrl> = relays.iter().cloned().collect();

        let to_remove = {
            let mut tracking = self.tracking.lock().await;
            let mut to_remove = Vec::new();

            for relay_url in unique_relays {
                match tracking.pinned_relays.get_mut(&relay_url) {
                    Some(ref_count) if *ref_count > 1 => {
                        *ref_count -= 1;
                    }
                    Some(_) => {
                        tracking.pinned_relays.remove(&relay_url);
                        if !tracking.ad_hoc_relays.contains_key(&relay_url) {
                            to_remove.push(relay_url);
                        }
                    }
                    None => {}
                }
            }

            to_remove
        };

        self.remove_relays(&to_remove).await
    }

    async fn prepare_ad_hoc_relays(&self, relays: &[RelayUrl], ttl: Duration) -> Result<()> {
        let unique_relays: HashSet<RelayUrl> = relays.iter().cloned().collect();
        let now = Instant::now();

        let expired_relays = {
            let mut tracking = self.tracking.lock().await;
            let mut expired_relays = Vec::new();

            for (relay_url, last_used_at) in &tracking.ad_hoc_relays {
                if unique_relays.contains(relay_url) {
                    continue;
                }
                if tracking.pinned_relays.contains_key(relay_url) {
                    continue;
                }
                if now.duration_since(*last_used_at) >= ttl {
                    expired_relays.push(relay_url.clone());
                }
            }

            for relay_url in &expired_relays {
                tracking.ad_hoc_relays.remove(relay_url);
            }

            for relay_url in unique_relays {
                if !tracking.pinned_relays.contains_key(&relay_url) {
                    tracking.ad_hoc_relays.insert(relay_url, now);
                }
            }

            expired_relays
        };

        self.remove_relays(&expired_relays).await?;

        Ok(())
    }

    async fn remove_relays(&self, relays: &[RelayUrl]) -> Result<()> {
        for relay_url in relays {
            if let Err(error) = self.session.remove_relay(relay_url).await {
                tracing::warn!(
                    target: "whitenoise::relay_control::ephemeral_executor",
                    relay_url = %relay_url,
                    "Failed to evict relay from ephemeral scope: {error}"
                );
            }
        }

        Ok(())
    }

    async fn snapshot(&self, account_pubkey: Option<PublicKey>) -> EphemeralScopeStateSnapshot {
        let (known_relays, pinned_relays, ad_hoc_relays) = {
            let tracking = self.tracking.lock().await;

            let mut known_relays = tracking
                .pinned_relays
                .keys()
                .cloned()
                .chain(tracking.ad_hoc_relays.keys().cloned())
                .collect::<Vec<_>>();
            known_relays.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
            known_relays.dedup();

            let mut pinned_relays = tracking
                .pinned_relays
                .iter()
                .map(|(relay_url, ref_count)| EphemeralPinnedRelayStateSnapshot {
                    relay_url: relay_url.to_string(),
                    ref_count: *ref_count,
                })
                .collect::<Vec<_>>();
            pinned_relays.sort_unstable_by(|left, right| left.relay_url.cmp(&right.relay_url));

            let mut ad_hoc_relays = tracking
                .ad_hoc_relays
                .keys()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            ad_hoc_relays.sort_unstable();

            (known_relays, pinned_relays, ad_hoc_relays)
        };

        EphemeralScopeStateSnapshot {
            scope_account_pubkey: account_pubkey.map(|pubkey| pubkey.to_hex()),
            pinned_relay_count: pinned_relays.len(),
            ad_hoc_relay_count: ad_hoc_relays.len(),
            pinned_relays,
            ad_hoc_relays,
            session: self.session.snapshot(&known_relays).await,
        }
    }
}
