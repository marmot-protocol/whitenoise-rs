use std::collections::HashMap;

use nostr_sdk::{RelayUrl, SubscriptionId};
use tokio::sync::RwLock;

use super::SubscriptionContext;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RelaySubscriptionKey {
    relay_url: RelayUrl,
    subscription_id: SubscriptionId,
}

impl RelaySubscriptionKey {
    fn new(relay_url: RelayUrl, subscription_id: SubscriptionId) -> Self {
        Self {
            relay_url,
            subscription_id,
        }
    }
}

/// Local routing table from opaque relay subscription IDs to internal context.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub(crate) struct RelayRouter {
    subscription_contexts: RwLock<HashMap<RelaySubscriptionKey, SubscriptionContext>>,
}

#[allow(dead_code)]
impl RelayRouter {
    pub(crate) async fn record_subscription_context(
        &self,
        relay_url: RelayUrl,
        subscription_id: SubscriptionId,
        context: SubscriptionContext,
    ) {
        self.subscription_contexts.write().await.insert(
            RelaySubscriptionKey::new(relay_url, subscription_id),
            context,
        );
    }

    pub(crate) async fn subscription_context(
        &self,
        relay_url: &RelayUrl,
        subscription_id: &SubscriptionId,
    ) -> Option<SubscriptionContext> {
        self.subscription_contexts
            .read()
            .await
            .get(&RelaySubscriptionKey::new(
                relay_url.clone(),
                subscription_id.clone(),
            ))
            .cloned()
    }

    pub(crate) async fn remove_subscription_context(
        &self,
        relay_url: &RelayUrl,
        subscription_id: &SubscriptionId,
    ) -> Option<SubscriptionContext> {
        self.subscription_contexts
            .write()
            .await
            .remove(&RelaySubscriptionKey::new(
                relay_url.clone(),
                subscription_id.clone(),
            ))
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
            plane: RelayPlane::Discovery,
            account_pubkey: None,
            relay_url: relay_url.clone(),
            stream: SubscriptionStream::DiscoveryUserData,
        };

        router
            .record_subscription_context(
                relay_url.clone(),
                subscription_id.clone(),
                context.clone(),
            )
            .await;

        assert_eq!(
            router
                .subscription_context(&relay_url, &subscription_id)
                .await,
            Some(context)
        );
    }

    #[tokio::test]
    async fn test_same_subscription_id_on_different_relays_is_isolated() {
        let router = RelayRouter::default();
        let subscription_id = SubscriptionId::new("opaque_sub");
        let relay_url_a = RelayUrl::parse("wss://relay-a.example.com").unwrap();
        let relay_url_b = RelayUrl::parse("wss://relay-b.example.com").unwrap();
        let context_a = SubscriptionContext {
            plane: RelayPlane::Discovery,
            account_pubkey: None,
            relay_url: relay_url_a.clone(),
            stream: SubscriptionStream::DiscoveryUserData,
        };
        let context_b = SubscriptionContext {
            plane: RelayPlane::Group,
            account_pubkey: None,
            relay_url: relay_url_b.clone(),
            stream: SubscriptionStream::GroupMessages,
        };

        router
            .record_subscription_context(
                relay_url_a.clone(),
                subscription_id.clone(),
                context_a.clone(),
            )
            .await;
        router
            .record_subscription_context(
                relay_url_b.clone(),
                subscription_id.clone(),
                context_b.clone(),
            )
            .await;

        assert_eq!(
            router
                .subscription_context(&relay_url_a, &subscription_id)
                .await,
            Some(context_a)
        );
        assert_eq!(
            router
                .subscription_context(&relay_url_b, &subscription_id)
                .await,
            Some(context_b)
        );
    }
}
