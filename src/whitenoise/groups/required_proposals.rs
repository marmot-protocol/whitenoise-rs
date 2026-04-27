// Intentionally not re-exported via `src/mdk.rs` — `RequiredProposal` is the
// whitenoise-owned mirror; this import is internal only.
use std::collections::BTreeSet;

use mdk_core::prelude::ProposalType;
use serde::{Deserialize, Serialize};

/// SelfRemove codepoint (`0x000a`). Used both as a proposal codepoint
/// ([`RequiredProposal::SelfRemove`]) and an extension codepoint
/// ([`MlsExtensionId::SelfRemove`]). Per RFC 9420 the two registries are
/// distinct namespaces that happen to share this value for the SelfRemove
/// pair.
pub(crate) const SELF_REMOVE_CODEPOINT: u16 = 0x000a;

/// Marmot NostrGroupData extension codepoint (`0xf2ee`). Matches
/// `mdk_core::constant::NOSTR_GROUP_DATA_EXTENSION_TYPE`.
pub(crate) const NOSTR_GROUP_DATA_EXTENSION_CODEPOINT: u16 = 0xf2ee;

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

/// A whitenoise-owned mirror of the MLS extension-type registry, restricted to
/// the codepoints surfaced through key-package capability projection
/// ([`crate::whitenoise::key_packages::marmot_key_package_capabilities`]).
///
/// Same wire/policy contract as [`RequiredProposal`]:
///
/// 1. Snake-case JSON (`"self_remove"`, `"nostr_group_data"`, `"unknown"`).
/// 2. `Unknown` is a unit variant — duplicates collapse in any
///    [`BTreeSet<MlsExtensionId>`].
///
/// **No-wildcard policy.** The plan's design called for a
/// `From<openmls::ExtensionType>` impl mirroring [`RequiredProposal`]'s
/// `From<ProposalType>`. We diverge: `openmls` is not a top-level dependency of
/// this crate (only mdk-core uses it) and the projection actually parses
/// _Nostr-event tag hex strings_, not openmls types. The
/// [`From<u16>`] impl below is the corresponding no-wildcard surface — every
/// branch is explicit, including the catch-all `_` arm which fans every other
/// codepoint (including unmodelled openmls variants like `ApplicationId =
/// 0x0001`) into [`MlsExtensionId::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MlsExtensionId {
    /// MIP-03 SelfRemove (`0x000a`) — present as a leaf-node capability when
    /// the peer can participate in SelfRemove proposals.
    SelfRemove,
    /// Marmot NostrGroupData (`0xf2ee`) — Marmot identity extension required by
    /// every Marmot key package.
    NostrGroupData,
    /// Any extension codepoint the whitenoise API does not distinguish today.
    Unknown,
}

impl From<u16> for MlsExtensionId {
    fn from(codepoint: u16) -> Self {
        match codepoint {
            SELF_REMOVE_CODEPOINT => Self::SelfRemove,
            NOSTR_GROUP_DATA_EXTENSION_CODEPOINT => Self::NostrGroupData,
            _ => Self::Unknown,
        }
    }
}

/// A KP's advertised capability set, projected from its Nostr-event tags.
///
/// Built by [`crate::whitenoise::key_packages::marmot_key_package_capabilities`]
/// at the validation boundary so callers don't re-walk tags. Both fields are
/// `BTreeSet`s so duplicates collapse and iteration order is deterministic for
/// logging and tests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct KeyPackageCapabilities {
    pub proposals: BTreeSet<RequiredProposal>,
    pub extensions: BTreeSet<MlsExtensionId>,
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

impl From<u16> for RequiredProposal {
    /// Maps a raw proposal codepoint to the mirror enum.
    ///
    /// Used by the key-package capability projection (which parses Nostr
    /// `mls_proposals` tag hex strings into `u16`s). The `_` arm is the
    /// proposal-namespace dual of `MlsExtensionId::Unknown` (a sibling
    /// crate-private mirror) — every codepoint we don't model collapses
    /// there.
    fn from(codepoint: u16) -> Self {
        match codepoint {
            SELF_REMOVE_CODEPOINT => Self::SelfRemove,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use mdk_core::prelude::ProposalType;

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
