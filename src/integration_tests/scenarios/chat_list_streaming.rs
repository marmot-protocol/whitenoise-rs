use crate::integration_tests::{
    core::*,
    test_cases::{chat_list_streaming::*, shared::*},
};
use crate::whitenoise::chat_list_streaming::ChatListUpdateTrigger;
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

/// Scenario that tests chat list streaming functionality end-to-end.
pub struct ChatListStreamingScenario {
    context: ScenarioContext,
}

impl ChatListStreamingScenario {
    const GROUP_NAME: &'static str = "chat_list_stream_group";
    const INACTIVE_GROUP_NAME: &'static str = "inactive_group";

    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }

    async fn phase1_setup(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 1: Setup accounts ===");

        CreateAccountsTestCase::with_names(vec!["stream_creator", "stream_member"])
            .execute(&mut self.context)
            .await?;

        Ok(())
    }

    async fn phase2_verify_initial_subscription(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 2: Verify initial subscription returns empty list ===");

        VerifySubscriptionInitialItemsTestCase::expect_empty("stream_creator")
            .execute(&mut self.context)
            .await?;

        Ok(())
    }

    async fn phase3_test_new_group(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 3: Test NewGroup update ===");

        // Subscribe to chat list updates for the member BEFORE creating the group
        let new_group_verifier =
            VerifyChatListUpdateTestCase::new("stream_member", ChatListUpdateTrigger::NewGroup)
                .expect_group_name(Self::GROUP_NAME)
                .expect_no_last_message();
        new_group_verifier.subscribe(&self.context).await?;

        // Create the group - this will send a welcome to stream_member
        CreateGroupTestCase::basic()
            .with_name(Self::GROUP_NAME)
            .with_members("stream_creator", vec!["stream_member"])
            .execute(&mut self.context)
            .await?;

        // Wait for welcome to be processed (auto-finalization)
        WaitForWelcomeTestCase::for_account("stream_member", Self::GROUP_NAME)
            .execute(&mut self.context)
            .await?;

        // Verify the NewGroup update was received
        new_group_verifier.execute(&mut self.context).await?;

        tracing::info!("✓ NewGroup update received after welcome finalization");
        Ok(())
    }

    async fn phase4_test_new_last_message(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 4: Test NewLastMessage update ===");

        // Subscribe to chat list updates BEFORE sending the message
        let new_msg_verifier = VerifyChatListUpdateTestCase::new(
            "stream_creator",
            ChatListUpdateTrigger::NewLastMessage,
        )
        .expect_group_name(Self::GROUP_NAME)
        .expect_last_message_content("Hello from streaming test!");
        new_msg_verifier.subscribe(&self.context).await?;

        // Send a message
        SendMessageTestCase::basic()
            .with_sender("stream_creator")
            .with_group(Self::GROUP_NAME)
            .with_message_id_key("msg_stream_1")
            .with_content("Hello from streaming test!")
            .execute(&mut self.context)
            .await?;

        // Verify the NewLastMessage update was received
        new_msg_verifier.execute(&mut self.context).await?;

        tracing::info!("✓ NewLastMessage update received after sending message");
        Ok(())
    }

    async fn phase5_test_last_message_deleted(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 5: Test LastMessageDeleted update ===");

        // First send another message that will become the new "last message" after deletion
        SendMessageTestCase::basic()
            .with_sender("stream_creator")
            .with_group(Self::GROUP_NAME)
            .with_message_id_key("msg_stream_2")
            .with_content("This will be the new last message")
            .execute(&mut self.context)
            .await?;

        // Wait for the message to be processed
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Send the message that we'll delete
        SendMessageTestCase::basic()
            .with_sender("stream_creator")
            .with_group(Self::GROUP_NAME)
            .with_message_id_key("msg_to_delete")
            .with_content("This message will be deleted")
            .execute(&mut self.context)
            .await?;

        // Wait for the message to be processed and become the last message
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Subscribe to chat list updates BEFORE deleting
        let delete_verifier = VerifyChatListUpdateTestCase::new(
            "stream_creator",
            ChatListUpdateTrigger::LastMessageDeleted,
        )
        .expect_group_name(Self::GROUP_NAME)
        .expect_last_message_content("This will be the new last message");
        delete_verifier.subscribe(&self.context).await?;

        // Delete the last message
        DeleteMessageTestCase::new("stream_creator", Self::GROUP_NAME, "msg_to_delete")
            .execute(&mut self.context)
            .await?;

        // Verify the LastMessageDeleted update was received with the new last message
        delete_verifier.execute(&mut self.context).await?;

        tracing::info!("✓ LastMessageDeleted update received with correct new last message");
        Ok(())
    }

    async fn phase6_verify_subscription_ordering(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 6: Verify subscription returns items in correct order ===");

        // Create a second group (will have created_at = now, but no messages)
        CreateGroupTestCase::basic()
            .with_name(Self::INACTIVE_GROUP_NAME)
            .with_members("stream_creator", vec!["stream_member"])
            .execute(&mut self.context)
            .await?;

        // Pause to ensure timestamp separation (Nostr timestamps are in seconds)
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // Send a new message to the original group to ensure it has the most recent activity
        SendMessageTestCase::basic()
            .with_sender("stream_creator")
            .with_group(Self::GROUP_NAME)
            .with_message_id_key("msg_stream_final")
            .with_content("Final message for ordering test")
            .execute(&mut self.context)
            .await?;

        // Wait for message to be processed
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // Verify subscription returns groups in correct order:
        // 1. GROUP_NAME (has most recent message)
        // 2. INACTIVE_GROUP_NAME (just created_at, no messages)
        VerifySubscriptionInitialItemsTestCase::expect_groups_in_order(
            "stream_creator",
            vec![Self::GROUP_NAME, Self::INACTIVE_GROUP_NAME],
        )
        .execute(&mut self.context)
        .await?;

        tracing::info!(
            "✓ Subscription returns items in correct order (most recent activity first)"
        );
        Ok(())
    }
}

#[async_trait]
impl Scenario for ChatListStreamingScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("Starting ChatListStreamingScenario");

        self.phase1_setup().await?;
        self.phase2_verify_initial_subscription().await?;
        self.phase3_test_new_group().await?;
        self.phase4_test_new_last_message().await?;
        self.phase5_test_last_message_deleted().await?;
        self.phase6_verify_subscription_ordering().await?;

        tracing::info!("✓ ChatListStreamingScenario completed successfully");
        Ok(())
    }
}
