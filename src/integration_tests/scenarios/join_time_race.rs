use crate::integration_tests::{
    core::*,
    test_cases::{advanced_messaging::*, shared::*},
};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

/// Verifies that a message sent by the creator *immediately after* the group
/// invite (before the invitee processes the Welcome) is visible to the invitee
/// after the join-time flow completes.
///
/// This exercises the catch-up fetch introduced to replace the fixed `sleep`
/// that previously guarded the self-update.  Without the catch-up step a
/// message sent in the gap between "Welcome dispatched" and "subscription live"
/// would never arrive over the live subscription and the member would miss it,
/// causing an epoch-N message to be rejected once the self-update advances to
/// epoch N+1.
pub struct JoinTimeRaceScenario {
    context: ScenarioContext,
}

impl JoinTimeRaceScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for JoinTimeRaceScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        // Create two accounts: the creator who sends messages and the member
        // who is invited and joins later.
        CreateAccountsTestCase::with_names(vec!["jtr_creator", "jtr_member"])
            .execute(&mut self.context)
            .await?;

        // Creator creates a group and immediately sends a message before the
        // member has had a chance to process the Welcome.  In a real network
        // there is always some latency between the invite being dispatched and
        // the invitee's subscription becoming live — this reproduces that window.
        CreateGroupTestCase::basic()
            .with_name("jtr_group")
            .with_members("jtr_creator", vec!["jtr_member"])
            .execute(&mut self.context)
            .await?;

        // Send a message RIGHT after group creation, before the member joins.
        // The member has not yet processed the Welcome at this point.
        SendMessageTestCase::basic()
            .with_sender("jtr_creator")
            .with_group("jtr_group")
            .with_content("Message sent before member joined")
            .with_message_id_key("pre_join_message")
            .execute(&mut self.context)
            .await?;

        // Now wait for the member to process the Welcome and complete the
        // join-time flow (subscriptions + catch-up + self-update).
        WaitForWelcomeTestCase::for_account("jtr_member", "jtr_group")
            .execute(&mut self.context)
            .await?;

        // Verify the self-update completed (epoch advanced), confirming the
        // join-time flow ran to completion including the catch-up step.
        VerifySelfUpdateTestCase::for_account("jtr_member", "jtr_group")
            .execute(&mut self.context)
            .await?;

        // The creator sends another message after the member has fully joined.
        // This establishes a baseline: the member must see both messages.
        SendMessageTestCase::basic()
            .with_sender("jtr_creator")
            .with_group("jtr_group")
            .with_content("Message sent after member joined")
            .with_message_id_key("post_join_message")
            .execute(&mut self.context)
            .await?;

        // Verify the member can see both messages: the pre-join message
        // (delivered via catch-up fetch) and the post-join message (delivered
        // via the live subscription).
        AggregateMessagesTestCase::new("jtr_member", "jtr_group", 2)
            .execute(&mut self.context)
            .await?;

        Ok(())
    }
}
