use crate::integration_tests::{
    core::*,
    test_cases::{shared::*, subscription_processing::*},
};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;
use nostr_sdk::prelude::*;

pub struct SubscriptionProcessingScenario {
    context: ScenarioContext,
}

impl SubscriptionProcessingScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for SubscriptionProcessingScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        CreateAccountsTestCase::with_names(vec!["subscription_test_account"])
            .execute(&mut self.context)
            .await?;

        // Test 1: Account metadata update
        let account_metadata = Metadata {
            name: Some("Updated User via Subscription".to_string()),
            ..Default::default()
        };
        PublishSubscriptionUpdateTestCase::for_account("subscription_test_account")
            .with_metadata(account_metadata)
            .execute(&mut self.context)
            .await?;

        // Test 2: Account relay list update
        let account_relay_url = "ws://localhost:8780".to_string();
        PublishSubscriptionUpdateTestCase::for_account("subscription_test_account")
            .with_relay_update(account_relay_url)
            .execute(&mut self.context)
            .await?;

        // Test 3: External user metadata update
        let alice_keys = Keys::generate();
        let alice_metadata = Metadata {
            name: Some("Alice Updated via Subscription".to_string()),
            about: Some("Alice's updated bio from external client".to_string()),
            ..Default::default()
        };
        PublishSubscriptionUpdateTestCase::for_external_user(alice_keys.clone())
            .with_metadata(alice_metadata)
            .execute(&mut self.context)
            .await?;

        // Test 4: External user relay list update
        let alice_relay_url = "ws://localhost:8781".to_string();
        PublishSubscriptionUpdateTestCase::for_external_user(alice_keys.clone())
            .with_relay_update(alice_relay_url)
            .execute(&mut self.context)
            .await?;

        // Test 5: Publish a follow list (as a TestCase with assertion)
        PublishSubscriptionUpdateTestCase::for_account("subscription_test_account")
            .with_follow_list(vec![alice_keys.public_key()])
            .execute(&mut self.context)
            .await?;

        // Test 6: Verify timestamp policy using a single builder-style test case
        VerifyLastSyncedTimestampTestCase::for_account_follow_event()
            .execute(&mut self.context)
            .await?;
        VerifyLastSyncedTimestampTestCase::for_global_metadata_event()
            .execute(&mut self.context)
            .await?;

        Ok(())
    }
}
