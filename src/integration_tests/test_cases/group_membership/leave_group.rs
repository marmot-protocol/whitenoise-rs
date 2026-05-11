use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use mdk_core::prelude::GroupId;

/// Atomic test case for an account leaving a group via SelfRemove.
///
/// Wraps [`crate::Whitenoise::leave_group`] so scenarios can orchestrate
/// voluntary departures the same way they orchestrate admin-driven removals
/// via [`super::RemoveGroupMembersTestCase`].
pub struct LeaveGroupTestCase {
    account_name: String,
    group_id: GroupId,
}

impl LeaveGroupTestCase {
    pub fn new(account_name: &str, group_id: GroupId) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_id,
        }
    }
}

#[async_trait]
impl TestCase for LeaveGroupTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Account '{}' leaving group via SelfRemove",
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let session = context.whitenoise.require_session(&account.pubkey)?;
        session.groups().leave(&self.group_id).await?;

        tracing::info!("✓ '{}' published SelfRemove proposal", self.account_name);
        Ok(())
    }
}
