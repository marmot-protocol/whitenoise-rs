use crate::integration_tests::{core::*, test_cases::shared::*};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

pub struct BasicMessagingScenario {
    context: ScenarioContext,
}

impl BasicMessagingScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for BasicMessagingScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        CreateAccountsTestCase::with_names(vec!["basic_msg_creator", "basic_msg_member"])
            .execute(&mut self.context)
            .await?;

        CreateGroupTestCase::basic()
            .with_name("basic_messaging_test_group")
            .with_members("basic_msg_creator", vec!["basic_msg_member"])
            .execute(&mut self.context)
            .await?;

        // Wait for the member to receive and process the welcome message
        WaitForWelcomeTestCase::for_account("basic_msg_member", "basic_messaging_test_group")
            .execute(&mut self.context)
            .await?;

        // Verify the member performed a post-welcome self-update (MIP-02).
        // This confirms the member's leaf node key material was rotated,
        // replacing the publicly-available KeyPackage keys with fresh material.
        VerifySelfUpdateTestCase::for_account("basic_msg_member", "basic_messaging_test_group")
            .execute(&mut self.context)
            .await?;

        SendMessageTestCase::basic()
            .with_sender("basic_msg_creator")
            .with_group("basic_messaging_test_group")
            .with_message_id_key("basic_msg_initial")
            .execute(&mut self.context)
            .await?;

        let basic_message_id = self.context.get_message_id("basic_msg_initial")?.clone();

        SendMessageTestCase::basic()
            .with_sender("basic_msg_creator")
            .into_reaction("👍", &basic_message_id)
            .with_group("basic_messaging_test_group")
            .execute(&mut self.context)
            .await?;

        SendMessageTestCase::basic()
            .with_sender("basic_msg_member")
            .into_reply("Great message!", &basic_message_id)
            .with_group("basic_messaging_test_group")
            .execute(&mut self.context)
            .await?;

        Ok(())
    }
}
