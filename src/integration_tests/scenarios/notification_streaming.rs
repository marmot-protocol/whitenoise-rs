use crate::integration_tests::{
    core::*,
    test_cases::{chat_list::CreateDmTestCase, notification_streaming::*, shared::*},
};
use crate::whitenoise::notification_streaming::NotificationTrigger;
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

/// Scenario that tests notification streaming functionality end-to-end.
///
/// # Design Note: Single-Instance Limitation
///
/// The notification system filters out messages from ANY logged-in account to prevent
/// users from receiving notifications for their own messages (even across multiple
/// accounts on the same device). In production, Alice sending to Bob works because:
/// - Alice's device: filters the message (Alice is logged in)
/// - Bob's device: emits notification (Alice is NOT logged in on Bob's device)
///
/// In integration tests, all accounts share the same Whitenoise instance, so ALL
/// accounts are "logged in" simultaneously. This means NewMessage notifications
/// cannot be tested in this environment.
///
/// However, GroupInvite notifications do NOT filter by welcomer, so they can be
/// tested in this environment.
///
/// This scenario verifies:
/// 1. GroupInvite notifications work (no sender filtering for invites)
/// 2. DM GroupInvite notifications correctly set is_dm=true
/// 3. Messages from logged-in accounts are correctly filtered (negative test)
pub struct NotificationStreamingScenario {
    context: ScenarioContext,
}

impl NotificationStreamingScenario {
    const GROUP_NAME: &'static str = "notif_stream_group";
    const DM_NAME: &'static str = "notif_dm";

    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }

    async fn phase1_setup(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 1: Setup accounts ===");

        CreateAccountsTestCase::with_names(vec!["creator", "member", "third"])
            .execute(&mut self.context)
            .await?;

        Ok(())
    }

    async fn phase2_test_group_invite_notification(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 2: Test GroupInvite notification for group ===");

        // Subscribe to notifications BEFORE creating the group
        let invite_verifier =
            VerifyNotificationUpdateTestCase::new(NotificationTrigger::GroupInvite)
                .expect_group(Self::GROUP_NAME)
                .expect_is_dm(false)
                .expect_sender("creator")
                .expect_receiver("member");
        invite_verifier.subscribe(&self.context)?;

        // Create the group - this will send a welcome to 'member'
        CreateGroupTestCase::basic()
            .with_name(Self::GROUP_NAME)
            .with_members("creator", vec!["member"])
            .execute(&mut self.context)
            .await?;

        // Wait for welcome to be processed (auto-finalization)
        WaitForWelcomeTestCase::for_account("member", Self::GROUP_NAME)
            .execute(&mut self.context)
            .await?;

        // Verify the GroupInvite notification was received
        invite_verifier.execute(&mut self.context).await?;

        tracing::info!("GroupInvite notification received for group");
        Ok(())
    }

    async fn phase3_test_dm_invite_notification(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 3: Test GroupInvite notification for DM ===");

        // Subscribe to notifications BEFORE creating the DM
        let dm_invite_verifier =
            VerifyNotificationUpdateTestCase::new(NotificationTrigger::GroupInvite)
                .expect_group(Self::DM_NAME)
                .expect_is_dm(true)
                .expect_sender("creator")
                .expect_receiver("third");
        dm_invite_verifier.subscribe(&self.context)?;

        // Create a DM between creator and third
        CreateDmTestCase::new("creator", "third")
            .with_context_name(Self::DM_NAME)
            .execute(&mut self.context)
            .await?;

        // Wait for welcome to be processed
        WaitForWelcomeTestCase::for_account("third", Self::DM_NAME)
            .execute(&mut self.context)
            .await?;

        // Verify the GroupInvite notification was received with is_dm=true
        dm_invite_verifier.execute(&mut self.context).await?;

        tracing::info!("GroupInvite notification received for DM with is_dm=true");
        Ok(())
    }

    async fn phase4_test_message_filtering(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 4: Verify messages from logged-in accounts are filtered ===");

        // Subscribe to notifications
        let subscription = self.context.whitenoise.subscribe_to_notifications();
        let mut receiver = subscription.updates;

        // Send a message from 'creator' (a logged-in account)
        // No notification should be emitted because 'creator' is logged in
        SendMessageTestCase::basic()
            .with_sender("creator")
            .with_group(Self::GROUP_NAME)
            .with_message_id_key("msg_filtered")
            .with_content("This should not trigger a notification")
            .execute(&mut self.context)
            .await?;

        // Wait briefly and verify NO notification was received
        let result =
            tokio::time::timeout(tokio::time::Duration::from_secs(2), receiver.recv()).await;

        assert!(
            result.is_err(),
            "Should NOT receive NewMessage notification for messages from logged-in accounts"
        );

        tracing::info!("Messages from logged-in accounts correctly filtered");
        Ok(())
    }

    async fn phase5_test_multiple_subscribers(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("=== Phase 5: Verify multiple subscribers receive same notification ===");

        // Create two subscriptions
        let subscription1 = self.context.whitenoise.subscribe_to_notifications();
        let subscription2 = self.context.whitenoise.subscribe_to_notifications();
        let mut receiver1 = subscription1.updates;
        let mut receiver2 = subscription2.updates;

        // Create a new group - both subscribers should receive GroupInvite
        const MULTI_SUB_GROUP: &str = "multi_sub_group";

        CreateGroupTestCase::basic()
            .with_name(MULTI_SUB_GROUP)
            .with_members("creator", vec!["third"])
            .execute(&mut self.context)
            .await?;

        WaitForWelcomeTestCase::for_account("third", MULTI_SUB_GROUP)
            .execute(&mut self.context)
            .await?;

        // Both receivers should get the GroupInvite notification
        let result1 =
            tokio::time::timeout(tokio::time::Duration::from_secs(10), receiver1.recv()).await;
        let result2 =
            tokio::time::timeout(tokio::time::Duration::from_secs(10), receiver2.recv()).await;

        assert!(
            result1.is_ok(),
            "First subscriber should receive notification"
        );
        assert!(
            result2.is_ok(),
            "Second subscriber should receive notification"
        );

        let update1 = result1.unwrap().expect("Should receive update");
        let update2 = result2.unwrap().expect("Should receive update");

        assert_eq!(update1.trigger, NotificationTrigger::GroupInvite);
        assert_eq!(update2.trigger, NotificationTrigger::GroupInvite);

        tracing::info!("Multiple subscribers correctly received same notification");
        Ok(())
    }
}

#[async_trait]
impl Scenario for NotificationStreamingScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        tracing::info!("Starting NotificationStreamingScenario");

        self.phase1_setup().await?;
        self.phase2_test_group_invite_notification().await?;
        self.phase3_test_dm_invite_notification().await?;
        self.phase4_test_message_filtering().await?;
        self.phase5_test_multiple_subscribers().await?;

        tracing::info!("NotificationStreamingScenario completed successfully");
        Ok(())
    }
}
