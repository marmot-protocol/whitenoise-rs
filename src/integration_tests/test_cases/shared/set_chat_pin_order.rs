use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;

/// Sets the pin order for a chat.
pub struct SetChatPinOrderTestCase {
    account_name: String,
    group_context_name: String,
    pin_order: Option<i64>,
}

impl SetChatPinOrderTestCase {
    pub fn new(account_name: &str, group_context_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_context_name: group_context_name.to_string(),
            pin_order: None,
        }
    }

    /// Pins the chat with the specified order (lower values appear first).
    pub fn with_pin_order(mut self, order: i64) -> Self {
        self.pin_order = Some(order);
        self
    }

    /// Unpins the chat.
    pub fn unpinned(mut self) -> Self {
        self.pin_order = None;
        self
    }
}

#[async_trait]
impl TestCase for SetChatPinOrderTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        let action = match self.pin_order {
            Some(order) => format!("pin_order={}", order),
            None => "unpinned".to_string(),
        };
        tracing::info!(
            "Setting chat '{}' to {} for account: {}",
            self.group_context_name,
            action,
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_context_name)?;

        let updated = context
            .whitenoise
            .set_chat_pin_order(account, &group.mls_group_id, self.pin_order)
            .await?;

        assert_eq!(
            updated.pin_order, self.pin_order,
            "Expected pin_order={:?} but got {:?}",
            self.pin_order, updated.pin_order
        );

        tracing::info!(
            "✓ Chat '{}' set to {} successfully",
            self.group_context_name,
            action
        );
        Ok(())
    }
}
