use std::collections::HashMap;

use nostr_sdk::SubscriptionId;
use tokio::sync::RwLock;

use super::SubscriptionContext;

/// Local routing table from opaque relay subscription IDs to internal context.
#[derive(Debug, Default)]
pub(crate) struct RelayRouter {
    subscription_contexts: RwLock<HashMap<SubscriptionId, SubscriptionContext>>,
}

impl RelayRouter {
    pub(crate) async fn record_subscription_context(
        &self,
        subscription_id: SubscriptionId,
        context: SubscriptionContext,
    ) {
        self.subscription_contexts
            .write()
            .await
            .insert(subscription_id, context);
    }

    pub(crate) async fn subscription_context(
        &self,
        subscription_id: &SubscriptionId,
    ) -> Option<SubscriptionContext> {
        self.subscription_contexts
            .read()
            .await
            .get(subscription_id)
            .cloned()
    }

    pub(crate) async fn remove_subscription_context(
        &self,
        subscription_id: &SubscriptionId,
    ) -> Option<SubscriptionContext> {
        self.subscription_contexts
            .write()
            .await
            .remove(subscription_id)
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::RelayUrl;

    use super::*;
    use crate::relay_control::{RelayPlane, SubscriptionStream};

    #[tokio::test]
    async fn test_record_and_lookup_subscription_context() {
        let router = RelayRouter::default();
        let subscription_id = SubscriptionId::new("opaque_sub");
        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let context = SubscriptionContext {
            plane: RelayPlane::Compatibility,
            account_pubkey: None,
            relay_url,
            stream: SubscriptionStream::CompatibilityGlobal,
        };

        router
            .record_subscription_context(subscription_id.clone(), context.clone())
            .await;

        assert_eq!(
            router.subscription_context(&subscription_id).await,
            Some(context)
        );
    }
}
