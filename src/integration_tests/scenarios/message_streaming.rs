//! Integration test scenario for message streaming functionality.
//!
//! This scenario tests the full message streaming flow:
//! 1. Set up accounts and groups
//! 2. Send various messages (plain, with reactions, deleted)
//! 3. Subscribe and verify initial snapshot
//! 4. Verify real-time stream updates

use crate::integration_tests::{
    core::*,
    test_cases::{message_streaming::*, shared::*},
};
use crate::whitenoise::message_streaming::UpdateTrigger;
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

/// Scenario that tests message streaming functionality end-to-end.
pub struct MessageStreamingScenario {
    context: ScenarioContext,
}

impl MessageStreamingScenario {
    const GROUP_NAME: &'static str = "streaming_test_group";

    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }

    async fn phase1_setup(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 1: Setup accounts and group ===");

        // Create two accounts
        CreateAccountsTestCase::with_names(vec!["stream_creator", "stream_member"])
            .execute(&mut self.context)
            .await?;

        // Create the test group
        CreateGroupTestCase::basic()
            .with_name(Self::GROUP_NAME)
            .with_members("stream_creator", vec!["stream_member"])
            .execute(&mut self.context)
            .await?;

        // Note: MLS membership is auto-finalized when welcome is received,
        // so members can participate immediately without explicit accept

        Ok(())
    }

    async fn phase2_populate_messages(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 2: Populate initial messages ===");

        // Message 1: Plain message (no modifications)
        SendMessageTestCase::basic()
            .with_sender("stream_creator")
            .with_group(Self::GROUP_NAME)
            .with_message_id_key("msg_plain")
            .with_content("Hello, this is a plain message")
            .execute(&mut self.context)
            .await?;

        // Message 2: Message that will have a reaction
        SendMessageTestCase::basic()
            .with_sender("stream_member")
            .with_group(Self::GROUP_NAME)
            .with_message_id_key("msg_with_reaction")
            .with_content("React to this message")
            .execute(&mut self.context)
            .await?;

        // Add reaction to msg_with_reaction
        let msg_with_reaction_id = self.context.get_message_id("msg_with_reaction")?.clone();
        SendMessageTestCase::basic()
            .with_sender("stream_creator")
            .with_group(Self::GROUP_NAME)
            .into_reaction("👍", &msg_with_reaction_id)
            .with_message_id_key("reaction_permanent")
            .execute(&mut self.context)
            .await?;

        // Message 3: Message with reaction that gets deleted
        SendMessageTestCase::basic()
            .with_sender("stream_creator")
            .with_group(Self::GROUP_NAME)
            .with_message_id_key("msg_reaction_deleted")
            .with_content("This reaction will be deleted")
            .execute(&mut self.context)
            .await?;

        let msg_reaction_deleted_id = self.context.get_message_id("msg_reaction_deleted")?.clone();
        SendMessageTestCase::basic()
            .with_sender("stream_member")
            .with_group(Self::GROUP_NAME)
            .into_reaction("❌", &msg_reaction_deleted_id)
            .with_message_id_key("reaction_to_delete")
            .execute(&mut self.context)
            .await?;

        // Delete the reaction
        DeleteMessageTestCase::new("stream_member", Self::GROUP_NAME, "reaction_to_delete")
            .execute(&mut self.context)
            .await?;

        // Message 4: Message that gets deleted
        SendMessageTestCase::basic()
            .with_sender("stream_member")
            .with_group(Self::GROUP_NAME)
            .with_message_id_key("msg_deleted")
            .with_content("This message will be deleted")
            .execute(&mut self.context)
            .await?;

        DeleteMessageTestCase::new("stream_member", Self::GROUP_NAME, "msg_deleted")
            .execute(&mut self.context)
            .await?;

        // Wait for all messages to be processed and cached via relay round-trip
        // Messages need time to: publish to relay -> come back via subscription -> MLS process -> cache
        tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

        Ok(())
    }

    async fn phase3_verify_initial_snapshot(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 3: Verify initial message snapshot ===");

        VerifyInitialMessagesTestCase::new(Self::GROUP_NAME)
            .expect_messages(vec![
                "msg_plain",
                "msg_with_reaction",
                "msg_reaction_deleted",
                "msg_deleted",
            ])
            .expect_with_reactions(vec!["msg_with_reaction"])
            .expect_no_reactions(vec!["msg_plain", "msg_reaction_deleted"])
            .expect_deleted(vec!["msg_deleted"])
            .execute(&mut self.context)
            .await?;

        Ok(())
    }

    async fn phase4_verify_stream_updates(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 4: Verify real-time stream updates ===");

        // Test 1: NewMessage trigger
        tracing::info!("--- Testing NewMessage update ---");
        let new_msg_verifier =
            VerifyStreamUpdateTestCase::new(Self::GROUP_NAME, UpdateTrigger::NewMessage)
                .expect_message_key("msg_stream_new");
        new_msg_verifier.subscribe(&self.context).await?;

        SendMessageTestCase::basic()
            .with_sender("stream_creator")
            .with_group(Self::GROUP_NAME)
            .with_message_id_key("msg_stream_new")
            .with_content("New message via stream")
            .execute(&mut self.context)
            .await?;

        new_msg_verifier.execute(&mut self.context).await?;

        // Allow async processing to complete before next test
        // Increased delay to handle CI timing variations and MLS state propagation
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Test 2: ReactionAdded trigger
        tracing::info!("--- Testing ReactionAdded update ---");
        let reaction_verifier =
            VerifyStreamUpdateTestCase::new(Self::GROUP_NAME, UpdateTrigger::ReactionAdded)
                .expect_message_key("msg_stream_new")
                .expect_has_reactions(true);
        reaction_verifier.subscribe(&self.context).await?;

        let msg_stream_new_id = self.context.get_message_id("msg_stream_new")?.clone();
        SendMessageTestCase::basic()
            .with_sender("stream_member")
            .with_group(Self::GROUP_NAME)
            .into_reaction("🎉", &msg_stream_new_id)
            .with_message_id_key("reaction_stream")
            .execute(&mut self.context)
            .await?;

        reaction_verifier.execute(&mut self.context).await?;

        // Allow async processing to complete before next test
        // Increased delay to handle CI timing variations and MLS state propagation
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Test 3: ReactionRemoved trigger
        tracing::info!("--- Testing ReactionRemoved update ---");
        let reaction_removed_verifier =
            VerifyStreamUpdateTestCase::new(Self::GROUP_NAME, UpdateTrigger::ReactionRemoved)
                .expect_message_key("msg_stream_new")
                .expect_has_reactions(false);
        reaction_removed_verifier.subscribe(&self.context).await?;

        DeleteMessageTestCase::new("stream_member", Self::GROUP_NAME, "reaction_stream")
            .execute(&mut self.context)
            .await?;

        reaction_removed_verifier.execute(&mut self.context).await?;

        // Allow async processing to complete before next test
        // Increased delay to handle CI timing variations and MLS state propagation
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Test 4: MessageDeleted trigger
        tracing::info!("--- Testing MessageDeleted update ---");
        SendMessageTestCase::basic()
            .with_sender("stream_creator")
            .with_group(Self::GROUP_NAME)
            .with_message_id_key("msg_to_delete_stream")
            .with_content("Delete me via stream")
            .execute(&mut self.context)
            .await?;

        // Small delay to ensure the message is processed
        // Increased delay to handle CI timing variations
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let delete_verifier =
            VerifyStreamUpdateTestCase::new(Self::GROUP_NAME, UpdateTrigger::MessageDeleted)
                .expect_message_key("msg_to_delete_stream")
                .expect_deleted();
        delete_verifier.subscribe(&self.context).await?;

        DeleteMessageTestCase::new("stream_creator", Self::GROUP_NAME, "msg_to_delete_stream")
            .execute(&mut self.context)
            .await?;

        delete_verifier.execute(&mut self.context).await?;

        tracing::info!("✓ All stream update verifications passed");
        Ok(())
    }
}

#[async_trait]
impl Scenario for MessageStreamingScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("Starting MessageStreamingScenario");

        self.phase1_setup().await?;
        self.phase2_populate_messages().await?;
        self.phase3_verify_initial_snapshot().await?;
        self.phase4_verify_stream_updates().await?;

        tracing::info!("✓ MessageStreamingScenario completed successfully");
        Ok(())
    }
}
