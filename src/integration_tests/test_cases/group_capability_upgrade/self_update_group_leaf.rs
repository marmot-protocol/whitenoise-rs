use async_trait::async_trait;

use crate::integration_tests::core::{ScenarioContext, TestCase};
use crate::{Whitenoise, WhitenoiseError};

pub struct SelfUpdateGroupLeafTestCase {
    account_name: String,
    group_name: String,
}

impl SelfUpdateGroupLeafTestCase {
    pub fn new(account_name: &str, group_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
        }
    }
}

#[async_trait]
impl TestCase for SelfUpdateGroupLeafTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let account = context.get_account(&self.account_name)?.clone();
        let group_id = context.get_group(&self.group_name)?.mls_group_id.clone();

        let (relay_urls, evolution_event) = {
            let mdk = context.whitenoise.create_mdk_for_account(account.pubkey)?;
            let relay_urls = Whitenoise::ensure_group_relays(&mdk, &group_id)?;
            let update_result = mdk.self_update(&group_id)?;

            (relay_urls, update_result.evolution_event)
        };

        context
            .whitenoise
            .publish_and_merge_commit(evolution_event, &account.pubkey, &group_id, &relay_urls)
            .await
    }
}
