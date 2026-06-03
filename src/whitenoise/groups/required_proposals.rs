use std::collections::BTreeSet;

use cgka_traits::capabilities::FeatureStatus;
use nostr_sdk::prelude::PublicKey;
use serde::{Deserialize, Serialize};

use crate::marmot::capabilities::{APP_DATA_UPDATE_PROPOSAL_CODEPOINT, SELF_REMOVE_CODEPOINT};
use crate::whitenoise::error::WhitenoiseError;

/// A whitenoise-owned mirror of the MLS proposal-type registry, restricted to
/// the capability surface exposed through [`crate::Whitenoise::group_required_proposals`].
///
/// 1. **Wire format:** serialized as snake_case JSON strings (`"self_remove"`,
///    `"unknown"`). Mirror this contract in any consumer that parses the
///    response.
/// 2. **`Unknown` semantics:** a catch-all for any `openmls` proposal type
///    whitenoise does not model today. Cardinality in the returned set reflects
///    distinct *mirror* values, not distinct source values — multiple source
///    variants collapsing here appear as a single `Unknown` entry.
/// 3. **Set semantics:** an empty set is the LCD outcome for mixed-capability
///    or empty-invitee groups and is distinct from
///    [`crate::WhitenoiseError::GroupNotFound`], which means the MLS record is
///    missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredProposal {
    /// MIP-03 `SelfRemove` (`0x000a`) — when present in a group's required
    /// capabilities, non-admin members can voluntarily leave without an admin
    /// commit.
    SelfRemove,
    /// Any proposal type the whitenoise API does not distinguish today.
    Unknown,
}

/// Upgrade readiness for the proposal types WhiteNoise exposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCapabilityUpgradeStatus {
    pub per_proposal: Vec<RequiredProposalUpgradeStatus>,
}

/// Upgrade readiness for one modeled required proposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredProposalUpgradeStatus {
    pub proposal: RequiredProposal,
    pub state: RequiredProposalUpgradability,
}

/// Whether a modeled proposal can be added to the group's `RequiredCapabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredProposalUpgradability {
    AlreadyRequired,
    Available,
    Blocked { blockers: Vec<PublicKey> },
}

pub(crate) fn validate_required_proposal_upgrade_targets(
    proposals: &BTreeSet<RequiredProposal>,
) -> Result<(), WhitenoiseError> {
    if proposals.contains(&RequiredProposal::Unknown) {
        return Err(WhitenoiseError::InvalidInput(
            "RequiredProposal::Unknown is a sentinel and cannot be used as an upgrade target"
                .to_string(),
        ));
    }

    Ok(())
}

pub(crate) fn project_darkmatter_self_remove_upgrade_status(
    status: FeatureStatus,
    blockers: Vec<PublicKey>,
) -> GroupCapabilityUpgradeStatus {
    let state = match status {
        FeatureStatus::Available => RequiredProposalUpgradability::AlreadyRequired,
        FeatureStatus::Upgradeable => RequiredProposalUpgradability::Available,
        FeatureStatus::Unavailable { .. } => RequiredProposalUpgradability::Blocked { blockers },
    };

    GroupCapabilityUpgradeStatus {
        per_proposal: vec![RequiredProposalUpgradeStatus {
            proposal: RequiredProposal::SelfRemove,
            state,
        }],
    }
}

impl From<u16> for RequiredProposal {
    /// Maps a raw proposal codepoint to the mirror enum.
    ///
    /// Used by the key-package capability projection (which parses Nostr
    /// `mls_proposals` tag hex strings into `u16`s). The `_` arm is the
    /// Every codepoint we don't model collapses to [`RequiredProposal::Unknown`].
    fn from(codepoint: u16) -> Self {
        match codepoint {
            SELF_REMOVE_CODEPOINT => Self::SelfRemove,
            _ => Self::Unknown,
        }
    }
}

/// Projects Darkmatter `Group.required_capabilities.proposals` onto the
/// existing WhiteNoise public API.
///
/// Darkmatter requires OpenMLS `AppDataUpdate` for its app-component runtime.
/// That proposal is an engine invariant rather than a user-visible group
/// policy, so exposing it as [`RequiredProposal::Unknown`] would make every
/// Darkmatter group look like it had an unknown public requirement. Unknown
/// non-baseline proposals are still preserved.
pub(crate) fn project_darkmatter_required_proposals(
    proposals: impl IntoIterator<Item = u16>,
) -> BTreeSet<RequiredProposal> {
    proposals
        .into_iter()
        .filter(|proposal| *proposal != APP_DATA_UPDATE_PROPOSAL_CODEPOINT)
        .map(RequiredProposal::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use nostr_sdk::Keys;

    use super::*;

    #[test]
    fn snake_case_serde_contract() {
        assert_eq!(
            serde_json::to_value(RequiredProposal::SelfRemove).unwrap(),
            serde_json::json!("self_remove"),
        );
        assert_eq!(
            serde_json::to_value(RequiredProposal::Unknown).unwrap(),
            serde_json::json!("unknown"),
        );
    }

    #[test]
    fn upgrade_status_serde_round_trip_preserves_payload() {
        let blocker = Keys::generate().public_key();
        let original = GroupCapabilityUpgradeStatus {
            per_proposal: vec![RequiredProposalUpgradeStatus {
                proposal: RequiredProposal::SelfRemove,
                state: RequiredProposalUpgradability::Blocked {
                    blockers: vec![blocker],
                },
            }],
        };

        let json = serde_json::to_string(&original).unwrap();
        let round_tripped: GroupCapabilityUpgradeStatus = serde_json::from_str(&json).unwrap();

        assert_eq!(round_tripped, original);
    }

    #[test]
    fn from_self_remove_codepoint_maps_to_self_remove() {
        assert_eq!(
            RequiredProposal::from(SELF_REMOVE_CODEPOINT),
            RequiredProposal::SelfRemove,
        );
    }

    #[test]
    fn from_unmodeled_codepoint_maps_to_unknown() {
        assert_eq!(RequiredProposal::from(0xf000), RequiredProposal::Unknown);
    }

    #[test]
    fn from_empty_set_stays_empty() {
        let mapped: BTreeSet<RequiredProposal> = BTreeSet::<u16>::new()
            .into_iter()
            .map(RequiredProposal::from)
            .collect();
        assert!(mapped.is_empty());
    }

    #[test]
    fn btreeset_collapses_unknown_duplicates() {
        let source: BTreeSet<u16> = [0x0001, 0x0002, 0x0100].into_iter().collect();

        let mapped: BTreeSet<RequiredProposal> =
            source.into_iter().map(RequiredProposal::from).collect();

        assert_eq!(mapped, BTreeSet::from([RequiredProposal::Unknown]));
    }

    #[test]
    fn project_darkmatter_required_proposals_hides_app_data_update_baseline() {
        let mapped = project_darkmatter_required_proposals([
            APP_DATA_UPDATE_PROPOSAL_CODEPOINT,
            SELF_REMOVE_CODEPOINT,
        ]);

        assert_eq!(mapped, BTreeSet::from([RequiredProposal::SelfRemove]));
    }

    #[test]
    fn project_darkmatter_required_proposals_keeps_non_baseline_unknowns() {
        let mapped =
            project_darkmatter_required_proposals([APP_DATA_UPDATE_PROPOSAL_CODEPOINT, 0xf000]);

        assert_eq!(mapped, BTreeSet::from([RequiredProposal::Unknown]));
    }
}
