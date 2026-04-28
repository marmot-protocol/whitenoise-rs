// Intentionally not re-exported via `src/mdk.rs` — `RequiredProposal` is the
// whitenoise-owned mirror; this import is internal only.
use openmls::prelude::ProposalType;
use serde::{Deserialize, Serialize};

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
/// 4. **No-wildcard policy:** the [`From`] impl enumerates every
///    `openmls::prelude::ProposalType` variant explicitly so a future openmls
///    bump that adds one fails to compile here, forcing a conscious decision
///    rather than a silent collapse into `Unknown`.
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

impl From<ProposalType> for RequiredProposal {
    fn from(pt: ProposalType) -> Self {
        match pt {
            ProposalType::SelfRemove => Self::SelfRemove,
            ProposalType::Add
            | ProposalType::Update
            | ProposalType::Remove
            | ProposalType::PreSharedKey
            | ProposalType::Reinit
            | ProposalType::ExternalInit
            | ProposalType::GroupContextExtensions
            | ProposalType::Grease(_)
            | ProposalType::Custom(_) => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use openmls::prelude::ProposalType;

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
    fn from_self_remove_maps_to_self_remove() {
        assert_eq!(
            RequiredProposal::from(ProposalType::SelfRemove),
            RequiredProposal::SelfRemove,
        );
    }

    #[test]
    fn from_each_non_selfremove_variant_maps_to_unknown() {
        assert_eq!(
            RequiredProposal::from(ProposalType::Add),
            RequiredProposal::Unknown
        );
        assert_eq!(
            RequiredProposal::from(ProposalType::Update),
            RequiredProposal::Unknown
        );
        assert_eq!(
            RequiredProposal::from(ProposalType::Remove),
            RequiredProposal::Unknown
        );
        assert_eq!(
            RequiredProposal::from(ProposalType::PreSharedKey),
            RequiredProposal::Unknown,
        );
        assert_eq!(
            RequiredProposal::from(ProposalType::Reinit),
            RequiredProposal::Unknown
        );
        assert_eq!(
            RequiredProposal::from(ProposalType::ExternalInit),
            RequiredProposal::Unknown,
        );
        assert_eq!(
            RequiredProposal::from(ProposalType::GroupContextExtensions),
            RequiredProposal::Unknown,
        );
        assert_eq!(
            RequiredProposal::from(ProposalType::Grease(0x0A0A)),
            RequiredProposal::Unknown,
        );
        assert_eq!(
            RequiredProposal::from(ProposalType::Custom(0xF000)),
            RequiredProposal::Unknown,
        );
    }

    #[test]
    fn from_empty_set_stays_empty() {
        let mapped: BTreeSet<RequiredProposal> = BTreeSet::<ProposalType>::new()
            .into_iter()
            .map(RequiredProposal::from)
            .collect();
        assert!(mapped.is_empty());
    }

    #[test]
    fn btreeset_collapses_unknown_duplicates() {
        let source: BTreeSet<ProposalType> = [
            ProposalType::Add,
            ProposalType::Remove,
            ProposalType::Custom(0x100),
        ]
        .into_iter()
        .collect();

        let mapped: BTreeSet<RequiredProposal> =
            source.into_iter().map(RequiredProposal::from).collect();

        assert_eq!(mapped, BTreeSet::from([RequiredProposal::Unknown]));
    }
}
