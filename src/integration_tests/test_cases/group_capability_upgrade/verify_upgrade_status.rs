use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::core::{ScenarioContext, TestCase, retry_default};
use crate::whitenoise::groups::{RequiredProposal, RequiredProposalUpgradability};

use super::pubkeys_match_unordered;

pub struct VerifyRequiredProposalUpgradeStatusTestCase {
    account_name: String,
    group_name: String,
    proposal: RequiredProposal,
    expected: RequiredProposalUpgradability,
}

impl VerifyRequiredProposalUpgradeStatusTestCase {
    pub fn new(
        account_name: &str,
        group_name: &str,
        proposal: RequiredProposal,
        expected: RequiredProposalUpgradability,
    ) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
            proposal,
            expected,
        }
    }
}

#[async_trait]
impl TestCase for VerifyRequiredProposalUpgradeStatusTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let account = context.get_account(&self.account_name)?.clone();
        let group_id = context.get_group(&self.group_name)?.mls_group_id.clone();
        let wn = context.whitenoise.clone();
        let account_name = self.account_name.clone();
        let proposal = self.proposal;
        let expected = self.expected.clone();

        retry_default(
            || {
                let account = account.clone();
                let group_id = group_id.clone();
                let account_name = account_name.clone();
                let expected = expected.clone();
                let wn = wn.clone();

                async move {
                    let status = wn
                        .group_capability_upgrade_status(&account, &group_id)
                        .await?;
                    let actual = status
                        .per_proposal
                        .into_iter()
                        .find(|row| row.proposal == proposal)
                        .map(|row| row.state)
                        .ok_or_else(|| {
                            WhitenoiseError::Internal(format!(
                                "{proposal:?} missing from capability upgrade status"
                            ))
                        })?;

                    if upgradability_matches(&actual, &expected) {
                        Ok(())
                    } else {
                        Err(WhitenoiseError::Internal(format!(
                            "{account_name} {proposal:?} upgrade status was {actual:?}, expected {expected:?}"
                        )))
                    }
                }
            },
            &format!("{account_name} {proposal:?} upgrade status"),
        )
        .await
    }
}

fn upgradability_matches(
    actual: &RequiredProposalUpgradability,
    expected: &RequiredProposalUpgradability,
) -> bool {
    match (actual, expected) {
        (
            RequiredProposalUpgradability::Blocked {
                blockers: actual_blockers,
            },
            RequiredProposalUpgradability::Blocked {
                blockers: expected_blockers,
            },
        ) => pubkeys_match_unordered(actual_blockers, expected_blockers),
        _ => actual == expected,
    }
}
