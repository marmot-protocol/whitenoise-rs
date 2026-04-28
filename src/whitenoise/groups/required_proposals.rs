// Intentionally not re-exported via `src/mdk.rs` — `RequiredProposal` is the
// whitenoise-owned mirror; this import is internal only.
use std::collections::BTreeSet;

use mdk_core::prelude::ProposalType;
use nostr_sdk::prelude::PublicKey;
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

/// Pre-validates a list of resolved members against a group's required
/// proposal set, returning the first member whose advertised
/// [`KeyPackageCapabilities::proposals`] do not cover `required` (along with
/// the first missing proposal).
///
/// This powers `Whitenoise::add_members_to_group`'s per-member pre-check —
/// a fold over data already produced by `resolve_member_key_package`. The
/// helper is extracted so the routing logic (which selects between
/// [`crate::WhitenoiseError::KeyPackageMissingSelfRemove`] and
/// [`crate::WhitenoiseError::GroupRejectedMember`] based on the missing
/// proposal) stays trivially testable without spinning up a Whitenoise
/// instance.
///
/// Iteration order over `members` is preserved; iteration over each member's
/// missing proposals is `BTreeSet`-ordered (deterministic for tests).
/// Returns `None` when every member's proposals cover `required`.
pub(crate) fn find_member_missing_required_proposal(
    members: &[(PublicKey, KeyPackageCapabilities)],
    required: &BTreeSet<RequiredProposal>,
) -> Option<(PublicKey, RequiredProposal)> {
    for (pubkey, caps) in members {
        if let Some(missing) = required.difference(&caps.proposals).next() {
            return Some((*pubkey, *missing));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use mdk_core::prelude::ProposalType;
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

    fn caps_with(proposals: BTreeSet<RequiredProposal>) -> KeyPackageCapabilities {
        KeyPackageCapabilities {
            proposals,
            extensions: BTreeSet::new(),
        }
    }

    #[test]
    fn find_missing_returns_member_lacking_self_remove() {
        let legacy_pk = Keys::generate().public_key();
        let members = vec![(legacy_pk, caps_with(BTreeSet::new()))];
        let required = BTreeSet::from([RequiredProposal::SelfRemove]);

        assert_eq!(
            find_member_missing_required_proposal(&members, &required),
            Some((legacy_pk, RequiredProposal::SelfRemove)),
        );
    }

    #[test]
    fn find_missing_returns_none_when_all_members_cover_required() {
        let pk_a = Keys::generate().public_key();
        let pk_b = Keys::generate().public_key();
        let modern = caps_with(BTreeSet::from([RequiredProposal::SelfRemove]));
        let members = vec![(pk_a, modern.clone()), (pk_b, modern)];
        let required = BTreeSet::from([RequiredProposal::SelfRemove]);

        assert_eq!(
            find_member_missing_required_proposal(&members, &required),
            None,
        );
    }

    #[test]
    fn find_missing_returns_non_self_remove_proposal_when_thats_whats_missing() {
        // Synthetic capability shape: member advertises SelfRemove but the
        // group's required set demands an Unknown (future) proposal. The
        // helper must surface the missing proposal verbatim so the caller
        // routes it to `GroupRejectedMember` rather than the SelfRemove-
        // specific variant.
        let pk = Keys::generate().public_key();
        let members = vec![(
            pk,
            caps_with(BTreeSet::from([RequiredProposal::SelfRemove])),
        )];
        let required = BTreeSet::from([RequiredProposal::Unknown]);

        assert_eq!(
            find_member_missing_required_proposal(&members, &required),
            Some((pk, RequiredProposal::Unknown)),
        );
    }
}
