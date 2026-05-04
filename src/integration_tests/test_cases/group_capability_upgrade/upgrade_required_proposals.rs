use std::collections::BTreeSet;

use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::core::{ScenarioContext, TestCase};
use crate::whitenoise::groups::RequiredProposal;

use super::pubkeys_match_unordered;

pub struct UpgradeRequiredProposalsTestCase {
    admin_account_name: String,
    group_name: String,
    proposals_to_add: BTreeSet<RequiredProposal>,
    expected_result: ExpectedUpgradeResult,
}

enum ExpectedUpgradeResult {
    Success,
    Blocked {
        proposal: RequiredProposal,
        blocker_account_names: Vec<String>,
    },
}

impl UpgradeRequiredProposalsTestCase {
    pub fn new(
        admin_account_name: &str,
        group_name: &str,
        proposals_to_add: BTreeSet<RequiredProposal>,
    ) -> Self {
        Self {
            admin_account_name: admin_account_name.to_string(),
            group_name: group_name.to_string(),
            proposals_to_add,
            expected_result: ExpectedUpgradeResult::Success,
        }
    }

    pub fn expect_blocked(
        mut self,
        proposal: RequiredProposal,
        blocker_account_names: Vec<&str>,
    ) -> Self {
        self.expected_result = ExpectedUpgradeResult::Blocked {
            proposal,
            blocker_account_names: blocker_account_names
                .into_iter()
                .map(str::to_string)
                .collect(),
        };
        self
    }
}

#[async_trait]
impl TestCase for UpgradeRequiredProposalsTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let admin = context.get_account(&self.admin_account_name)?;
        let group_id = &context.get_group(&self.group_name)?.mls_group_id;
        let result = context
            .whitenoise
            .upgrade_group_required_proposals(admin, group_id, self.proposals_to_add.clone())
            .await;

        match &self.expected_result {
            ExpectedUpgradeResult::Success => result,
            ExpectedUpgradeResult::Blocked {
                proposal: expected_proposal,
                blocker_account_names,
            } => {
                let expected_blockers = blocker_account_names
                    .iter()
                    .map(|name| context.get_account(name).map(|account| account.pubkey))
                    .collect::<Result<Vec<_>, _>>()?;

                match result {
                    Err(WhitenoiseError::CapabilityUpgradeBlocked { proposal, blockers }) => {
                        if proposal == *expected_proposal
                            && pubkeys_match_unordered(&blockers, &expected_blockers)
                        {
                            Ok(())
                        } else {
                            Err(WhitenoiseError::Internal(format!(
                                "unexpected blocked upgrade payload: proposal={proposal:?}, blockers={blockers:?}, expected_blockers={expected_blockers:?}"
                            )))
                        }
                    }
                    Err(other) => Err(WhitenoiseError::Internal(format!(
                        "expected CapabilityUpgradeBlocked, got {other:?}"
                    ))),
                    Ok(()) => Err(WhitenoiseError::Internal(
                        "expected blocked required-proposals upgrade".to_string(),
                    )),
                }
            }
        }
    }
}
