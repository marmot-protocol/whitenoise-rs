use crate::integration_tests::{core::*, test_cases::login_flow::*};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

/// Integration test scenario for the multi-step login flow.
///
/// Tests all paths through `login_start` / `login_publish_default_relays` /
/// `login_with_custom_relay` / `login_cancel`:
///
/// 1. Happy path: relay lists exist on the network, login completes in one call.
/// 2. No relay lists: login_start returns NeedsRelayLists.
/// 3. Publish defaults: user chooses to publish default relay lists, login completes.
/// 4. Custom relay found: user provides a relay URL, lists are found, login completes.
/// 5. Custom relay not found: user provides a relay URL, lists are NOT found,
///    returns NeedsRelayLists again (then falls back to publishing defaults).
/// 6. Cancel: user cancels mid-flow, partial state is cleaned up.
pub struct LoginFlowScenario {
    context: ScenarioContext,
}

impl LoginFlowScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for LoginFlowScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        // 1. Happy path: relay lists already published on the network.
        LoginStartHappyPathTestCase::new("login_flow_happy")
            .execute(&mut self.context)
            .await?;

        // 2. No relay lists: login_start detects missing lists.
        LoginStartNoRelaysTestCase::new("login_flow_no_relays")
            .execute(&mut self.context)
            .await?;

        // 3. Publish defaults: complete the login from step 2.
        LoginPublishDefaultsTestCase::new("login_flow_no_relays")
            .execute(&mut self.context)
            .await?;

        // 4. Custom relay: relay lists published on a specific relay.
        LoginCustomRelayTestCase::new("login_flow_custom")
            .execute(&mut self.context)
            .await?;

        // 5. Custom relay not found: lists don't exist on the provided relay.
        //    Then fall back to publishing defaults to complete.
        LoginCustomRelayNotFoundTestCase::new("login_flow_custom_miss")
            .execute(&mut self.context)
            .await?;
        // Complete the login from step 5 by publishing defaults.
        LoginPublishDefaultsTestCase::new("login_flow_custom_miss")
            .execute(&mut self.context)
            .await?;

        // 6. Cancel: start a login then back out.
        LoginCancelTestCase::new("login_flow_cancel")
            .execute(&mut self.context)
            .await?;

        Ok(())
    }
}
