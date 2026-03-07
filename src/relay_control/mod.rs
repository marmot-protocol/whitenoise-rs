//! Internal relay-control boundary.
//!
//! Phase 0 intentionally introduces only the boundary and shared types. Runtime
//! behavior continues to flow through the existing `NostrManager` paths until
//! later phases migrate individual relay workloads onto dedicated sessions.
#![allow(clippy::large_enum_variant)]

use core::str::FromStr;
use std::sync::Arc;

use nostr_sdk::{PublicKey, RelayUrl};

pub(crate) mod account_inbox;
pub(crate) mod discovery;
pub(crate) mod ephemeral;
pub(crate) mod groups;
pub(crate) mod observability;
pub(crate) mod router;
pub(crate) mod sessions;

use crate::whitenoise::database::{Database, DatabaseError};

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
    discovery: discovery::DiscoveryPlaneConfig,
    router: router::RelayRouter,
    observability: observability::RelayObservability,
}

#[allow(dead_code)]
impl RelayControlPlane {
    /// Create the inactive Phase 0 control-plane host.
    pub(crate) fn new(database: Arc<Database>, discovery_relays: Vec<RelayUrl>) -> Self {
        Self {
            database,
            discovery: discovery::DiscoveryPlaneConfig::new(discovery_relays),
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
    ) -> Result<(), DatabaseError> {
        self.observability.record(&self.database, telemetry).await
    }

    /// Discovery-plane configuration, including the configured relay set.
    pub(crate) fn discovery(&self) -> &discovery::DiscoveryPlaneConfig {
        &self.discovery
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

    fn from_str(value: &str) -> Result<Self, Self::Err> {
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
    DiscoveryMetadata,
    DiscoveryRelayLists,
    GroupMessages,
    AccountInboxGiftwraps,
}

#[allow(dead_code)]
impl SubscriptionStream {
    /// Stable identifier used only within White Noise.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::DiscoveryMetadata => "discovery_metadata",
            Self::DiscoveryRelayLists => "discovery_relay_lists",
            Self::GroupMessages => "group_messages",
            Self::AccountInboxGiftwraps => "account_inbox_giftwraps",
        }
    }
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
