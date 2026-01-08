use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;

/// Test case that verifies the initial items returned when subscribing to chat list updates.
pub struct VerifySubscriptionInitialItemsTestCase {
    account_name: String,
    /// Expected groups in order. Empty means expect no groups.
    expected_groups: Vec<String>,
}

impl VerifySubscriptionInitialItemsTestCase {
    /// Creates a test case expecting an empty subscription.
    pub fn expect_empty(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            expected_groups: Vec::new(),
        }
    }

    /// Creates a test case expecting groups in exact order (most recent activity first).
    pub fn expect_groups_in_order(account_name: &str, group_names: Vec<&str>) -> Self {
        Self {
            account_name: account_name.to_string(),
            expected_groups: group_names.into_iter().map(String::from).collect(),
        }
    }
}

#[async_trait]
impl TestCase for VerifySubscriptionInitialItemsTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let account = context.get_account(&self.account_name)?;
        let subscription = context.whitenoise.subscribe_to_chat_list(account).await?;

        assert_eq!(
            subscription.initial_items.len(),
            self.expected_groups.len(),
            "Expected {} groups, got {}",
            self.expected_groups.len(),
            subscription.initial_items.len()
        );

        if self.expected_groups.is_empty() {
            tracing::info!(
                "✓ Subscription for '{}' returns empty chat list",
                self.account_name
            );
            return Ok(());
        }

        for (index, expected_name) in self.expected_groups.iter().enumerate() {
            let expected_group = context.get_group(expected_name)?;
            let actual_item = &subscription.initial_items[index];

            assert_eq!(
                actual_item.mls_group_id, expected_group.mls_group_id,
                "Position {}: expected '{}', got '{:?}'",
                index, expected_name, actual_item.name
            );
        }

        tracing::info!(
            "✓ Subscription for '{}' returns groups in correct order: {:?}",
            self.account_name,
            self.expected_groups
        );

        Ok(())
    }
}
