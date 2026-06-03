use std::sync::Arc;

use crate::integration_tests::{
    core::*,
    test_cases::{group_membership::*, group_state_streaming::*, shared::*},
};
use crate::marmot::GroupId;
use crate::whitenoise::group_state_streaming::GroupStateUpdate;
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

/// End-to-end coverage for [`crate::whitenoise::group_state_streaming`].
///
/// Verifies the three contracts that the unit tests can't cover from end to end:
///   1. Voluntary departure (SelfRemove) emits `LeftGroup` to the leaver's stream.
///   2. Admin removal emits `RemovedFromGroup` to the removed account's stream.
///   3. Streams are isolated per `(account, group)` — an event triggered by one
///      account never reaches another account's subscriber on the same group.
///
/// Each phase uses its own MLS group so the membership-changing flows can't
/// race against each other. Two destructive proposals against the same group in
/// quick succession can produce unprocessable MLS messages before any state
/// event can fire for the affected member.
pub struct GroupStateStreamingScenario {
    context: ScenarioContext,
}

impl GroupStateStreamingScenario {
    const ADMIN: &'static str = "gss_admin";
    const LEAVER: &'static str = "gss_leaver";
    const KICKED: &'static str = "gss_kicked";
    const ISOLATED: &'static str = "gss_isolated";

    const LEAVE_GROUP: &'static str = "gss_leave_group";
    const KICK_GROUP: &'static str = "gss_kick_group";
    const ISOLATION_GROUP: &'static str = "gss_isolation_group";

    pub fn new(whitenoise: Arc<Whitenoise>) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }

    fn group_id(&self, name: &str) -> Result<GroupId, WhitenoiseError> {
        Ok(self.context.get_group(name)?.mls_group_id.clone())
    }

    async fn phase1_setup(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 1: Setup accounts and per-phase groups ===");

        CreateAccountsTestCase::with_names(vec![
            Self::ADMIN,
            Self::LEAVER,
            Self::KICKED,
            Self::ISOLATED,
        ])
        .execute(&mut self.context)
        .await?;

        // Each phase exercises a destructive flow against its own group so the
        // membership changes can't race against each other.
        for (group_name, member) in [
            (Self::LEAVE_GROUP, Self::LEAVER),
            (Self::KICK_GROUP, Self::KICKED),
            (Self::ISOLATION_GROUP, Self::ISOLATED),
        ] {
            CreateGroupTestCase::basic()
                .with_name(group_name)
                .with_members(Self::ADMIN, vec![member])
                .execute(&mut self.context)
                .await?;

            WaitForWelcomeTestCase::for_account(member, group_name)
                .execute(&mut self.context)
                .await?;
        }

        Ok(())
    }

    async fn phase2_self_remove_emits_left_group(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 2: SelfRemove emits LeftGroup ===");

        let verifier = VerifyGroupStateUpdateTestCase::new(
            Self::LEAVER,
            Self::LEAVE_GROUP,
            GroupStateUpdate::LeftGroup,
        );
        verifier.subscribe(&self.context).await?;

        LeaveGroupTestCase::new(Self::LEAVER, self.group_id(Self::LEAVE_GROUP)?)
            .execute(&mut self.context)
            .await?;

        verifier.execute(&mut self.context).await?;
        Ok(())
    }

    async fn phase3_admin_removal_emits_removed_from_group(
        &mut self,
    ) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 3: Admin removal emits RemovedFromGroup ===");

        let verifier = VerifyGroupStateUpdateTestCase::new(
            Self::KICKED,
            Self::KICK_GROUP,
            GroupStateUpdate::RemovedFromGroup,
        );
        verifier.subscribe(&self.context).await?;

        let kicked_pubkey = self.context.get_account(Self::KICKED)?.pubkey;
        RemoveGroupMembersTestCase::new(
            Self::ADMIN,
            self.group_id(Self::KICK_GROUP)?,
            vec![kicked_pubkey],
        )
        .execute(&mut self.context)
        .await?;

        verifier.execute(&mut self.context).await?;
        Ok(())
    }

    async fn phase4_streams_isolated_per_account(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 4: Per-(account, group) stream isolation ===");

        // Subscribe both before triggering the action.
        let removed_verifier = VerifyGroupStateUpdateTestCase::new(
            Self::ISOLATED,
            Self::ISOLATION_GROUP,
            GroupStateUpdate::RemovedFromGroup,
        );
        let admin_silent =
            VerifyNoGroupStateUpdateTestCase::new(Self::ADMIN, Self::ISOLATION_GROUP);
        removed_verifier.subscribe(&self.context).await?;
        admin_silent.subscribe(&self.context).await?;

        let isolated_pubkey = self.context.get_account(Self::ISOLATED)?.pubkey;
        RemoveGroupMembersTestCase::new(
            Self::ADMIN,
            self.group_id(Self::ISOLATION_GROUP)?,
            vec![isolated_pubkey],
        )
        .execute(&mut self.context)
        .await?;

        // Order matters: await the positive event first to synchronise on MLS
        // propagation, then assert the admin's stream remained silent.
        removed_verifier.execute(&mut self.context).await?;
        admin_silent.execute(&mut self.context).await?;

        Ok(())
    }
}

#[async_trait]
impl Scenario for GroupStateStreamingScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("Starting GroupStateStreamingScenario");

        self.phase1_setup().await?;
        self.phase2_self_remove_emits_left_group().await?;
        self.phase3_admin_removal_emits_removed_from_group().await?;
        self.phase4_streams_isolated_per_account().await?;

        tracing::info!("✓ GroupStateStreamingScenario completed successfully");
        Ok(())
    }
}
