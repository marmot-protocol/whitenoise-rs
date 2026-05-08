//! Types for the group state streaming feature.
//!
//! Group state events are state changes for a group, from the perspective of a
//! single account, that an open chat view should react to. Today this means the
//! account leaving (`LeftGroup`) or being removed by an admin
//! (`RemovedFromGroup`); both also flow through chat-list streaming, but a
//! consumer that is only subscribed to message updates would otherwise miss them.

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// A state change for the group identified by the stream's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GroupStateUpdate {
    /// The account voluntarily left this group via SelfRemove.
    LeftGroup,
    /// The account was involuntarily removed from this group by an admin.
    RemovedFromGroup,
}

/// Result of subscribing to a group's state stream.
///
/// Real-time only — there is no initial snapshot, since group state events are
/// transitions rather than persistent state.
pub struct GroupStateSubscription {
    pub updates: broadcast::Receiver<GroupStateUpdate>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialization_roundtrip() {
        for update in [
            GroupStateUpdate::LeftGroup,
            GroupStateUpdate::RemovedFromGroup,
        ] {
            let serialized = serde_json::to_string(&update).expect("serialize");
            let deserialized: GroupStateUpdate =
                serde_json::from_str(&serialized).expect("deserialize");
            assert_eq!(update, deserialized);
        }
    }
}
