use std::collections::HashMap;

use nostr_sdk::{RelayUrl, SubscriptionId};
use tokio::sync::RwLock;

use super::SubscriptionContext;
use crate::perf_span;

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
#[derive(Debug, Default)]
pub(crate) struct RelayRouter {
    subscription_contexts: RwLock<HashMap<RelaySubscriptionKey, SubscriptionContext>>,
}

impl RelayRouter {
    pub(crate) async fn record_subscription_context(
        &self,
        relay_url: RelayUrl,
        subscription_id: SubscriptionId,
        context: SubscriptionContext,
    ) {
        let _span = perf_span!("relay::router_record_context");
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
        let _span = perf_span!("relay::router_subscription_context");
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
        let _span = perf_span!("relay::router_remove_context");
        self.subscription_contexts
            .write()
            .await
            .remove(&RelaySubscriptionKey::new(
                relay_url.clone(),
                subscription_id.clone(),
            ))
    }

    pub(crate) async fn matching_group_contexts(
        &self,
        relay_url: &RelayUrl,
        group_id: &str,
    ) -> Vec<SubscriptionContext> {
        let _span = perf_span!("relay::router_matching_group_contexts");
        // This is an O(n) scan over active subscription contexts under a read
        // lock. That is acceptable for the current migrated planes and account
        // counts; add a secondary index if group fanout becomes hot.
        self.subscription_contexts
            .read()
            .await
            .iter()
            .filter(|(key, context)| {
                key.relay_url == *relay_url
                    && context.stream == crate::relay_control::SubscriptionStream::GroupMessages
                    && context
                        .group_ids
                        .iter()
                        .any(|candidate| candidate == group_id)
            })
            .map(|(_, context)| context.clone())
            .collect()
    }

    pub(crate) async fn context_count(&self) -> usize {
        self.subscription_contexts.read().await.len()
    }

    pub(crate) async fn clear(&self) {
        self.subscription_contexts.write().await.clear();
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
            group_ids: vec![],
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
            group_ids: vec![],
        };
        let context_b = SubscriptionContext {
            plane: RelayPlane::Group,
            account_pubkey: None,
            relay_url: relay_url_b.clone(),
            stream: SubscriptionStream::GroupMessages,
            group_ids: vec!["group-a".to_string()],
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

    #[tokio::test]
    async fn test_matching_group_contexts_finds_all_accounts_for_group_on_relay() {
        let router = RelayRouter::default();
        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let context_a = SubscriptionContext {
            plane: RelayPlane::Group,
            account_pubkey: Some(nostr_sdk::Keys::generate().public_key()),
            relay_url: relay_url.clone(),
            stream: SubscriptionStream::GroupMessages,
            group_ids: vec!["group-a".to_string(), "group-b".to_string()],
        };
        let context_b = SubscriptionContext {
            plane: RelayPlane::Group,
            account_pubkey: Some(nostr_sdk::Keys::generate().public_key()),
            relay_url: relay_url.clone(),
            stream: SubscriptionStream::GroupMessages,
            group_ids: vec!["group-b".to_string()],
        };
        let context_c = SubscriptionContext {
            plane: RelayPlane::Group,
            account_pubkey: Some(nostr_sdk::Keys::generate().public_key()),
            relay_url: relay_url.clone(),
            stream: SubscriptionStream::GroupMessages,
            group_ids: vec!["group-c".to_string()],
        };

        router
            .record_subscription_context(
                relay_url.clone(),
                SubscriptionId::new("sub-a"),
                context_a.clone(),
            )
            .await;
        router
            .record_subscription_context(
                relay_url.clone(),
                SubscriptionId::new("sub-b"),
                context_b.clone(),
            )
            .await;
        router
            .record_subscription_context(relay_url.clone(), SubscriptionId::new("sub-c"), context_c)
            .await;

        let mut matches = router.matching_group_contexts(&relay_url, "group-b").await;
        matches.sort_by_key(|context| context.account_pubkey.map(|pubkey| pubkey.to_hex()));

        let mut expected = vec![context_a, context_b];
        expected.sort_by_key(|context| context.account_pubkey.map(|pubkey| pubkey.to_hex()));

        assert_eq!(matches, expected);
    }
}
