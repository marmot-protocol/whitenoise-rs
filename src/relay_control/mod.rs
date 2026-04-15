//! Internal relay-control boundary.
//!
//! Long-lived discovery, group, and account-inbox subscriptions now run
//! through dedicated relay-plane sessions. Query and one-off publish flows now
//! run through dedicated relay-control sessions as the legacy shared-client
//! compatibility layer is retired incrementally.
#![allow(clippy::large_enum_variant)]

use core::str::FromStr;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use nostr_sdk::{PublicKey, RelayUrl};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, broadcast, mpsc::Sender};
use tokio::task::JoinHandle;

pub(crate) mod account_inbox;
pub(crate) mod discovery;
pub(crate) mod ephemeral;
pub(crate) mod ephemeral_executor;
pub(crate) mod groups;
pub(crate) mod observability;
pub(crate) mod router;
pub(crate) mod sessions;

use crate::perf_instrument;
use crate::whitenoise::database::{Database, DatabaseError};
use crate::{
    RelayType,
    nostr_manager::Result as NostrResult,
    types::{
        AccountInboxPlaneStateSnapshot, AccountInboxPlanesStateSnapshot, ProcessableEvent,
        RelayControlStateSnapshot,
    },
};

/// Top-level relay-control owner hosted by `Whitenoise`.
///
/// This type defines the long-term system boundary described in
/// `relay-control-plane-rearchitecture.md`. Discovery, group, and ephemeral
/// subscriptions route through this boundary, and it provides factory methods
/// for account inbox planes whose ownership lives in `AccountSession`.
#[derive(Debug)]
pub(crate) struct RelayControlPlane {
    database: Arc<Database>,
    event_sender: Sender<ProcessableEvent>,
    session_salt: [u8; 16],
    discovery: discovery::DiscoveryPlane,
    group_plane: groups::GroupPlane,
    ephemeral: ephemeral::EphemeralPlane,
    observability: observability::RelayObservability,
    telemetry_persistors_started: AtomicBool,
    telemetry_handles: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl RelayControlPlane {
    /// Create the relay-control host. Telemetry persistors are started during
    /// the explicit async startup phase once a Tokio runtime is available.
    pub(crate) fn new(
        database: Arc<Database>,
        discovery_relays: Vec<RelayUrl>,
        event_sender: Sender<ProcessableEvent>,
        session_salt: [u8; 16],
    ) -> Self {
        let observability = observability::RelayObservability::new(
            observability::RelayObservabilityConfig::default(),
        );
        let discovery = discovery::DiscoveryPlane::new(
            discovery::DiscoveryPlaneConfig::new(discovery_relays),
            event_sender.clone(),
        );
        let group_plane = groups::GroupPlane::new(event_sender.clone(), session_salt);
        let ephemeral = ephemeral::EphemeralPlane::new(
            ephemeral::EphemeralPlaneConfig::default(),
            database.clone(),
            event_sender.clone(),
            observability.clone(),
        );

        Self {
            database,
            event_sender,
            session_salt,
            discovery,
            group_plane,
            ephemeral,
            observability,
            telemetry_persistors_started: AtomicBool::new(false),
            telemetry_handles: Mutex::new(HashMap::new()),
        }
    }

    #[perf_instrument("relay")]
    pub(crate) async fn start_telemetry_persistors(&self) {
        if self
            .telemetry_persistors_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        self.spawn_telemetry_persistor("discovery", self.discovery.telemetry())
            .await;
        self.spawn_telemetry_persistor("group", self.group_plane.telemetry())
            .await;
    }

    /// Structured relay observability configuration and helpers.
    pub(crate) fn observability(&self) -> &observability::RelayObservability {
        &self.observability
    }

    /// Persist structured relay telemetry for later observability and retry work.
    #[perf_instrument("relay")]
    pub(crate) async fn record_relay_telemetry(
        &self,
        telemetry: &observability::RelayTelemetry,
    ) -> std::result::Result<(), DatabaseError> {
        if !Self::should_persist_telemetry(telemetry) {
            tracing::debug!(
                target: "whitenoise::relay_control::observability",
                plane = telemetry.plane.as_str(),
                relay_url = %telemetry.relay_url,
                kind = telemetry.kind.as_str(),
                "Skipping relay telemetry sample without required account scope"
            );
            return Ok(());
        }

        self.observability.record(&self.database, telemetry).await
    }

    pub(crate) fn session_salt(&self) -> &[u8; 16] {
        &self.session_salt
    }

    /// Spawn a telemetry writer, aborting any previous task registered under
    /// the same name so that re-activation does not leak persistor tasks.
    async fn spawn_telemetry_persistor(
        &self,
        task_name: &str,
        receiver: broadcast::Receiver<observability::RelayTelemetry>,
    ) {
        let handle = self.spawn_telemetry_persistor_task(task_name, receiver);
        let task_key = task_name.to_string();

        if let Some(previous) = self.telemetry_handles.lock().await.insert(task_key, handle) {
            previous.abort();
        }
    }

    fn spawn_telemetry_persistor_task(
        &self,
        task_name: &str,
        mut receiver: broadcast::Receiver<observability::RelayTelemetry>,
    ) -> JoinHandle<()> {
        let database = self.database.clone();
        let observability = self.observability.clone();
        let task_name = task_name.to_string();

        tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(telemetry) => {
                        if !Self::should_persist_telemetry(&telemetry) {
                            tracing::debug!(
                                target: "whitenoise::relay_control::observability",
                                task = task_name,
                                plane = telemetry.plane.as_str(),
                                relay_url = %telemetry.relay_url,
                                kind = telemetry.kind.as_str(),
                                "Skipping relay telemetry sample without required account scope"
                            );
                            continue;
                        }

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
        })
    }

