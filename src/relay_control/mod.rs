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
use tokio::sync::{RwLock, broadcast, mpsc::Sender};

pub(crate) mod account_inbox;
pub(crate) mod discovery;
pub(crate) mod ephemeral;
pub(crate) mod groups;
pub(crate) mod observability;
pub(crate) mod router;
pub(crate) mod sessions;

use crate::whitenoise::database::{Database, DatabaseError};
use crate::{
    RelayType,
    nostr_manager::Result as NostrResult,
    types::{AccountInboxPlanesStateSnapshot, ProcessableEvent, RelayControlStateSnapshot},
};

/// Top-level relay-control owner hosted by `Whitenoise`.
///
/// This type defines the long-term system boundary described in
/// `relay-control-plane-rearchitecture.md`. Discovery, group, and account
/// inbox subscriptions already route through this boundary; remaining query
/// and publish work still migrates incrementally.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct RelayControlPlane {
    database: Arc<Database>,
    event_sender: Sender<ProcessableEvent>,
    session_salt: [u8; 16],
    discovery: discovery::DiscoveryPlane,
    account_inbox_planes: RwLock<HashMap<PublicKey, account_inbox::AccountInboxPlane>>,
    group_plane: groups::GroupPlane,
    ephemeral: ephemeral::EphemeralPlane,
    router: router::RelayRouter,
    observability: observability::RelayObservability,
    telemetry_persistors_started: AtomicBool,
}

