//! `upgrade_required_proposals_after_legacy_self_update` scenario.
//!
//! Covers the migration path for a group initially downgraded by a real
//! legacy LeafNode. Once the legacy member's modern client self-updates its
//! leaf, an admin can ratchet `RequiredCapabilities` back to SelfRemove.

use std::collections::BTreeSet;

use async_trait::async_trait;

use crate::integration_tests::{
    core::*,
    test_cases::{group_capability_upgrade::*, shared::*},
};
use crate::whitenoise::groups::{RequiredProposal, RequiredProposalUpgradability};
use crate::{Whitenoise, WhitenoiseError};

use std::sync::Arc;

pub struct UpgradeRequiredProposalsAfterLegacySelfUpdateScenario {
    context: ScenarioContext,
}

impl UpgradeRequiredProposalsAfterLegacySelfUpdateScenario {
    pub fn new(whitenoise: Arc<Whitenoise>) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for UpgradeRequiredProposalsAfterLegacySelfUpdateScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        CreateAccountsTestCase::with_names(vec!["upgrade_admin", "upgrade_modern_member"])
            .execute(&mut self.context)
            .await?;
        CreateLegacyPeerAccountTestCase::with_name("upgrade_legacy_peer")
            .execute(&mut self.context)
            .await?;

        let legacy_account = self.context.get_account("upgrade_legacy_peer")?.clone();
        let legacy_event_id =
            publish_legacy_leaf_key_package(&self.context, &legacy_account).await?;
        tracing::info!(
            target: "whitenoise::integration_tests::scenarios::upgrade_required_proposals_after_legacy_self_update",
            "Published legacy LeafNode KP {} for {}",
            legacy_event_id.to_hex(),
            legacy_account.pubkey.to_hex(),
        );

        CreateGroupTestCase::basic()
            .with_name("upgrade_required_group")
            .with_members(
                "upgrade_admin",
                vec!["upgrade_legacy_peer", "upgrade_modern_member"],
            )
            .execute(&mut self.context)
            .await?;

        VerifyRequiredProposalsTestCase::new(
            vec!["upgrade_admin"],
            "upgrade_required_group",
            BTreeSet::new(),
        )
        .execute(&mut self.context)
        .await?;

        VerifyRequiredProposalUpgradeStatusTestCase::new(
            "upgrade_admin",
            "upgrade_required_group",
            RequiredProposal::SelfRemove,
            RequiredProposalUpgradability::Blocked {
                blockers: vec![legacy_account.pubkey],
            },
        )
        .execute(&mut self.context)
        .await?;

        UpgradeRequiredProposalsTestCase::new(
            "upgrade_admin",
            "upgrade_required_group",
            BTreeSet::from([RequiredProposal::SelfRemove]),
        )
        .expect_blocked(RequiredProposal::SelfRemove, vec!["upgrade_legacy_peer"])
        .execute(&mut self.context)
        .await?;

        WaitForWelcomeTestCase::new(
            vec!["upgrade_legacy_peer", "upgrade_modern_member"],
            "upgrade_required_group",
        )
        .execute(&mut self.context)
        .await?;

        SelfUpdateGroupLeafTestCase::new("upgrade_legacy_peer", "upgrade_required_group")
            .execute(&mut self.context)
            .await?;

        VerifyRequiredProposalUpgradeStatusTestCase::new(
            "upgrade_admin",
            "upgrade_required_group",
            RequiredProposal::SelfRemove,
            RequiredProposalUpgradability::Available,
        )
        .execute(&mut self.context)
        .await?;

        UpgradeRequiredProposalsTestCase::new(
            "upgrade_admin",
            "upgrade_required_group",
            BTreeSet::from([RequiredProposal::SelfRemove]),
        )
        .execute(&mut self.context)
        .await?;

        VerifyRequiredProposalsTestCase::new(
            vec![
                "upgrade_admin",
                "upgrade_legacy_peer",
                "upgrade_modern_member",
            ],
            "upgrade_required_group",
            BTreeSet::from([RequiredProposal::SelfRemove]),
        )
        .execute(&mut self.context)
        .await?;

        UpgradeRequiredProposalsTestCase::new(
            "upgrade_admin",
            "upgrade_required_group",
            BTreeSet::from([RequiredProposal::SelfRemove]),
        )
        .execute(&mut self.context)
        .await?;

        Ok(())
    }
}
