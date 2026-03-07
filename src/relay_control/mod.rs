//! Internal relay-control boundary.
//!
//! Phase 0 intentionally introduces only the boundary and shared types. Runtime
//! behavior continues to flow through the existing `NostrManager` paths until
//! later phases migrate individual relay workloads onto dedicated sessions.
#![allow(clippy::large_enum_variant)]

use core::str::FromStr;
use std::{collections::HashMap, sync::Arc};

use nostr_sdk::{PublicKey, RelayUrl};
use sha2::{Digest, Sha256};
use tokio::sync::{RwLock, mpsc::Sender};

pub(crate) mod account_inbox;
pub(crate) mod discovery;
pub(crate) mod ephemeral;
pub(crate) mod groups;
pub(crate) mod observability;
pub(crate) mod router;
pub(crate) mod sessions;

use crate::whitenoise::database::{Database, DatabaseError};
use crate::{
    nostr_manager::Result as NostrResult,
    types::{AccountInboxPlanesStateSnapshot, ProcessableEvent, RelayControlStateSnapshot},
};

/// Top-level relay-control owner hosted by `Whitenoise`.
///
/// This type defines the long-term system boundary described in
/// `relay-control-plane-rearchitecture.md`. In Phase 0 it only stores shared
/// state and typed configuration; production code does not yet route relay
/// work through it.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct RelayControlPlane {
    database: Arc<Database>,
    event_sender: Sender<ProcessableEvent>,
    session_salt: [u8; 16],
    discovery: discovery::DiscoveryPlane,
    account_inbox_planes: RwLock<HashMap<PublicKey, account_inbox::AccountInboxPlane>>,
    group_plane: groups::GroupPlane,
    router: router::RelayRouter,
    observability: observability::RelayObservability,
}

#[allow(dead_code)]
impl RelayControlPlane {
    /// Create the inactive Phase 0 control-plane host.
    pub(crate) fn new(
        database: Arc<Database>,
        discovery_relays: Vec<RelayUrl>,
        event_sender: Sender<ProcessableEvent>,
        session_salt: [u8; 16],
    ) -> Self {
        Self {
            database,
            event_sender: event_sender.clone(),
            session_salt,
            discovery: discovery::DiscoveryPlane::new(
                discovery::DiscoveryPlaneConfig::new(discovery_relays),
                event_sender.clone(),
            ),
            account_inbox_planes: RwLock::new(HashMap::new()),
            group_plane: groups::GroupPlane::new(event_sender, session_salt),
            router: router::RelayRouter::default(),
            observability: observability::RelayObservability::new(
                observability::RelayObservabilityConfig::default(),
            ),
        }
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
        self.observability.record(&self.database, telemetry).await
    }

    pub(crate) fn session_salt(&self) -> &[u8; 16] {
        &self.session_salt
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
    /// **Atomicity:** Activation is NOT atomic across planes. Group
    /// subscriptions are established first; if inbox activation subsequently
    /// fails, group subscriptions will already be active. Callers that receive
    /// an error should call [`Self::deactivate_account_subscriptions`] to clean
    /// up any partially-established state.
    pub(crate) async fn activate_account_subscriptions(
        &self,
        account_pubkey: PublicKey,
        inbox_relays: &[RelayUrl],
        group_relays: &[RelayUrl],
        group_ids: &[String],
        since: Option<nostr_sdk::Timestamp>,
        signer: Arc<dyn nostr_sdk::NostrSigner>,
    ) -> NostrResult<()> {
        self.group_plane
            .update_account(account_pubkey, group_relays, group_ids, since)
            .await?;

        let plane = account_inbox::AccountInboxPlane::new(
            account_inbox::AccountInboxPlaneConfig::new(account_pubkey, inbox_relays.to_vec()),
            self.event_sender.clone(),
            self.session_salt,
        );
        self.account_inbox_planes
            .write()
            .await
            .insert(account_pubkey, plane.clone());
        if let Err(error) = plane.activate(inbox_relays, since, signer).await {
            self.account_inbox_planes
                .write()
                .await
                .remove(&account_pubkey);
            plane.deactivate().await;
            return Err(error);
        }
        Ok(())
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
