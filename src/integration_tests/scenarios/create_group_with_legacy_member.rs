//! `create_group_with_legacy_member` scenario.
//!
//! Verifies the Phase 1 unblock from the mixed-version-groups plan: a peer
//! whose published Nostr key-package event omits the SelfRemove proposal
//! advertisement (legacy capability profile) can be invited to a fresh group
//! without preflight rejection on the WhiteNoise side.
//!
//! **Scope.** The fixture rewrites only the Nostr-event capability tags; the
//! underlying MLS LeafNode bytes (built by MDK) still advertise the full
//! capability set, so MDK's LCD will not actually downgrade the resulting
//! group's `RequiredCapabilities`. The assertion is therefore limited to the
//! unblock — `create_group` returns `Ok` and the legacy member appears in the
//! resulting group's member list. End-to-end LCD verification needs real
//! legacy LeafNode bytes (recorded from an older MDK) and is out of scope for
//! this plan.

use std::collections::BTreeSet;

use async_trait::async_trait;
use nostr_sdk::PublicKey;

use crate::integration_tests::{core::*, test_cases::shared::*};
use crate::whitenoise::groups::{KeyPackageCapabilities, MlsExtensionId};
use crate::{Whitenoise, WhitenoiseError};

pub struct CreateGroupWithLegacyMemberScenario {
    context: ScenarioContext,
}

impl CreateGroupWithLegacyMemberScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for CreateGroupWithLegacyMemberScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        // A creates the group, B is the "legacy" peer.
        CreateAccountsTestCase::with_names(vec!["legacy_creator", "legacy_peer"])
            .execute(&mut self.context)
            .await?;

        let peer_account = self.context.get_account("legacy_peer")?.clone();
        let peer_pubkey: PublicKey = peer_account.pubkey;

        let peer_relays = peer_account
            .key_package_relays(self.context.whitenoise)
            .await?;
        if peer_relays.is_empty() {
            return Err(WhitenoiseError::Internal(
                "legacy_peer has no key-package relays configured".to_string(),
            ));
        }

        // Publish a hand-crafted KP for B that advertises **no** SelfRemove
        // proposal at the Nostr-event level — only the NostrGroupData identity
        // extension required by the baseline validator.
        let legacy_caps = KeyPackageCapabilities {
            proposals: BTreeSet::new(),
            extensions: [MlsExtensionId::NostrGroupData].into_iter().collect(),
        };
        let legacy_event_id = publish_legacy_capability_key_package(
            &self.context,
            &peer_account,
            &peer_relays,
            legacy_caps,
        )
        .await?;
        tracing::info!(
            "Published legacy-capability KP {} for peer {}",
            legacy_event_id.to_hex(),
            peer_pubkey.to_hex(),
        );

        // Create the group as A inviting B. Phase 1's exit criterion: this
        // call must succeed (no `KeyPackageMissingSelfRemove` /
        // `IncompatibleKeyPackage` rejection from preflight).
        CreateGroupTestCase::basic()
            .with_name("legacy_member_group")
            .with_members("legacy_creator", vec!["legacy_peer"])
            .execute(&mut self.context)
            .await?;

        // Confirm B appears in the resulting group's member list.
        let creator_account = self.context.get_account("legacy_creator")?.clone();
        let group = self.context.get_group("legacy_member_group")?;
        let members = self
            .context
            .whitenoise
            .group_members(&creator_account, &group.mls_group_id)
            .await?;

        if !members.contains(&peer_pubkey) {
            return Err(WhitenoiseError::Internal(format!(
                "legacy peer {} not present in group members {:?}",
                peer_pubkey.to_hex(),
                members.iter().map(|pk| pk.to_hex()).collect::<Vec<_>>()
            )));
        }

        // NOTE: We intentionally do not assert on `group_required_proposals`
        // here. MDK's LCD reads from MLS LeafNode bytes, and the fixture only
        // rewrites Nostr-event tags. End-to-end LCD verification requires real
        // legacy LeafNode bytes and is tracked as a follow-up.

        tracing::info!(
            "✓ create_group with legacy peer succeeded; peer {} present in {} member(s)",
            peer_pubkey.to_hex(),
            members.len(),
        );

        Ok(())
    }
}
