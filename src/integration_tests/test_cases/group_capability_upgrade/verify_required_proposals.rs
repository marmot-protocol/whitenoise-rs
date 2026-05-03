use std::collections::BTreeSet;

use async_trait::async_trait;

use crate::WhitenoiseError;
use crate::integration_tests::core::{ScenarioContext, TestCase, retry_default};
use crate::whitenoise::groups::RequiredProposal;

pub struct VerifyRequiredProposalsTestCase {
    account_names: Vec<String>,
    group_name: String,
    expected: BTreeSet<RequiredProposal>,
}

impl VerifyRequiredProposalsTestCase {
    pub fn new(
        account_names: Vec<&str>,
        group_name: &str,
        expected: BTreeSet<RequiredProposal>,
    ) -> Self {
        Self {
            account_names: account_names.into_iter().map(str::to_string).collect(),
            group_name: group_name.to_string(),
            expected,
        }
    }
}

#[async_trait]
impl TestCase for VerifyRequiredProposalsTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let group_id = context.get_group(&self.group_name)?.mls_group_id.clone();
        let wn = context.whitenoise;

        for account_name in &self.account_names {
            let account = context.get_account(account_name)?.clone();
            let account_name = account_name.clone();
            let expected = self.expected.clone();

            retry_default(
                || {
                    let account = account.clone();
                    let group_id = group_id.clone();
                    let expected = expected.clone();
                    let account_name = account_name.clone();

                    async move {
                        let required = wn.group_required_proposals(&account, &group_id).await?;
                        if required == expected {
                            Ok(())
                        } else {
                            Err(WhitenoiseError::Internal(format!(
                                "{account_name} required proposals were {required:?}, expected {expected:?}"
                            )))
                        }
                    }
                },
                &format!("{account_name} required proposals converge"),
            )
            .await?;
        }

        Ok(())
    }
}
