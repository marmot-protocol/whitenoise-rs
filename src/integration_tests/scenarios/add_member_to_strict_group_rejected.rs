//! `add_member_to_strict_group_rejected` scenario.
//!
//! When a group is created with all-modern initial members, MDK's
//! lowest-common-denominator logic computes `RequiredProposals = {SelfRemove}`
//! for the group. `add_members_to_group` must then reject a subsequent
//! invitation of a peer whose published key package does not advertise
//! SelfRemove, returning the typed
//! [`crate::WhitenoiseError::KeyPackageMissingSelfRemove`] variant attributed
//! to the offending member.
//!
//! **Scope.** The legacy fixture rewrites only the Nostr-event capability
//! tags; the underlying MLS LeafNode bytes (built by MDK) still advertise the
//! full capability set. That doesn't affect this test's assertion path —
//! `add_members_to_group`'s pre-check reads from the projected
//! `KeyPackageCapabilities` (Nostr-tag level) and fires before MDK ever sees
//! the LeafNode bytes. The MDK defense-in-depth fallback is unreachable from
//! this fixture by construction and is covered by the unit test
//! `map_invitee_missing_required_proposal_yields_group_rejected_member`.

use std::collections::BTreeSet;

use async_trait::async_trait;
use nostr_sdk::PublicKey;

use crate::integration_tests::{core::*, test_cases::shared::*};
use crate::whitenoise::groups::{KeyPackageCapabilities, MlsExtensionId, RequiredProposal};
use crate::{Whitenoise, WhitenoiseError};

pub struct AddMemberToStrictGroupRejectedScenario {
    context: ScenarioContext,
}

impl AddMemberToStrictGroupRejectedScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for AddMemberToStrictGroupRejectedScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        // Three accounts: A creates the group with B (both modern, so LCD
        // fires `{SelfRemove}`); C is the legacy peer A later tries to invite.
        // C is constructed via the no-initial-KP path so the legacy fixture
        // below is the *only* publisher of C's key packages — no
        // auto-published modern KP to shadow the legacy variant.
        CreateAccountsTestCase::with_names(vec!["strict_creator", "strict_modern_peer"])
            .execute(&mut self.context)
            .await?;
        CreateLegacyPeerAccountTestCase::with_name("strict_legacy_peer")
            .execute(&mut self.context)
            .await?;

        // Create the group with A + B. Both publish modern key packages via
        // the standard scheduler path, so MDK's LCD computes
        // `RequiredProposals = {SelfRemove}`.
        CreateGroupTestCase::basic()
            .with_name("strict_group")
            .with_members("strict_creator", vec!["strict_modern_peer"])
            .execute(&mut self.context)
            .await?;

        let creator_account = self.context.get_account("strict_creator")?.clone();
        let group_id = self.context.get_group("strict_group")?.mls_group_id.clone();

        // Sanity: the group's required proposals must contain SelfRemove —
        // otherwise this test is asserting nothing meaningful and we should
        // know about the LCD mismatch immediately rather than ship a green-
        // but-wrong test.
        let required = self
            .context
            .whitenoise
            .group_required_proposals(&creator_account, &group_id)
            .await?;
        if !required.contains(&RequiredProposal::SelfRemove) {
            return Err(WhitenoiseError::Internal(format!(
                "precondition failed: strict group's required proposals \
                 should contain SelfRemove, got {required:?}"
            )));
        }

        // Now publish a legacy-capability key package for C and try to invite
        // C into the strict group.
        let legacy_account = self.context.get_account("strict_legacy_peer")?.clone();
        let legacy_pubkey: PublicKey = legacy_account.pubkey;

        let legacy_relays = legacy_account
            .key_package_relays(self.context.whitenoise)
            .await?;
        if legacy_relays.is_empty() {
            return Err(WhitenoiseError::Internal(
                "strict_legacy_peer has no key-package relays configured".to_string(),
            ));
        }

        let legacy_caps = KeyPackageCapabilities {
            proposals: BTreeSet::new(),
            extensions: [MlsExtensionId::NostrGroupData].into_iter().collect(),
        };
        let legacy_event_id = publish_legacy_capability_key_package(
            &self.context,
            &legacy_account,
            &legacy_relays,
            legacy_caps,
        )
        .await?;
        tracing::info!(
            "Published legacy-capability KP {} for legacy peer {}",
            legacy_event_id.to_hex(),
            legacy_pubkey.to_hex(),
        );

        let result = self
            .context
            .whitenoise
            .add_members_to_group(&creator_account, &group_id, vec![legacy_pubkey])
            .await;

        match result {
            Err(WhitenoiseError::KeyPackageMissingSelfRemove { member_pubkey }) => {
                if member_pubkey != legacy_pubkey {
                    return Err(WhitenoiseError::Internal(format!(
                        "expected KeyPackageMissingSelfRemove for {}, got attribution to {}",
                        legacy_pubkey.to_hex(),
                        member_pubkey.to_hex()
                    )));
                }
                tracing::info!(
                    "Pre-check rejected legacy peer {} as expected",
                    member_pubkey.to_hex()
                );
                Ok(())
            }
            Err(other) => Err(WhitenoiseError::Internal(format!(
                "expected KeyPackageMissingSelfRemove, got: {other:?}"
            ))),
            Ok(()) => Err(WhitenoiseError::Internal(
                "expected add_members_to_group to fail for legacy peer; \
                 got Ok(())"
                    .to_string(),
            )),
        }
    }
}
