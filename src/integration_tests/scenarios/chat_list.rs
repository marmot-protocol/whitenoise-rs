use crate::integration_tests::{
    core::*,
    test_cases::{chat_list::*, metadata_management::*, shared::*},
};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

pub struct ChatListScenario {
    context: ScenarioContext,
}

impl ChatListScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for ChatListScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        // ============================================================
        // Setup: Create all accounts needed for this scenario
        // ============================================================
        CreateAccountsTestCase::with_names(vec![
            "chat_list_empty",
            "chat_list_alice",
            "chat_list_bob",
            "chat_list_charlie",
        ])
        .execute(&mut self.context)
        .await?;

        // ============================================================
        // Test 1: Empty chat list for account with no groups
        // ============================================================
        tracing::info!("Test 1: Verifying empty chat list...");

        VerifyChatListTestCase::new("chat_list_empty")
            .expecting_groups(0)
            .expecting_dms(0)
            .execute(&mut self.context)
            .await?;

        // ============================================================
        // Test 2: Chat list with Groups and DMs
        // ============================================================
        tracing::info!("Test 2: Setting up Groups and DMs...");

        // Set metadata for bob so DM name resolution works
        UpdateMetadataTestCase::for_account("chat_list_bob")
            .with_name("Bob")
            .with_picture("https://example.com/bob.jpg")
            .execute(&mut self.context)
            .await?;

        // Create a Group (with non-empty name)
        CreateGroupTestCase::basic()
            .with_name("chat_list_group")
            .with_members("chat_list_alice", vec!["chat_list_bob"])
            .execute(&mut self.context)
            .await?;

        // Create a DM (empty name = DirectMessage type)
        CreateDmTestCase::new("chat_list_alice", "chat_list_charlie")
            .with_context_name("chat_list_dm")
            .execute(&mut self.context)
            .await?;

        // Verify Alice has 1 group and 1 DM
        VerifyChatListTestCase::new("chat_list_alice")
            .expecting_groups(1)
            .expecting_dms(1)
            .execute(&mut self.context)
            .await?;

        // ============================================================
        // Test 3: Group without messages (uses created_at for sorting)
        // ============================================================
        tracing::info!("Test 3: Verifying group without messages...");

        VerifyChatListItemTestCase::new("chat_list_alice", "chat_list_group")
            .expecting_name("chat_list_group") // Name matches context key from with_name()
            .expecting_no_last_message()
            .expecting_not_pending() // Creator's groups are auto-accepted
            .expecting_unread_count(0) // No messages = no unread
            .execute(&mut self.context)
            .await?;

        // ============================================================
        // Test 4: Send message and verify it appears in chat list
        // ============================================================
        tracing::info!("Test 4: Verifying last message in chat list...");

        SendMessageTestCase::basic()
            .with_sender("chat_list_alice")
            .with_group("chat_list_group")
            .with_content("Hello from Alice!")
            .with_message_id_key("chat_list_msg1")
            .execute(&mut self.context)
            .await?;

        // Wait for message to be received from relay and aggregated
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        VerifyChatListItemTestCase::new("chat_list_alice", "chat_list_group")
            .expecting_name("chat_list_group")
            .expecting_last_message("Hello from Alice!")
            .expecting_unread_count(1) // 1 message, none marked as read
            .execute(&mut self.context)
            .await?;

        // ============================================================
        // Test 5: Multiple messages and DM last message
        // ============================================================
        tracing::info!("Test 5: Verifying messages in multiple chats...");

        // Wait to ensure timestamp difference (Nostr uses second granularity)
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Send a message to the DM
        SendMessageTestCase::basic()
            .with_sender("chat_list_alice")
            .with_group("chat_list_dm")
            .with_content("Hello in DM!")
            .with_message_id_key("chat_list_dm_msg")
            .execute(&mut self.context)
            .await?;

        // Wait for message to be received from relay and aggregated
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // Verify both chats have messages now
        VerifyChatListTestCase::new("chat_list_alice")
            .expecting_groups(1)
            .expecting_dms(1)
            .execute(&mut self.context)
            .await?;

        // Verify DM has the correct last message
        VerifyChatListItemTestCase::new("chat_list_alice", "chat_list_dm")
            .expecting_last_message("Hello in DM!")
            .expecting_unread_count(1) // 1 DM message, none marked as read
            .execute(&mut self.context)
            .await?;

        // ============================================================
        // Test 6: DM name resolution from other user's metadata
        // ============================================================
        tracing::info!("Test 6: Setting up DM with user metadata...");

        // Create DM between Alice and Bob (Bob has metadata set)
        CreateDmTestCase::new("chat_list_alice", "chat_list_bob")
            .with_context_name("chat_list_dm_bob")
            .execute(&mut self.context)
            .await?;

        // Verify DM properties
        VerifyDmChatListItemTestCase::new("chat_list_alice", "chat_list_dm_bob", "chat_list_bob")
            .execute(&mut self.context)
            .await?;

        // ============================================================
        // Test 7: Welcomer pubkey verification (invited member's perspective)
        // ============================================================
        tracing::info!("Test 7: Verifying welcomer_pubkey for invited member...");

        // Wait for Bob to receive and process the welcome for chat_list_group
        WaitForWelcomeTestCase::for_account("chat_list_bob", "chat_list_group")
            .execute(&mut self.context)
            .await?;

        // Verify Bob's view of the group shows Alice as the welcomer and pending confirmation
        // Note: Bob also sees Alice's message that was sent in Test 4
        VerifyChatListItemTestCase::new("chat_list_bob", "chat_list_group")
            .expecting_name("chat_list_group")
            .expecting_last_message("Hello from Alice!") // Bob sees Alice's message
            .expecting_pending_confirmation(true) // Bob hasn't accepted yet
            .expecting_welcomer("chat_list_alice") // Alice invited Bob
            .expecting_unread_count(1) // Bob sees 1 unread message from Alice
            .execute(&mut self.context)
            .await?;

        // ============================================================
        // Test 8: Mark message as read and verify unread_count decreases
        // ============================================================
        tracing::info!("Test 8: Verifying mark_message_read reduces unread_count...");

        // Alice marks her message in the group as read
        MarkMessageReadTestCase::new("chat_list_alice", "chat_list_msg1")
            .execute(&mut self.context)
            .await?;

        // Verify unread_count is now 0 for Alice in the group
        VerifyChatListItemTestCase::new("chat_list_alice", "chat_list_group")
            .expecting_name("chat_list_group")
            .expecting_last_message("Hello from Alice!")
            .expecting_unread_count(0) // Now 0 after marking as read
            .execute(&mut self.context)
            .await?;

        // ============================================================
        // Summary
        // ============================================================
        tracing::info!("✓ Chat list scenario completed with:");
        tracing::info!("  • Empty chat list verification");
        tracing::info!("  • Groups and DMs mixed in chat list");
        tracing::info!("  • Group with and without messages");
        tracing::info!("  • Last message content verification");
        tracing::info!("  • Sorting by last activity time");
        tracing::info!("  • DM name resolution from user metadata");
        tracing::info!("  • Creator's groups auto-accepted (pending_confirmation=false)");
        tracing::info!("  • Invited member sees welcomer_pubkey and pending_confirmation=true");
        tracing::info!("  • Mark message as read reduces unread_count");

        Ok(())
    }
}
