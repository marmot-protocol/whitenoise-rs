use crate::integration_tests::{
    core::*,
    test_cases::{advanced_messaging::*, shared::*},
};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

pub struct AdvancedMessagingScenario {
    context: ScenarioContext,
}

impl AdvancedMessagingScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for AdvancedMessagingScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        // Create accounts for advanced messaging tests
        CreateAccountsTestCase::with_names(vec!["adv_msg_sender", "adv_msg_reactor"])
            .execute(&mut self.context)
            .await?;

        // Create test group
        CreateGroupTestCase::basic()
            .with_name("advanced_messaging_group")
            .with_members("adv_msg_sender", vec!["adv_msg_reactor"])
            .execute(&mut self.context)
            .await?;

        // Note: MLS membership is auto-finalized when welcome is received,
        // so members can participate immediately without explicit accept

        // Send initial message that will receive reactions
        SendMessageTestCase::basic()
            .with_sender("adv_msg_sender")
            .with_group("advanced_messaging_group")
            .with_message_id_key("adv_msg_initial")
            .execute(&mut self.context)
            .await?;

        let basic_message_id = self.context.get_message_id("adv_msg_initial")?.clone();

        // Send a message that will be replied to
        SendMessageTestCase::basic()
            .with_sender("adv_msg_sender")
            .with_group("advanced_messaging_group")
            .with_content("This message will receive replies")
            .with_message_id_key("reply_target_message")
            .execute(&mut self.context)
            .await?;

        let reply_target_id = self.context.get_message_id("reply_target_message")?.clone();

        // Send a message that will be deleted
        SendMessageTestCase::basic()
            .with_sender("adv_msg_sender")
            .with_group("advanced_messaging_group")
            .with_content("This message will be deleted!")
            .with_message_id_key("to_delete_message")
            .execute(&mut self.context)
            .await?;

        // First, let reactor send a simple message to ensure group access
        tracing::info!("Testing reactor's group access with a simple message...");
        SendMessageTestCase::basic()
            .with_sender("adv_msg_reactor")
            .with_group("advanced_messaging_group")
            .with_content("Testing group access")
            .with_message_id_key("reactor_test_message")
            .execute(&mut self.context)
            .await?;

        // Now send reactions to different messages
        // Track this reaction's ID so we can delete it later
        SendMessageTestCase::basic()
            .with_sender("adv_msg_reactor")
            .with_group("advanced_messaging_group")
            .into_reaction("👍", &basic_message_id)
            .with_message_id_key("thumbs_up_reaction")
            .execute(&mut self.context)
            .await?;

        SendMessageTestCase::basic()
            .with_sender("adv_msg_reactor")
            .with_group("advanced_messaging_group")
            .into_reaction("🎉", &reply_target_id)
            .execute(&mut self.context)
            .await?;

        // Send a reply message
        SendMessageTestCase::basic()
            .with_sender("adv_msg_reactor")
            .with_group("advanced_messaging_group")
            .into_reply("Great message, I agree!", &reply_target_id)
            .execute(&mut self.context)
            .await?;

        // Delete the message we marked for deletion
        DeleteMessageTestCase::new(
            "adv_msg_sender",
            "advanced_messaging_group",
            "to_delete_message",
        )
        .execute(&mut self.context)
        .await?;

        // Send one more message after all interactions
        SendMessageTestCase::basic()
            .with_sender("adv_msg_sender")
            .with_group("advanced_messaging_group")
            .with_content("Final message after all interactions!")
            .with_message_id_key("final_message")
            .execute(&mut self.context)
            .await?;

        // Test message aggregation with all the complex interactions
        AggregateMessagesTestCase::new("adv_msg_sender", "advanced_messaging_group", 5) // Expect at least 5 messages
            .execute(&mut self.context)
            .await?;

        // ============================================================
        // Test: Reaction Deletion
        // Delete the 👍 reaction we sent earlier to adv_msg_initial,
        // verify the reaction no longer appears in aggregated messages
        // ============================================================
        tracing::info!("Testing reaction deletion flow...");

        // Delete the thumbs up reaction we sent earlier
        DeleteMessageTestCase::new(
            "adv_msg_reactor",
            "advanced_messaging_group",
            "thumbs_up_reaction",
        )
        .execute(&mut self.context)
        .await?;

        // Verify the reaction is no longer present on the target message
        VerifyReactionDeletionTestCase::new(
            "adv_msg_sender",
            "advanced_messaging_group",
            "adv_msg_initial",
            "adv_msg_reactor",
        )
        .execute(&mut self.context)
        .await?;

        tracing::info!("✓ Advanced messaging scenario completed with:");
        tracing::info!("  • Multiple chat messages");
        tracing::info!("  • Reactions with proper targeting");
        tracing::info!("  • Reply messages with e-tag targeting");
        tracing::info!("  • Message deletion with verification");
        tracing::info!("  • Reaction deletion with verification");
        tracing::info!("  • Message aggregation with complex relationships");

        Ok(())
    }
}
