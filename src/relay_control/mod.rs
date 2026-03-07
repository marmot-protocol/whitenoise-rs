//! Internal relay-control boundary.
//!
//! Phase 0 intentionally introduces only the boundary and shared types. Runtime
//! behavior continues to flow through the existing `NostrManager` paths until
//! later phases migrate individual relay workloads onto dedicated sessions.
#![allow(dead_code)]
#![allow(clippy::large_enum_variant)]

use std::sync::Arc;

use nostr_sdk::{PublicKey, RelayUrl};

pub(crate) mod account_inbox;
pub(crate) mod discovery;
pub(crate) mod ephemeral;
pub(crate) mod groups;
pub(crate) mod observability;
pub(crate) mod router;
pub(crate) mod sessions;

use crate::whitenoise::database::Database;

/// Top-level relay-control owner hosted by `Whitenoise`.
///
/// This type defines the long-term system boundary described in
/// `relay-control-plane-rearchitecture.md`. In Phase 0 it only stores shared
/// state and typed configuration; production code does not yet route relay
/// work through it.
#[derive(Debug)]
pub(crate) struct RelayControlPlane {
    database: Arc<Database>,
    router: router::RelayRouter,
    observability: observability::RelayObservability,
}

impl RelayControlPlane {
    /// Create the inactive Phase 0 control-plane host.
    pub(crate) fn new(database: Arc<Database>) -> Self {
        Self {
            database,
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
}

/// Logical relay workload partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RelayPlane {
    Discovery,
    Group,
    AccountInbox,
    Ephemeral,
    Compatibility,
}

impl RelayPlane {
    /// Stable identifier used for logs, persistence, and metrics labels.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Group => "group",
            Self::AccountInbox => "account_inbox",
            Self::Ephemeral => "ephemeral",
            Self::Compatibility => "compatibility",
        }
    }
}

/// Logical stream within a relay plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SubscriptionStream {
    DiscoveryMetadata,
    DiscoveryRelayLists,
    GroupMessages,
    AccountInboxGiftwraps,
    CompatibilityAccount,
    CompatibilityGlobal,
}

impl SubscriptionStream {
    /// Stable identifier used only within White Noise.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::DiscoveryMetadata => "discovery_metadata",
            Self::DiscoveryRelayLists => "discovery_relay_lists",
            Self::GroupMessages => "group_messages",
            Self::AccountInboxGiftwraps => "account_inbox_giftwraps",
            Self::CompatibilityAccount => "compatibility_account",
            Self::CompatibilityGlobal => "compatibility_global",
        }
    }
}

/// Local subscription-routing metadata for an opaque relay-facing subscription.
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
        assert_eq!(RelayPlane::Compatibility.as_str(), "compatibility");
    }

    #[test]
    fn test_subscription_stream_as_str() {
        assert_eq!(
            SubscriptionStream::AccountInboxGiftwraps.as_str(),
            "account_inbox_giftwraps"
        );
    }
}
