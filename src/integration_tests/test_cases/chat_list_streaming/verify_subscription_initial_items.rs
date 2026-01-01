use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;

/// Test case that verifies the initial items returned when subscribing to chat list updates.
pub struct VerifySubscriptionInitialItemsTestCase {
    account_name: String,
    expect_empty: bool,
    expected_group_name: Option<String>,
}

impl VerifySubscriptionInitialItemsTestCase {
    /// Creates a test case expecting an empty subscription
    pub fn expect_empty(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            expect_empty: true,
            expected_group_name: None,
        }
    }

    /// Creates a test case expecting to find a specific group in the subscription
    pub fn expect_group(account_name: &str, group_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            expect_empty: false,
            expected_group_name: Some(group_name.to_string()),
        }
    }
}

#[async_trait]
impl TestCase for VerifySubscriptionInitialItemsTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let account = context.get_account(&self.account_name)?;
        let subscription = context.whitenoise.subscribe_to_chat_list(account).await?;

        if self.expect_empty {
            assert!(
                subscription.initial_items.is_empty(),
                "Expected empty initial items for '{}', got {} items",
                self.account_name,
                subscription.initial_items.len()
            );
            tracing::info!(
                "✓ Subscription for '{}' returns empty chat list",
                self.account_name
            );
        } else {
            assert!(
                !subscription.initial_items.is_empty(),
                "Expected non-empty initial items for '{}'",
                self.account_name
            );

            if let Some(expected_group) = &self.expected_group_name {
                let group = context.get_group(expected_group)?;
                let has_group = subscription
                    .initial_items
                    .iter()
                    .any(|item| item.mls_group_id == group.mls_group_id);

                assert!(
                    has_group,
                    "Expected to find group '{}' in initial items for '{}'",
                    expected_group, self.account_name
                );
                tracing::info!(
                    "✓ Subscription for '{}' contains group '{}'",
                    self.account_name,
                    expected_group
                );
            } else {
                tracing::info!(
                    "✓ Subscription for '{}' returns {} items",
                    self.account_name,
                    subscription.initial_items.len()
                );
            }
        }

        Ok(())
    }
}
