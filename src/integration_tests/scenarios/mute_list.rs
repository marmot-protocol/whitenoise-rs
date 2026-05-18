use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nostr_sdk::Keys;

use crate::integration_tests::{
    core::*,
    test_cases::{mute_list::*, shared::*},
};
use crate::{Whitenoise, WhitenoiseError};

/// Brief settling delay between successive mute-list mutations so the relay
/// doesn't reject a republish with "replaced: have newer event" because two
/// kind-10000 events share a second-precision timestamp. Same shape as
/// `FollowManagementScenario`'s `CONTACT_LIST_SETTLE_DELAY`.
const MUTE_LIST_SETTLE_DELAY: Duration = Duration::from_secs(2);

pub struct MuteListScenario {
    context: ScenarioContext,
}

impl MuteListScenario {
    pub fn new(whitenoise: Arc<Whitenoise>) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for MuteListScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        // One account; generated target pubkeys (no need for full accounts —
        // mute-list entries are arbitrary pubkeys).
        CreateAccountsTestCase::with_names(vec!["mute_list_owner"])
            .execute(&mut self.context)
            .await?;

        let target_a = Keys::generate().public_key();
        let target_b = Keys::generate().public_key();

        // Start: no blocks.
        VerifyBlockedUsersTestCase::new("mute_list_owner", vec![])
            .execute(&mut self.context)
            .await?;

        // Block A → cache + publish.
        BlockUserTestCase::new("mute_list_owner", target_a)
            .execute(&mut self.context)
            .await?;
        VerifyBlockedUsersTestCase::new("mute_list_owner", vec![target_a])
            .execute(&mut self.context)
            .await?;

        tokio::time::sleep(MUTE_LIST_SETTLE_DELAY).await;

        // Block A again → fast-path idempotency: returns Ok without
        // republishing. Cache is unchanged.
        BlockUserTestCase::new("mute_list_owner", target_a)
            .execute(&mut self.context)
            .await?;
        VerifyBlockedUsersTestCase::new("mute_list_owner", vec![target_a])
            .execute(&mut self.context)
            .await?;

        // Block B → cache + publish (republish includes both entries).
        BlockUserTestCase::new("mute_list_owner", target_b)
            .execute(&mut self.context)
            .await?;
        VerifyBlockedUsersTestCase::new("mute_list_owner", vec![target_a, target_b])
            .execute(&mut self.context)
            .await?;

        tokio::time::sleep(MUTE_LIST_SETTLE_DELAY).await;

        // Unblock A → cache delete + republish.
        UnblockUserTestCase::new("mute_list_owner", target_a)
            .execute(&mut self.context)
            .await?;
        VerifyBlockedUsersTestCase::new("mute_list_owner", vec![target_b])
            .execute(&mut self.context)
            .await?;

        tokio::time::sleep(MUTE_LIST_SETTLE_DELAY).await;

        // Unblock A again → not-blocked early-return; no error.
        UnblockUserTestCase::new("mute_list_owner", target_a)
            .execute(&mut self.context)
            .await?;
        VerifyBlockedUsersTestCase::new("mute_list_owner", vec![target_b])
            .execute(&mut self.context)
            .await?;

        // Unblock B → empty mute list.
        UnblockUserTestCase::new("mute_list_owner", target_b)
            .execute(&mut self.context)
            .await?;
        VerifyBlockedUsersTestCase::new("mute_list_owner", vec![])
            .execute(&mut self.context)
            .await?;

        tracing::info!("✓ Mute-list scenario completed with:");
        tracing::info!("  • Empty-state read");
        tracing::info!("  • Block A → publish + cache");
        tracing::info!("  • Block A again → fast-path idempotency");
        tracing::info!("  • Block B → republish with both");
        tracing::info!("  • Unblock A → republish with B only");
        tracing::info!("  • Unblock A again → already-unblocked early-return");
        tracing::info!("  • Unblock B → empty mute list");

        Ok(())
    }
}
