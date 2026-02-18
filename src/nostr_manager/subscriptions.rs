//! Subscription functions for NostrManager
//! This mostly handles subscribing and processing events as they come in while the user is active.

use std::collections::HashMap;
use std::time::Duration;

use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};

const MAX_USERS_PER_GLOBAL_SUBSCRIPTION: usize = 1000;

use crate::nostr_manager::{
    NostrManager, NostrManagerError, Result, utils::adjust_since_for_giftwrap,
};

impl NostrManager {
    /// Create a short hash from a pubkey for use in subscription IDs
    /// Uses first 12 characters of SHA256 hash for privacy and collision resistance, salted per session
    pub(crate) fn create_pubkey_hash(&self, pubkey: &PublicKey) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.session_salt());
        hasher.update(pubkey.to_bytes());
        let hash = hasher.finalize();
        format!("{:x}", hash)[..12].to_string()
    }

    // Sets up subscriptions in batches for all users and their relays
    pub(crate) async fn setup_batched_relay_subscriptions_with_signer(
        &self,
        users_with_relays: Vec<(PublicKey, Vec<RelayUrl>)>,
        default_relays: &[RelayUrl],
        signer: impl NostrSigner + 'static,
        since: Option<Timestamp>,
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::nostr_manager::setup_batched_relay_subscriptions_with_signer",
            "Setting up batched relay subscriptions with signer (users={}, defaults={})",
            users_with_relays.len(),
            default_relays.len()
        );

        // 1. Group users by relay
        let relay_user_map = self.group_users_by_relay(users_with_relays, default_relays);

        // 2. Set signer once and process all relays in parallel
        self.with_signer(signer, || async {
            self.setup_batched_relay_subscriptions_inner(relay_user_map, since)
                .await
        })
        .await?;

        Ok(())
    }

    // Sets up subscriptions in batches for all users and their relays (without signer).
    // Only used in tests; production code always uses the _with_signer variant.
    #[cfg(test)]
    pub(crate) async fn setup_batched_relay_subscriptions(
        &self,
        users_with_relays: Vec<(PublicKey, Vec<RelayUrl>)>,
        default_relays: &[RelayUrl],
        since: Option<Timestamp>,
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::nostr_manager::setup_batched_relay_subscriptions",
            "Setting up batched relay subscriptions (no signer) (users={}, defaults={})",
            users_with_relays.len(),
            default_relays.len()
        );

        let relay_user_map = self.group_users_by_relay(users_with_relays, default_relays);
        self.setup_batched_relay_subscriptions_inner(relay_user_map, since)
            .await
    }

    // Inner implementation for setting up batched relay subscriptions
    async fn setup_batched_relay_subscriptions_inner(
        &self,
        relay_user_map: std::collections::HashMap<RelayUrl, Vec<PublicKey>>,
        since: Option<Timestamp>,
    ) -> Result<()> {
        let futures = relay_user_map
            .into_iter()
            .map(|(relay_url, users)| async move {
                match self
                    .create_deterministic_batches_for_relay(relay_url.clone(), users, since)
                    .await
                {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::error!(
                            target: "whitenoise::nostr_manager::setup_batched_relay_subscriptions",
                            error = %e,
                            "Failed to create deterministic batches for relay: {}",
                            relay_url
                        );
                        false
                    }
                }
            });

        let results = futures::future::join_all(futures).await;
        if !results.into_iter().any(|success| success) {
            return Err(NostrManagerError::NoRelayConnections);
        }
        Ok(())
    }

    async fn create_deterministic_batches_for_relay(
        &self,
        relay_url: RelayUrl,
        users: Vec<PublicKey>,
        since: Option<Timestamp>,
    ) -> Result<()> {
        let batch_count = self.calculate_batch_count(users.len());

        // Group users into deterministic batches
        let mut batches: Vec<Vec<PublicKey>> = vec![Vec::new(); batch_count];
        for user in users {
            let batch_id = self.user_to_batch_id(&user, batch_count);
            batches[batch_id].push(user);
        }

        // Create subscription for each non-empty batch in parallel (signer already set at outer level)
        let batch_futures =
            batches
                .into_iter()
                .enumerate()
                .filter_map(|(batch_id, batch_users)| {
                    if batch_users.is_empty() {
                        None
                    } else {
                        let subscription_id = self.batched_subscription_id(&relay_url, batch_id);
                        let relay_url_clone = relay_url.clone();
                        Some(async move {
                            let res = self
                                .subscribe_user_batch(
                                    relay_url_clone,
                                    batch_users,
                                    subscription_id,
                                    since,
                                )
                                .await;
                            (batch_id, res)
                        })
                    }
                });

        let results = futures::future::join_all(batch_futures).await;

        let non_empty_batches = results.len();
        let failed_batches = results.iter().filter(|(_, r)| r.is_err()).count();

        // Log any errors
        for (batch_id, result) in results.iter() {
            if let Err(e) = result {
                tracing::error!(
                    target: "whitenoise::nostr_manager::create_deterministic_batches_for_relay",
                    error = %e,
                    "Failed to subscribe user batch {} for relay: {}",
                    batch_id,
                    relay_url
                );
            }
        }

        if non_empty_batches > 0 && non_empty_batches == failed_batches {
            return Err(NostrManagerError::NoRelayConnections);
        }

        Ok(())
    }

    /// Calculate batch count based on user count (stateless)
    fn calculate_batch_count(&self, user_count: usize) -> usize {
        if user_count == 0 {
            1
        } else {
            user_count.div_ceil(MAX_USERS_PER_GLOBAL_SUBSCRIPTION)
        }
    }

    /// Deterministic batch assignment: hash(pubkey) % batch_count
    fn user_to_batch_id(&self, pubkey: &PublicKey, batch_count: usize) -> usize {
        let mut hasher = Sha256::new();
        hasher.update(pubkey.to_bytes());
        let hash = hasher.finalize();
        let hash_int = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]);
        (hash_int as usize) % batch_count
    }

    fn group_users_by_relay(
        &self,
        users_with_relays: Vec<(PublicKey, Vec<RelayUrl>)>,
        default_relays: &[RelayUrl],
    ) -> HashMap<RelayUrl, Vec<PublicKey>> {
        let mut relay_user_map: HashMap<RelayUrl, Vec<PublicKey>> = HashMap::new();

        for (user_pubkey, mut user_relays) in users_with_relays {
            if user_relays.is_empty() {
                user_relays = default_relays.to_vec();
            }

            for relay_url in user_relays {
                relay_user_map
                    .entry(relay_url)
                    .or_default()
                    .push(user_pubkey);
            }
        }

        relay_user_map
    }

    /// Helper methods for batched subscriptions
    fn batched_subscription_id(&self, relay_url: &RelayUrl, batch_id: usize) -> SubscriptionId {
        let relay_hash = self.create_relay_hash(relay_url);
        SubscriptionId::new(format!("global_users_{}_{}", relay_hash, batch_id))
    }

    fn create_relay_hash(&self, relay_url: &RelayUrl) -> String {
        let mut hasher = Sha256::new();
        hasher.update(relay_url.as_str().as_bytes());
        let hash = hasher.finalize();
        format!("{:x}", hash)[..12].to_string()
    }

    async fn subscribe_user_batch(
        &self,
        relay_url: RelayUrl,
        batch_users: Vec<PublicKey>,
        subscription_id: SubscriptionId,
        since: Option<Timestamp>,
    ) -> Result<()> {
        let mut filter = Filter::new().authors(batch_users).kinds([
            Kind::Metadata,
            Kind::RelayList,
            Kind::InboxRelays,
            Kind::MlsKeyPackageRelays,
        ]);
        if let Some(since) = since {
            filter = filter.since(since);
        }

        self.ensure_relays_connected(std::slice::from_ref(&relay_url))
            .await?;
        self.client
            .subscribe_with_id_to(vec![relay_url], subscription_id, filter, None)
            .await?;
        Ok(())
    }

    /// Refresh subscriptions for a specific user across all their relays
    pub(crate) async fn refresh_user_global_subscriptions_with_signer(
        &self,
        user_pubkey: PublicKey,
        users_with_relays: Vec<(PublicKey, Vec<RelayUrl>)>,
        default_relays: &[RelayUrl],
        signer: impl NostrSigner + 'static,
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::nostr_manager::refresh_user_global_subscriptions_with_signer",
            "Refreshing user global subscriptions with signer"
        );
        let relay_user_map = self.group_users_by_relay(users_with_relays, default_relays);

        // Set signer once and process all relays in parallel
        self.with_signer(signer, || async {
            self.refresh_user_global_subscriptions_inner(user_pubkey, relay_user_map)
                .await
        })
        .await
    }

    /// Refresh subscriptions for a specific user across all their relays (without signer).
    /// Only used in tests; production code always uses the _with_signer variant.
    #[cfg(test)]
    pub(crate) async fn refresh_user_global_subscriptions(
        &self,
        user_pubkey: PublicKey,
        users_with_relays: Vec<(PublicKey, Vec<RelayUrl>)>,
        default_relays: &[RelayUrl],
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::nostr_manager::refresh_user_global_subscriptions",
            "Refreshing user global subscriptions (no signer)"
        );
        let relay_user_map = self.group_users_by_relay(users_with_relays, default_relays);
        self.refresh_user_global_subscriptions_inner(user_pubkey, relay_user_map)
            .await
    }

    /// Refresh subscriptions for all batches across all relays (with signer).
    ///
    /// Unlike `refresh_user_global_subscriptions_with_signer`, this refreshes
    /// every batch on every relay — not just the batches containing a specific
    /// user. Use this after bulk user discovery (e.g. processing a large contact
    /// list) where new users may be spread across many different relay batches.
    pub(crate) async fn refresh_all_global_subscriptions_with_signer(
        &self,
        users_with_relays: Vec<(PublicKey, Vec<RelayUrl>)>,
        default_relays: &[RelayUrl],
        signer: impl NostrSigner + 'static,
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::nostr_manager::subscriptions",
            "Refreshing all global subscriptions with signer"
        );
        let relay_user_map = self.group_users_by_relay(users_with_relays, default_relays);

        self.with_signer(signer, || async {
            self.refresh_all_global_subscriptions_inner(relay_user_map)
                .await
        })
        .await
    }

    /// Refresh subscriptions for all batches across all relays (without signer).
    pub(crate) async fn refresh_all_global_subscriptions(
        &self,
        users_with_relays: Vec<(PublicKey, Vec<RelayUrl>)>,
        default_relays: &[RelayUrl],
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::nostr_manager::subscriptions",
            "Refreshing all global subscriptions (no signer)"
        );
        let relay_user_map = self.group_users_by_relay(users_with_relays, default_relays);
        self.refresh_all_global_subscriptions_inner(relay_user_map)
            .await
    }

    /// Inner implementation for refreshing user global subscriptions
    async fn refresh_user_global_subscriptions_inner(
        &self,
        user_pubkey: PublicKey,
        relay_user_map: std::collections::HashMap<RelayUrl, Vec<PublicKey>>,
    ) -> Result<()> {
        let futures = relay_user_map
            .into_iter()
            .filter(|(_, users)| users.contains(&user_pubkey))
            .map(|(relay_url, users)| async move {
                if let Err(e) = self
                    .refresh_batch_for_relay_containing_user(relay_url.clone(), users, user_pubkey)
                    .await
                {
                    tracing::error!(
                        target: "whitenoise::nostr_manager::subscriptions",
                        error = %e,
                        "Failed to refresh batch for relay: {}",
                        relay_url
                    );
                }
            });

        futures::future::join_all(futures).await;
        Ok(())
    }

    /// Refreshes all batches across all relays.
    async fn refresh_all_global_subscriptions_inner(
        &self,
        relay_user_map: std::collections::HashMap<RelayUrl, Vec<PublicKey>>,
    ) -> Result<()> {
        let futures = relay_user_map
            .into_iter()
            .map(|(relay_url, users)| async move {
                if let Err(e) = self
                    .refresh_all_batches_for_relay(relay_url.clone(), users)
                    .await
                {
                    tracing::error!(
                        target: "whitenoise::nostr_manager::subscriptions",
                        error = %e,
                        "Failed to refresh batches for relay: {}",
                        relay_url
                    );
                }
            });

        futures::future::join_all(futures).await;
        Ok(())
    }

    /// This method rebuilds the subscriptions for all of the relays the user has
    async fn refresh_batch_for_relay_containing_user(
        &self,
        relay_url: RelayUrl,
        users: Vec<PublicKey>,
        user_pubkey: PublicKey,
    ) -> Result<()> {
        let batch_count = self.calculate_batch_count(users.len());
        let user_batch_id = self.user_to_batch_id(&user_pubkey, batch_count);

        // Group users into deterministic batches (same logic as setup)
        // we need this because we need to know all the users present in the batch
        let mut batches: Vec<Vec<PublicKey>> = vec![Vec::new(); batch_count];
        for user in users {
            let batch_id = self.user_to_batch_id(&user, batch_count);
            batches[batch_id].push(user);
        }

        let mut non_empty_batches = 0;
        let mut failed_batches = 0;

        // Only refresh the batch containing the triggering user
        if let Some(batch_users) = batches.get(user_batch_id)
            && !batch_users.is_empty()
        {
            non_empty_batches += 1;
            if let Err(e) = self
                .refresh_batch_subscription(relay_url.clone(), user_batch_id, batch_users.clone())
                .await
            {
                tracing::error!(
                    target: "whitenoise::nostr_manager::refresh_batch_for_relay_containing_user",
                    error = %e,
                    "Failed to refresh batch for relay: {}",
                    relay_url
                );
                failed_batches += 1;
            }
        }

        if (non_empty_batches > 0) && (non_empty_batches == failed_batches) {
            return Err(NostrManagerError::NoRelayConnections);
        }

        Ok(())
    }

    /// Refreshes all batches for a given relay.
    async fn refresh_all_batches_for_relay(
        &self,
        relay_url: RelayUrl,
        users: Vec<PublicKey>,
    ) -> Result<()> {
        let batch_count = self.calculate_batch_count(users.len());

        let mut batches: Vec<Vec<PublicKey>> = vec![Vec::new(); batch_count];
        for user in users {
            let batch_id = self.user_to_batch_id(&user, batch_count);
            batches[batch_id].push(user);
        }

        let batch_futures = batches
            .into_iter()
            .enumerate()
            .filter(|(_, batch_users)| !batch_users.is_empty())
            .map(|(batch_id, batch_users)| {
                let relay_url = relay_url.clone();
                async move {
                    if let Err(e) = self
                        .refresh_batch_subscription(relay_url.clone(), batch_id, batch_users)
                        .await
                    {
                        tracing::error!(
                            target: "whitenoise::nostr_manager::subscriptions",
                            error = %e,
                            "Failed to refresh batch {} for relay: {}",
                            batch_id,
                            relay_url
                        );
                    }
                }
            });

        futures::future::join_all(batch_futures).await;
        Ok(())
    }

    async fn refresh_batch_subscription(
        &self,
        relay_url: RelayUrl,
        batch_id: usize,
        batch_users: Vec<PublicKey>,
    ) -> Result<()> {
        let buffer_time = Timestamp::now() - Duration::from_secs(10);

        let subscription_id = self.batched_subscription_id(&relay_url, batch_id);
        self.client.unsubscribe(&subscription_id).await;

        self.subscribe_user_batch(relay_url, batch_users, subscription_id, Some(buffer_time))
            .await
    }

    pub(crate) async fn setup_account_subscriptions(
        &self,
        pubkey: PublicKey,
        user_relays: &[RelayUrl],
        inbox_relays: &[RelayUrl],
        group_relays: &[RelayUrl],
        nostr_group_ids: &[String],
        since: Option<Timestamp>,
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::nostr_manager::setup_account_subscriptions",
            "Setting up account subscriptions"
        );

        // Combine all relay types into a single deduplicated collection
        let all_relays: Vec<RelayUrl> = user_relays
            .iter()
            .chain(inbox_relays)
            .chain(group_relays)
            .cloned()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // Ensure we're connected to all relays before subscribing
        self.ensure_relays_connected(&all_relays).await?;

        // Set up core subscriptions in parallel
        let (user_follow_list_result, giftwrap_result, groups_result) = tokio::join!(
            self.setup_user_follow_list_subscription(pubkey, user_relays, since),
            self.setup_giftwrap_subscription(pubkey, inbox_relays, since),
            self.setup_group_messages_subscription(pubkey, nostr_group_ids, group_relays, since)
        );

        // Handle results
        user_follow_list_result?;
        giftwrap_result?;
        groups_result?;

        Ok(())
    }

    async fn setup_user_follow_list_subscription(
        &self,
        pubkey: PublicKey,
        user_relays: &[RelayUrl],
        since: Option<Timestamp>,
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::nostr_manager::setup_user_follow_list_subscription",
            "Setting up subscription for pubkey {} on {} relays",
            pubkey.to_hex(),
            user_relays.len()
        );
        let pubkey_hash = self.create_pubkey_hash(&pubkey);
        let subscription_id = SubscriptionId::new(format!("{}_user_follow_list", pubkey_hash));

        let mut user_follow_list_filter = Filter::new().kind(Kind::ContactList).author(pubkey);
        if let Some(since) = since {
            user_follow_list_filter = user_follow_list_filter.since(since);
        }

        self.client
            .subscribe_with_id_to(
                user_relays,
                subscription_id.clone(),
                user_follow_list_filter,
                None,
            )
            .await?;

        tracing::debug!(
            target: "whitenoise::nostr_manager::setup_user_follow_list_subscription",
            "FollowList subscription '{}' set up successfully for {}",
            subscription_id,
            pubkey.to_hex()
        );
        Ok(())
    }

    async fn setup_giftwrap_subscription(
        &self,
        pubkey: PublicKey,
        inbox_relays: &[RelayUrl],
        since: Option<Timestamp>,
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::nostr_manager::setup_giftwrap_subscription",
            "Setting up giftwrap subscription"
        );
        let pubkey_hash = self.create_pubkey_hash(&pubkey);
        let subscription_id = SubscriptionId::new(format!("{}_giftwrap", pubkey_hash));

        let mut giftwrap_filter = Filter::new().kind(Kind::GiftWrap).pubkey(pubkey);
        if let Some(since) = since {
            // Account for NIP-59 backdated timestamps - giftwrap events may be timestamped
            // in the past for privacy, so we look back further than last_synced_at
            let adjusted_since = adjust_since_for_giftwrap(Some(since));
            if let Some(adjusted) = adjusted_since {
                giftwrap_filter = giftwrap_filter.since(adjusted);
            }
        }

        self.client
            .subscribe_with_id_to(inbox_relays, subscription_id, giftwrap_filter, None)
            .await?;

        tracing::debug!(
            target: "whitenoise::nostr_manager::setup_giftwrap_subscription",
            "Giftwrap subscription set up"
        );
        Ok(())
    }

    /// Set up subscription for group messages - can be updated when groups change
    pub(crate) async fn setup_group_messages_subscription(
        &self,
        pubkey: PublicKey,
        nostr_group_ids: &[String],
        group_relays: &[RelayUrl],
        since: Option<Timestamp>,
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::nostr_manager::setup_group_messages_subscription",
            "Setting up group messages subscription"
        );
        if nostr_group_ids.is_empty() {
            // No groups yet, skip subscription
            return Ok(());
        }

        let pubkey_hash = self.create_pubkey_hash(&pubkey);
        let subscription_id = SubscriptionId::new(format!("{}_mls_messages", pubkey_hash));

        let mut mls_message_filter = Filter::new()
            .kind(Kind::MlsGroupMessage)
            .custom_tags(SingleLetterTag::lowercase(Alphabet::H), nostr_group_ids);

        if let Some(since) = since {
            mls_message_filter = mls_message_filter.since(since);
        }

        self.client
            .subscribe_with_id_to(group_relays, subscription_id, mls_message_filter, None)
            .await?;

        tracing::debug!(
            target: "whitenoise::nostr_manager::setup_group_messages_subscription",
            "Group messages subscription set up"
        );
        Ok(())
    }

    /// Unsubscribe from all account-specific subscriptions for a given pubkey.
    /// This includes user follow list, giftwrap, and MLS group message subscriptions.
    pub(crate) async fn unsubscribe_account_subscriptions(&self, pubkey: &PublicKey) -> Result<()> {
        let pubkey_hash = self.create_pubkey_hash(pubkey);

        let subscription_ids = [
            SubscriptionId::new(format!("{}_user_follow_list", pubkey_hash)),
            SubscriptionId::new(format!("{}_giftwrap", pubkey_hash)),
            SubscriptionId::new(format!("{}_mls_messages", pubkey_hash)),
        ];

        let unsubscribe_futures = subscription_ids
            .iter()
            .map(|id| self.client.unsubscribe(id));

        futures::future::join_all(unsubscribe_futures).await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::event_tracker::NoEventTracker;
    use nostr_sdk::Keys;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_create_pubkey_hash() {
        let (event_sender, _) = mpsc::channel(100);
        let event_tracker = Arc::new(NoEventTracker);
        let nostr_manager =
            NostrManager::new(event_sender, event_tracker, NostrManager::default_timeout())
                .await
                .unwrap();

        let pubkey = Keys::generate().public_key();
        let hash1 = nostr_manager.create_pubkey_hash(&pubkey);
        let hash2 = nostr_manager.create_pubkey_hash(&pubkey);

        // Same pubkey should produce same hash with same session salt
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 12); // Should be 12 characters as specified
    }

    #[tokio::test]
    async fn test_giftwrap_subscription_lookback_buffer() {
        use crate::nostr_manager::utils::GIFTWRAP_LOOKBACK_BUFFER;
        use std::time::Duration;

        // Test that adjust_since_for_giftwrap extends the lookback period correctly
        let original_timestamp = Timestamp::now();
        let adjusted = adjust_since_for_giftwrap(Some(original_timestamp));

        assert!(adjusted.is_some());
        let adjusted_ts = adjusted.unwrap();

        // Should be exactly GIFTWRAP_LOOKBACK_BUFFER earlier
        assert_eq!(adjusted_ts, original_timestamp - GIFTWRAP_LOOKBACK_BUFFER);

        // Verify the buffer is significant (7 days)
        assert_eq!(
            GIFTWRAP_LOOKBACK_BUFFER,
            Duration::from_secs(7 * 24 * 60 * 60)
        );

        // Test with None - should return None
        let none_result = adjust_since_for_giftwrap(None);
        assert!(none_result.is_none());
    }

    #[tokio::test]
    async fn test_setup_batched_relay_subscriptions_with_empty_users() {
        let (event_sender, _) = mpsc::channel(100);
        let event_tracker = Arc::new(NoEventTracker);
        let nostr_manager =
            NostrManager::new(event_sender, event_tracker, NostrManager::default_timeout())
                .await
                .unwrap();

        let keys = Keys::generate();
        let users_with_relays: Vec<(PublicKey, Vec<RelayUrl>)> = vec![];
        let default_relays: Vec<RelayUrl> = vec![];

        // With empty users and no relays, should fail with NoRelayConnections
        let result = nostr_manager
            .setup_batched_relay_subscriptions_with_signer(
                users_with_relays,
                &default_relays,
                keys.clone(),
                None,
            )
            .await;

        // Should error because there are no relay connections to establish
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            NostrManagerError::NoRelayConnections
        ));
    }

    #[tokio::test]
    async fn test_calculate_batch_count() {
        let (event_sender, _) = mpsc::channel(100);
        let event_tracker = Arc::new(NoEventTracker);
        let nostr_manager =
            NostrManager::new(event_sender, event_tracker, NostrManager::default_timeout())
                .await
                .unwrap();

        // Test edge cases
        assert_eq!(nostr_manager.calculate_batch_count(0), 1);
        assert_eq!(nostr_manager.calculate_batch_count(1), 1);
        assert_eq!(nostr_manager.calculate_batch_count(999), 1);
        assert_eq!(nostr_manager.calculate_batch_count(1000), 1);
        assert_eq!(nostr_manager.calculate_batch_count(1001), 2);
        assert_eq!(nostr_manager.calculate_batch_count(2000), 2);
        assert_eq!(nostr_manager.calculate_batch_count(2001), 3);
    }

    #[tokio::test]
    async fn test_user_to_batch_id_deterministic() {
        let (event_sender, _) = mpsc::channel(100);
        let event_tracker = Arc::new(NoEventTracker);
        let nostr_manager =
            NostrManager::new(event_sender, event_tracker, NostrManager::default_timeout())
                .await
                .unwrap();

        let pubkey = Keys::generate().public_key();
        let batch_count = 5;

        // Same pubkey should always map to same batch
        let batch_id_1 = nostr_manager.user_to_batch_id(&pubkey, batch_count);
        let batch_id_2 = nostr_manager.user_to_batch_id(&pubkey, batch_count);
        assert_eq!(batch_id_1, batch_id_2);

        // Batch ID should be within valid range
        assert!(batch_id_1 < batch_count);
    }

    /// Test setup_batched_relay_subscriptions (no signer version) with empty users
    #[tokio::test]
    async fn test_setup_batched_relay_subscriptions_no_signer_empty_users() {
        let (event_sender, _) = mpsc::channel(100);
        let event_tracker = Arc::new(NoEventTracker);
        let nostr_manager =
            NostrManager::new(event_sender, event_tracker, NostrManager::default_timeout())
                .await
                .unwrap();

        let users_with_relays: Vec<(PublicKey, Vec<RelayUrl>)> = vec![];
        let default_relays: Vec<RelayUrl> = vec![];

        // With empty users and no relays, should fail with NoRelayConnections
        let result = nostr_manager
            .setup_batched_relay_subscriptions(users_with_relays, &default_relays, None)
            .await;

        // Should error because there are no relay connections to establish
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            NostrManagerError::NoRelayConnections
        ));
    }

    /// Test that no-signer version works the same as with-signer version for empty inputs
    #[tokio::test]
    async fn test_setup_batched_relay_subscriptions_no_signer_single_user() {
        let (event_sender, _) = mpsc::channel(100);
        let event_tracker = Arc::new(NoEventTracker);
        let nostr_manager =
            NostrManager::new(event_sender, event_tracker, NostrManager::default_timeout())
                .await
                .unwrap();

        let pubkey = Keys::generate().public_key();
        let users_with_relays = vec![(pubkey, vec![])];
        let default_relays: Vec<RelayUrl> = vec![];

        // With user but no relays, should fail with NoRelayConnections
        let result = nostr_manager
            .setup_batched_relay_subscriptions(users_with_relays, &default_relays, None)
            .await;

        // Should error because there are no relay connections
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            NostrManagerError::NoRelayConnections
        ));
    }

    /// Test refresh_user_global_subscriptions (no signer version)
    #[tokio::test]
    async fn test_refresh_user_global_subscriptions_empty_relays() {
        let (event_sender, _) = mpsc::channel(100);
        let event_tracker = Arc::new(NoEventTracker);
        let nostr_manager =
            NostrManager::new(event_sender, event_tracker, NostrManager::default_timeout())
                .await
                .unwrap();

        let pubkey = Keys::generate().public_key();
        let users_with_relays: Vec<(PublicKey, Vec<RelayUrl>)> = vec![];
        let default_relays: Vec<RelayUrl> = vec![];

        // With empty relays, should handle gracefully
        let result = nostr_manager
            .refresh_user_global_subscriptions(pubkey, users_with_relays, &default_relays)
            .await;

        // Should succeed but do nothing (no relays to connect to)
        assert!(result.is_ok());
    }

    /// Test group_users_by_relay helper function
    #[tokio::test]
    async fn test_group_users_by_relay() {
        let (event_sender, _) = mpsc::channel(100);
        let event_tracker = Arc::new(NoEventTracker);
        let nostr_manager =
            NostrManager::new(event_sender, event_tracker, NostrManager::default_timeout())
                .await
                .unwrap();

        let pubkey1 = Keys::generate().public_key();
        let pubkey2 = Keys::generate().public_key();
        let relay1 = RelayUrl::parse("wss://relay1.test.com").unwrap();
        let relay2 = RelayUrl::parse("wss://relay2.test.com").unwrap();

        let users_with_relays = vec![
            (pubkey1, vec![relay1.clone(), relay2.clone()]),
            (pubkey2, vec![relay2.clone()]),
        ];
        let default_relays: Vec<RelayUrl> = vec![];

        let grouped = nostr_manager.group_users_by_relay(users_with_relays, &default_relays);

        // relay1 should have 1 user (pubkey1)
        assert_eq!(grouped.get(&relay1).map(|v| v.len()), Some(1));
        // relay2 should have 2 users (pubkey1 and pubkey2)
        assert_eq!(grouped.get(&relay2).map(|v| v.len()), Some(2));
    }

    /// Test group_users_by_relay with default relays
    #[tokio::test]
    async fn test_group_users_by_relay_with_defaults() {
        let (event_sender, _) = mpsc::channel(100);
        let event_tracker = Arc::new(NoEventTracker);
        let nostr_manager =
            NostrManager::new(event_sender, event_tracker, NostrManager::default_timeout())
                .await
                .unwrap();

        let pubkey1 = Keys::generate().public_key();
        let default_relay = RelayUrl::parse("wss://default.relay.com").unwrap();

        // User with no relays should use defaults
        let users_with_relays = vec![(pubkey1, vec![])];
        let default_relays = vec![default_relay.clone()];

        let grouped = nostr_manager.group_users_by_relay(users_with_relays, &default_relays);

        // User with no relays should be grouped under default relay
        assert_eq!(grouped.get(&default_relay).map(|v| v.len()), Some(1));
    }
}
