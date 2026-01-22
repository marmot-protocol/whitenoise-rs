use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;

/// Verifies the order of chats in the chat list.
pub struct VerifyChatListOrderTestCase {
    account_name: String,
    expected_order: Vec<String>,
}

impl VerifyChatListOrderTestCase {
    pub fn new(account_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            expected_order: Vec::new(),
        }
    }

    /// Specifies the expected order of groups by their context names.
    /// First item in the list should appear first in the chat list.
    pub fn expecting_order(mut self, group_context_names: Vec<&str>) -> Self {
        self.expected_order = group_context_names.into_iter().map(String::from).collect();
        self
    }
}

#[async_trait]
impl TestCase for VerifyChatListOrderTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Verifying chat list order for account: {} (expecting {:?})",
            self.account_name,
            self.expected_order
        );

        let account = context.get_account(&self.account_name)?;
        let chat_list = context.whitenoise.get_chat_list(account).await?;

        // Build the expected group IDs in order
        let expected_group_ids: Vec<_> = self
            .expected_order
            .iter()
            .map(|name| context.get_group(name).map(|g| g.mls_group_id.clone()))
            .collect::<Result<Vec<_>, _>>()?;

        // Get actual group IDs from chat list (only for groups we're checking)
        let actual_group_ids: Vec<_> = chat_list
            .iter()
            .map(|item| item.mls_group_id.clone())
            .filter(|id| expected_group_ids.contains(id))
            .collect();

        // Compare orders
        assert_eq!(
            actual_group_ids.len(),
            expected_group_ids.len(),
            "Expected {} groups in order check but found {}",
            expected_group_ids.len(),
            actual_group_ids.len()
        );

        for (i, (actual, expected)) in actual_group_ids.iter().zip(&expected_group_ids).enumerate()
        {
            assert_eq!(
                actual, expected,
                "At position {}: expected group '{}' but got different group",
                i, self.expected_order[i]
            );
        }

        tracing::info!("✓ Chat list order verified: {:?}", self.expected_order);
        Ok(())
    }
}