    async fn abort_control_plane_telemetry_persistors(&self) {
        for (_, handle) in self.telemetry_handles.lock().await.drain() {
            handle.abort();
        }
        self.telemetry_persistors_started
            .store(false, Ordering::Release);
    }

    fn should_persist_telemetry(telemetry: &observability::RelayTelemetry) -> bool {
        !(telemetry.plane == RelayPlane::AccountInbox && telemetry.account_pubkey.is_none())
    }

    #[perf_instrument("relay")]
    pub(crate) async fn start_discovery_plane(&self) -> NostrResult<()> {
        let relays = self.discovery.relays();
        let (discovery_result, warm_result) =
            tokio::join!(self.discovery.start(), self.ephemeral.warm_relays(relays));
        discovery_result?;
        if let Err(error) = warm_result {
            tracing::warn!(
                target: "whitenoise::relay_control",
                "Failed to warm discovery relays on the ephemeral executor: {error}"
            );
        }
        Ok(())
    }

    /// Returns the number of groups in the group plane for an account.
    pub(crate) async fn group_plane_account_group_count(&self, pubkey: &PublicKey) -> usize {
        self.group_plane.account_group_count(pubkey).await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn sync_account_group_subscriptions(
        &self,
        account_pubkey: PublicKey,
        group_specs: &[groups::GroupSubscriptionSpec],
        since: Option<nostr_sdk::Timestamp>,
    ) -> NostrResult<()> {
        self.group_plane
            .update_account(account_pubkey, group_specs, since)
            .await
    }

    /// Snapshot the current group-plane state for an account (for rollback).
    pub(crate) async fn group_plane_account_state(
        &self,
        pubkey: &PublicKey,
    ) -> Option<Vec<groups::GroupSubscriptionSpec>> {
        self.group_plane.account_state(pubkey).await
    }

    /// Remove an account from the group plane entirely.
    pub(crate) async fn remove_account_from_group_plane(&self, pubkey: &PublicKey) {
        self.group_plane.remove_account(pubkey).await;
    }

    /// Create an inbox plane for an account. The caller owns the returned plane.
    pub(crate) fn create_account_inbox_plane(
        &self,
        account_pubkey: PublicKey,
        inbox_relays: Vec<RelayUrl>,
    ) -> account_inbox::AccountInboxPlane {
        account_inbox::AccountInboxPlane::new(
            account_inbox::AccountInboxPlaneConfig::new(account_pubkey, inbox_relays),
            self.event_sender.clone(),
            self.session_salt,
        )
    }

    /// Spawn a telemetry persistor task for an account inbox plane.
    /// The caller owns the returned handle and is responsible for aborting it.
    pub(crate) fn spawn_account_inbox_telemetry(
        &self,
        account_pubkey: &PublicKey,
        receiver: broadcast::Receiver<observability::RelayTelemetry>,
    ) -> JoinHandle<()> {
        self.spawn_telemetry_persistor_task(
            &format!("account_inbox:{}", account_pubkey.to_hex()),
            receiver,
        )
    }

    /// Shut down shared relay infrastructure (group, ephemeral, telemetry).
    ///
    /// Account inbox planes are owned by `AccountSession` and must be
    /// deactivated through the session before calling this method.
    #[perf_instrument("relay")]
    pub(crate) async fn shutdown_all(&self) {
        self.abort_control_plane_telemetry_persistors().await;
        self.group_plane.reset().await;
        self.ephemeral.remove_all_scopes().await;
    }

    /// Check if the group plane has an active subscription for this account.
    #[perf_instrument("relay")]
    pub(crate) async fn has_group_subscription(&self, account_pubkey: &PublicKey) -> bool {
        self.group_plane
            .has_active_subscription(account_pubkey)
            .await
    }

    /// Remove an account's ephemeral relay scopes.
    pub(crate) async fn remove_account_ephemeral_scope(&self, account_pubkey: &PublicKey) {
        self.ephemeral.remove_account_scope(account_pubkey).await;
    }

    #[perf_instrument("relay")]
    pub(crate) async fn has_discovery_subscriptions(&self) -> bool {
        self.discovery.has_subscriptions().await && self.discovery.has_connected_relay().await
    }

    /// Discovery-plane configuration, including the configured relay set.
    pub(crate) fn discovery(&self) -> &discovery::DiscoveryPlane {
        &self.discovery
    }

    pub(crate) fn ephemeral(&self) -> ephemeral::EphemeralPlane {
        self.ephemeral.clone()
    }

    #[perf_instrument("relay")]
    pub(crate) async fn warm_ephemeral_relays(&self, relays: &[RelayUrl]) -> NostrResult<()> {
        self.ephemeral.warm_relays(relays).await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn warm_ephemeral_relays_for_account(
        &self,
        account_pubkey: PublicKey,
        relays: &[RelayUrl],
    ) -> NostrResult<()> {
        self.ephemeral
            .warm_relays_for_account(account_pubkey, relays)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn unwarm_ephemeral_relays(&self, relays: &[RelayUrl]) -> NostrResult<()> {
        self.ephemeral.unwarm_relays(relays).await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_metadata_from(
        &self,
        relays: &[RelayUrl],
        pubkey: PublicKey,
    ) -> NostrResult<Option<nostr_sdk::Event>> {
        self.ephemeral.fetch_metadata_from(relays, pubkey).await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_user_relays(
        &self,
        pubkey: PublicKey,
        relay_type: RelayType,
        relays: &[RelayUrl],
    ) -> NostrResult<Option<nostr_sdk::Event>> {
        self.ephemeral
            .fetch_user_relays(pubkey, relay_type, relays)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn fetch_user_key_package(
        &self,
        pubkey: PublicKey,
        relays: &[RelayUrl],
    ) -> NostrResult<Option<nostr_sdk::Event>> {
        self.ephemeral.fetch_user_key_package(pubkey, relays).await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_welcome(
        &self,
        receiver: &PublicKey,
        rumor: nostr_sdk::UnsignedEvent,
        extra_tags: &[nostr_sdk::Tag],
        account_pubkey: PublicKey,
        relays: &[RelayUrl],
        signer: Arc<dyn nostr_sdk::NostrSigner>,
    ) -> NostrResult<nostr_sdk::prelude::Output<nostr_sdk::EventId>> {
        self.ephemeral
            .publish_gift_wrap_to(receiver, rumor, extra_tags, account_pubkey, relays, signer)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_event_to(
        &self,
        event: nostr_sdk::Event,
        account_pubkey: &PublicKey,
        relays: &[RelayUrl],
    ) -> NostrResult<nostr_sdk::prelude::Output<nostr_sdk::EventId>> {
        self.ephemeral
            .publish_event_to(event, account_pubkey, relays)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_metadata_with_signer(
        &self,
        metadata: &nostr_sdk::Metadata,
        relays: &[RelayUrl],
        signer: Arc<dyn nostr_sdk::NostrSigner>,
    ) -> NostrResult<nostr_sdk::prelude::Output<nostr_sdk::EventId>> {
        self.ephemeral
            .publish_metadata_with_signer(metadata, relays, signer)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_relay_list_with_signer(
        &self,
        relay_list: &[RelayUrl],
        relay_type: RelayType,
        target_relays: &[RelayUrl],
        signer: Arc<dyn nostr_sdk::NostrSigner>,
    ) -> NostrResult<()> {
        self.ephemeral
            .publish_relay_list_with_signer(relay_list, relay_type, target_relays, signer)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_follow_list_with_signer(
        &self,
        follow_list: &[PublicKey],
        target_relays: &[RelayUrl],
        signer: Arc<dyn nostr_sdk::NostrSigner>,
    ) -> NostrResult<()> {
        self.ephemeral
            .publish_follow_list_with_signer(follow_list, target_relays, signer)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_key_package_with_signer(
        &self,
        encoded_key_package: &str,
        relays: &[RelayUrl],
        tags: &[nostr_sdk::Tag],
        signer: Arc<dyn nostr_sdk::NostrSigner>,
    ) -> NostrResult<nostr_sdk::prelude::Output<nostr_sdk::EventId>> {
        self.ephemeral
            .publish_key_package_with_signer(encoded_key_package, relays, tags, signer)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_event_deletion_with_signer(
        &self,
        event_id: &nostr_sdk::EventId,
        relays: &[RelayUrl],
        signer: Arc<dyn nostr_sdk::NostrSigner>,
    ) -> NostrResult<nostr_sdk::prelude::Output<nostr_sdk::EventId>> {
        self.ephemeral
            .publish_event_deletion_with_signer(event_id, relays, signer)
            .await
    }

    #[perf_instrument("relay")]
    pub(crate) async fn publish_batch_event_deletion_with_signer(
        &self,
        event_ids: &[nostr_sdk::EventId],
        relays: &[RelayUrl],
        signer: Arc<dyn nostr_sdk::NostrSigner>,
    ) -> NostrResult<nostr_sdk::prelude::Output<nostr_sdk::EventId>> {
        self.ephemeral
            .publish_batch_event_deletion_with_signer(event_ids, relays, signer)
            .await
    }

    /// Build a diagnostic snapshot. Inbox snapshots are collected from sessions
    /// by the caller and passed in, since inbox planes are now session-owned.
    #[perf_instrument("relay")]
    pub(crate) async fn snapshot(
        &self,
        mut account_inbox_snapshots: Vec<AccountInboxPlaneStateSnapshot>,
    ) -> RelayControlStateSnapshot {
        let discovery = self.discovery.snapshot().await;
        let ephemeral = self.ephemeral.snapshot().await;

        account_inbox_snapshots
            .sort_unstable_by(|left, right| left.account_pubkey.cmp(&right.account_pubkey));

        RelayControlStateSnapshot {
            generated_at: nostr_sdk::Timestamp::now().as_secs(),
            discovery,
            ephemeral,
            account_inbox: AccountInboxPlanesStateSnapshot {
                active_account_count: account_inbox_snapshots.len(),
                accounts: account_inbox_snapshots,
            },
            group: self.group_plane.snapshot().await,
        }
    }

    #[cfg(feature = "integration-tests")]
    pub(crate) async fn reset_for_tests(&self) -> NostrResult<()> {
        self.discovery.retire_all().await;
        self.abort_control_plane_telemetry_persistors().await;
        self.group_plane.reset().await;
        self.ephemeral.remove_all_scopes().await;
        Ok(())
    }
}

/// Logical relay workload partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RelayPlane {
    Discovery,
    Group,
    AccountInbox,
    Ephemeral,
}

impl RelayPlane {
    /// Stable identifier used for logs, persistence, and metrics labels.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Group => "group",
            Self::AccountInbox => "account_inbox",
            Self::Ephemeral => "ephemeral",
        }
    }
}

impl FromStr for RelayPlane {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "discovery" => Ok(Self::Discovery),
            "group" => Ok(Self::Group),
            "account_inbox" => Ok(Self::AccountInbox),
            "ephemeral" => Ok(Self::Ephemeral),
            _ => Err(format!("invalid relay plane: {value}")),
        }
    }
}

/// Logical stream within a relay plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SubscriptionStream {
    DiscoveryUserData,
    DiscoveryFollowLists,
    GroupMessages,
    AccountInboxGiftwraps,
}

pub(crate) fn hash_pubkey_for_subscription_id(
    session_salt: &[u8; 16],
    pubkey: &PublicKey,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_salt);
    hasher.update(pubkey.to_bytes());
    format!("{:x}", hasher.finalize())[..12].to_string()
}

/// Local subscription-routing metadata for an opaque relay-facing subscription.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SubscriptionContext {
    pub(crate) plane: RelayPlane,
    pub(crate) account_pubkey: Option<PublicKey>,
    pub(crate) relay_url: RelayUrl,
    pub(crate) stream: SubscriptionStream,
    /// Hex-encoded Nostr group IDs carried in `#h` tags for group-message routing.
    pub(crate) group_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sqlx::sqlite::SqlitePoolOptions;
    use tokio::sync::broadcast;
    use tokio::time::{Duration, timeout};

    use super::*;
    use crate::relay_control::observability::{RelayTelemetry, RelayTelemetryKind};
    use crate::whitenoise::database::{Database, relay_status::RelayStatusRecord};

    #[test]
    fn test_relay_plane_as_str() {
        assert_eq!(RelayPlane::Discovery.as_str(), "discovery");
        assert_eq!(RelayPlane::AccountInbox.as_str(), "account_inbox");
    }

    #[test]
    fn test_relay_plane_from_str() {
        assert_eq!("group".parse::<RelayPlane>().unwrap(), RelayPlane::Group);
        assert!("not-a-plane".parse::<RelayPlane>().is_err());
    }

    async fn setup_test_db() -> Database {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        let database = Database {
            pool,
            path: ":memory:".to_owned().into(),
            last_connected: std::time::SystemTime::now(),
        };
        database.migrate_up().await.unwrap();
        database
    }

    #[tokio::test]
    async fn test_telemetry_persistor_records_events_and_status() {
        let database = Arc::new(setup_test_db().await);
        let (event_sender, _) = tokio::sync::mpsc::channel(8);
        let relay_control =
            RelayControlPlane::new(database.clone(), Vec::new(), event_sender, [1; 16]);
        let (telemetry_sender, telemetry_receiver) = broadcast::channel(8);
        relay_control
            .spawn_telemetry_persistor("test", telemetry_receiver)
            .await;

        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let telemetry = RelayTelemetry::new(
            RelayTelemetryKind::SubscriptionSuccess,
            RelayPlane::Discovery,
            relay_url.clone(),
        )
        .with_occurred_at(Utc::now())
        .with_subscription_id("sub-1");

        telemetry_sender.send(telemetry).unwrap();
        drop(telemetry_sender);

        // SubscriptionSuccess no longer writes to relay_events (Fix 6); verify
        // via relay_status counters instead.
        timeout(Duration::from_secs(1), async {
            loop {
                let status =
                    RelayStatusRecord::find(&relay_url, RelayPlane::Discovery, None, &database)
                        .await
                        .unwrap();

                if let Some(s) = status
                    && s.success_count == 1
                {
                    break;
                }

                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let account_pubkey = nostr_sdk::Keys::generate().public_key();
        let account_telemetry = RelayTelemetry::new(
            RelayTelemetryKind::SubscriptionSuccess,
            RelayPlane::AccountInbox,
            relay_url.clone(),
        )
        .with_account_pubkey(account_pubkey)
        .with_occurred_at(Utc::now())
        .with_subscription_id("account-sub-1");

        let (account_sender, account_receiver) = broadcast::channel(8);
        relay_control
            .spawn_telemetry_persistor("test-account", account_receiver)
            .await;
        account_sender.send(account_telemetry).unwrap();
        account_sender
            .send(
                RelayTelemetry::new(
                    RelayTelemetryKind::SubscriptionSuccess,
                    RelayPlane::AccountInbox,
                    relay_url.clone(),
                )
                .with_account_pubkey(account_pubkey)
                .with_occurred_at(Utc::now())
                .with_subscription_id("account-sub-2"),
            )
            .unwrap();
        drop(account_sender);

        // SubscriptionSuccess no longer writes to relay_events (Fix 6); check
        // relay_status success_count reached 2 instead.
        timeout(Duration::from_secs(1), async {
            loop {
                let status = RelayStatusRecord::find(
                    &relay_url,
                    RelayPlane::AccountInbox,
                    Some(account_pubkey),
                    &database,
                )
                .await
                .unwrap();

                if let Some(s) = status
                    && s.success_count == 2
                {
                    assert_eq!(s.account_pubkey, Some(account_pubkey));
                    break;
                }

                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        relay_control
            .record_relay_telemetry(
                &RelayTelemetry::new(
                    RelayTelemetryKind::SubscriptionSuccess,
                    RelayPlane::AccountInbox,
                    relay_url.clone(),
                )
                .with_occurred_at(Utc::now())
                .with_subscription_id("account-sub-ignored"),
            )
            .await
            .unwrap();

        assert!(
            RelayStatusRecord::find(&relay_url, RelayPlane::AccountInbox, None, &database)
                .await
                .unwrap()
                .is_none(),
            "account inbox telemetry without an account scope must be ignored"
        );
    }

    #[tokio::test]
    async fn test_snapshot_includes_ephemeral_plane() {
        let database = Arc::new(setup_test_db().await);
        let (event_sender, _) = tokio::sync::mpsc::channel(8);
        let relay_control = RelayControlPlane::new(database, Vec::new(), event_sender, [1; 16]);

        let snapshot = relay_control.snapshot(vec![]).await;

        assert_eq!(snapshot.ephemeral.account_scope_count, 0);
        assert!(snapshot.ephemeral.anonymous.is_none());
        assert!(snapshot.ephemeral.accounts.is_empty());
    }

    #[tokio::test]
    async fn test_spawn_telemetry_persistor_replaces_same_key() {
        let database = Arc::new(setup_test_db().await);
        let (event_sender, _) = tokio::sync::mpsc::channel(8);
        let relay_control =
            RelayControlPlane::new(database.clone(), Vec::new(), event_sender, [1; 16]);
        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let (sender1, receiver1) = broadcast::channel(8);
        let (sender2, receiver2) = broadcast::channel(8);

        relay_control
            .spawn_telemetry_persistor("same-key", receiver1)
            .await;
        relay_control
            .spawn_telemetry_persistor("same-key", receiver2)
            .await;

        timeout(Duration::from_secs(1), async {
            loop {
                if sender1.receiver_count() == 0 && sender2.receiver_count() == 1 {
                    break;
                }

                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        sender2
            .send(
                RelayTelemetry::new(
                    RelayTelemetryKind::SubscriptionSuccess,
                    RelayPlane::Discovery,
                    relay_url.clone(),
                )
                .with_occurred_at(Utc::now())
                .with_subscription_id("same-key-sub"),
            )
            .unwrap();
        drop(sender2);

        timeout(Duration::from_secs(1), async {
            loop {
                let status =
                    RelayStatusRecord::find(&relay_url, RelayPlane::Discovery, None, &database)
                        .await
                        .unwrap();

                if let Some(s) = status
                    && s.success_count == 1
                {
                    break;
                }

                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        drop(sender1);
    }

    #[tokio::test]
    async fn test_shutdown_all_clears_all_ephemeral_scopes() {
        let database = Arc::new(setup_test_db().await);
        let (event_sender, _) = tokio::sync::mpsc::channel(8);
        let relay_control = RelayControlPlane::new(database, Vec::new(), event_sender, [1; 16]);
        let anonymous_relay = RelayUrl::parse("ws://127.0.0.1:1").unwrap();
        let account_relay = RelayUrl::parse("ws://127.0.0.1:2").unwrap();
        let account_pubkey = nostr_sdk::Keys::generate().public_key();

        let _ = relay_control
            .warm_ephemeral_relays(std::slice::from_ref(&anonymous_relay))
            .await;
        let _ = relay_control
            .warm_ephemeral_relays_for_account(account_pubkey, std::slice::from_ref(&account_relay))
            .await;

        let before = relay_control.snapshot(vec![]).await;
        assert!(before.ephemeral.anonymous.is_some());
        assert_eq!(before.ephemeral.account_scope_count, 1);

        relay_control.shutdown_all().await;

        let after = relay_control.snapshot(vec![]).await;
        assert!(after.ephemeral.anonymous.is_none());
        assert_eq!(after.ephemeral.account_scope_count, 0);
    }

    #[tokio::test]
    async fn test_shutdown_all_resets_telemetry_persistor_start_gate() {
        let database = Arc::new(setup_test_db().await);
        let (event_sender, _) = tokio::sync::mpsc::channel(8);
        let relay_control = RelayControlPlane::new(database, Vec::new(), event_sender, [1; 16]);

        relay_control.start_telemetry_persistors().await;
        assert!(
            relay_control
                .telemetry_persistors_started
                .load(Ordering::Acquire)
        );
        assert_eq!(relay_control.telemetry_handles.lock().await.len(), 2);

        relay_control.shutdown_all().await;

        assert!(
            !relay_control
                .telemetry_persistors_started
                .load(Ordering::Acquire)
        );
        assert!(relay_control.telemetry_handles.lock().await.is_empty());

        relay_control.start_telemetry_persistors().await;
        assert!(
            relay_control
                .telemetry_persistors_started
                .load(Ordering::Acquire)
        );
        assert_eq!(relay_control.telemetry_handles.lock().await.len(), 2);
    }
}