#[allow(dead_code)]
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
            event_sender: event_sender.clone(),
            session_salt,
            discovery,
            account_inbox_planes: RwLock::new(HashMap::new()),
            group_plane,
            ephemeral,
            router: router::RelayRouter::default(),
            observability,
            telemetry_persistors_started: AtomicBool::new(false),
        }
    }

    pub(crate) async fn start_telemetry_persistors(&self) {
        if self
            .telemetry_persistors_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        self.spawn_telemetry_persistor("discovery", self.discovery.telemetry());
        self.spawn_telemetry_persistor("group", self.group_plane.telemetry());
    }

    /// Access to the shared application database for later relay-control phases.
    pub(crate) fn database(&self) -> &Arc<Database> {
        &self.database
    }

    /// Local relay-routing metadata owned by the control plane.
    pub(crate) fn router(&self) -> &router::RelayRouter {
        &self.router
    }

    /// Structured relay observability configuration and helpers.
    pub(crate) fn observability(&self) -> &observability::RelayObservability {
        &self.observability
    }

    /// Persist structured relay telemetry for later observability and retry work.
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

    /// Spawn a fire-and-forget telemetry writer that exits when the telemetry
    /// sender for the subscribed plane is dropped and the broadcast channel closes.
    fn spawn_telemetry_persistor(
        &self,
        task_name: &str,
        mut receiver: broadcast::Receiver<observability::RelayTelemetry>,
    ) {
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
        });
    }

    fn should_persist_telemetry(telemetry: &observability::RelayTelemetry) -> bool {
        !(telemetry.plane == RelayPlane::AccountInbox && telemetry.account_pubkey.is_none())
    }

    pub(crate) async fn start_discovery_plane(&self) -> NostrResult<()> {
        self.discovery.start().await
    }

    pub(crate) async fn sync_discovery_subscriptions(
        &self,
        watched_users: &[PublicKey],
        follow_list_accounts: &[(PublicKey, Option<nostr_sdk::Timestamp>)],
        public_since: Option<nostr_sdk::Timestamp>,
    ) -> NostrResult<()> {
        self.discovery
            .sync(watched_users, follow_list_accounts, public_since)
            .await
    }

    /// Activate group and inbox subscriptions for an account.
    ///
    /// **Atomicity:** First-time activation is NOT atomic across planes. Group
    /// subscriptions are established first; if inbox activation subsequently
    /// fails, group subscriptions may already be active. Refreshing an already
    /// active account attempts to restore the previous group state on inbox
    /// activation failure.
    pub(crate) async fn activate_account_subscriptions(
        &self,
        account_pubkey: PublicKey,
        inbox_relays: &[RelayUrl],
        group_specs: &[groups::GroupSubscriptionSpec],
        since: Option<nostr_sdk::Timestamp>,
        signer: Arc<dyn nostr_sdk::NostrSigner>,
    ) -> NostrResult<()> {
        let previous_group_state = self.group_plane.account_state(&account_pubkey).await;

        self.group_plane
            .update_account(account_pubkey, group_specs, since)
            .await?;

        let plane = account_inbox::AccountInboxPlane::new(
            account_inbox::AccountInboxPlaneConfig::new(account_pubkey, inbox_relays.to_vec()),
            self.event_sender.clone(),
            self.session_salt,
        );

        if let Err(error) = plane.activate(inbox_relays, since, signer).await {
            plane.deactivate().await;

            if let Some(previous_group_specs) = previous_group_state {
                if let Err(restore_error) = self
                    .group_plane
                    .update_account(account_pubkey, &previous_group_specs, since)
                    .await
                {
                    tracing::error!(
                        target: "whitenoise::relay_control",
                        account_pubkey = %account_pubkey,
                        "Failed to restore previous group-plane state after inbox activation error: {restore_error}"
                    );
                }
            } else {
                self.group_plane.remove_account(&account_pubkey).await;
            }

            return Err(error);
        }

        let telemetry_receiver = plane.telemetry();

        if let Some(previous_plane) = self
            .account_inbox_planes
            .write()
            .await
            .insert(account_pubkey, plane)
        {
            previous_plane.deactivate().await;
        }

        self.spawn_telemetry_persistor(
            &format!("account_inbox:{}", account_pubkey.to_hex()),
            telemetry_receiver,
        );

        Ok(())
    }

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

    pub(crate) async fn deactivate_account_subscriptions(&self, account_pubkey: &PublicKey) {
        if let Some(plane) = self
            .account_inbox_planes
            .write()
            .await
            .remove(account_pubkey)
        {
            plane.deactivate().await;
        }

        self.group_plane.remove_account(account_pubkey).await;
    }

    /// Deactivates all account subscriptions. Called during full data teardown.
    pub(crate) async fn shutdown_all(&self) {
        let pubkeys: Vec<_> = self
            .account_inbox_planes
            .read()
            .await
            .keys()
            .cloned()
            .collect();
        for pubkey in pubkeys {
            self.deactivate_account_subscriptions(&pubkey).await;
        }
    }

    pub(crate) async fn has_account_subscriptions(&self, account_pubkey: &PublicKey) -> bool {
        // Both planes must confirm the account is active. The group plane
        // keeps an entry even for accounts with zero groups (empty state), so
        // a missing entry unambiguously means setup never completed or failed.
        let inbox_healthy = {
            let plane = self
                .account_inbox_planes
                .read()
                .await
                .get(account_pubkey)
                .cloned();
            match plane {
                Some(plane) => plane.has_connected_relay().await,
                None => false,
            }
        };

        inbox_healthy
            && self
                .group_plane
                .has_active_subscription(account_pubkey)
                .await
    }

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

    pub(crate) async fn fetch_metadata_from(
        &self,
        relays: &[RelayUrl],
        pubkey: PublicKey,
    ) -> NostrResult<Option<nostr_sdk::Event>> {
        self.ephemeral.fetch_metadata_from(relays, pubkey).await
    }

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

    pub(crate) async fn fetch_user_key_package(
        &self,
        pubkey: PublicKey,
        relays: &[RelayUrl],
    ) -> NostrResult<Option<nostr_sdk::Event>> {
        self.ephemeral.fetch_user_key_package(pubkey, relays).await
    }

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

    pub(crate) async fn snapshot(&self) -> RelayControlStateSnapshot {
        let discovery = self.discovery.snapshot().await;

        let inbox_planes = self
            .account_inbox_planes
            .read()
            .await
            .iter()
            .map(|(pubkey, plane)| (*pubkey, plane.clone()))
            .collect::<Vec<_>>();

        let mut account_snapshots = Vec::with_capacity(inbox_planes.len());
        for (_, plane) in inbox_planes {
            account_snapshots.push(plane.snapshot().await);
        }
        account_snapshots
            .sort_unstable_by(|left, right| left.account_pubkey.cmp(&right.account_pubkey));

        RelayControlStateSnapshot {
            generated_at: nostr_sdk::Timestamp::now().as_secs(),
            discovery,
            account_inbox: AccountInboxPlanesStateSnapshot {
                active_account_count: account_snapshots.len(),
                accounts: account_snapshots,
            },
            group: self.group_plane.snapshot().await,
        }
    }

    #[cfg(feature = "integration-tests")]
    pub(crate) async fn reset_for_tests(&self) -> NostrResult<()> {
        self.sync_discovery_subscriptions(&[], &[], None).await?;

        let inbox_planes = self
            .account_inbox_planes
            .write()
            .await
            .drain()
            .map(|(_, plane)| plane)
            .collect::<Vec<_>>();

        for plane in inbox_planes {
            plane.deactivate().await;
        }

        self.group_plane.reset().await;
        Ok(())
    }
}

/// Logical relay workload partition.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RelayPlane {
    Discovery,
    Group,
    AccountInbox,
    Ephemeral,
}

#[allow(dead_code)]
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
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SubscriptionStream {
    DiscoveryUserData,
    DiscoveryFollowLists,
    GroupMessages,
    AccountInboxGiftwraps,
}

#[allow(dead_code)]
impl SubscriptionStream {
    /// Stable identifier used only within White Noise.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::DiscoveryUserData => "discovery_user_data",
            Self::DiscoveryFollowLists => "discovery_follow_lists",
            Self::GroupMessages => "group_messages",
            Self::AccountInboxGiftwraps => "account_inbox_giftwraps",
        }
    }
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
#[allow(dead_code)]
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

    #[test]
    fn test_subscription_stream_as_str() {
        assert_eq!(
            SubscriptionStream::AccountInboxGiftwraps.as_str(),
            "account_inbox_giftwraps"
        );
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
        relay_control.spawn_telemetry_persistor("test", telemetry_receiver);

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
        relay_control.spawn_telemetry_persistor("test-account", account_receiver);
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
}
