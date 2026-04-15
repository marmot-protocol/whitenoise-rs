use std::sync::Arc;

use mdk_core::prelude::GroupId;
use nostr_sdk::prelude::*;
use nostr_sdk::{PublicKey, RelayUrl};

use super::SharedSigner;
use crate::RelayType;
use crate::nostr_manager::{NostrManagerError, Result as NostrResult};
use crate::relay_control::RelayControlPlane;
use crate::relay_control::ephemeral::{EphemeralPlane, MessagePublishResult};
use crate::relay_control::groups::GroupSubscriptionSpec;
use crate::whitenoise::database::Database;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::message_streaming::MessageStreamManager;

/// Scoped handle into the shared ephemeral relay plane.
///
/// Carries this account's identity and signer reference. Delegates to shared
/// warm relay connections internally. The API never exposes the pubkey or signer
/// — they are baked in at construction time.
pub(crate) struct AccountEphemeralHandle {
    account_pubkey: PublicKey,
    ephemeral: EphemeralPlane,
    signer: SharedSigner,
}

impl AccountEphemeralHandle {
    pub(crate) fn new(
        account_pubkey: PublicKey,
        ephemeral: EphemeralPlane,
        signer: SharedSigner,
    ) -> Self {
        Self {
            account_pubkey,
            ephemeral,
            signer,
        }
    }

    /// Read the current signer, returning an error if none is set.
    async fn require_signer(&self) -> NostrResult<Arc<dyn NostrSigner>> {
        self.signer.read().await.clone().ok_or_else(|| {
            NostrManagerError::WhitenoiseInstance(
                WhitenoiseError::SignerUnavailable(self.account_pubkey).to_string(),
            )
        })
    }

    // ── Publish helpers (signer baked in) ───────────────────────────

    #[expect(dead_code, reason = "phase 7+")]
    pub(crate) async fn publish_gift_wrap(
        &self,
        receiver: &PublicKey,
        rumor: UnsignedEvent,
        extra_tags: &[Tag],
        relays: &[RelayUrl],
    ) -> NostrResult<Output<EventId>> {
        let signer = self.require_signer().await?;
        self.ephemeral
            .publish_gift_wrap_to(
                receiver,
                rumor,
                extra_tags,
                self.account_pubkey,
                relays,
                signer,
            )
            .await
    }

    #[expect(dead_code, reason = "phase 7+")]
    pub(crate) async fn publish_metadata(
        &self,
        metadata: &Metadata,
        relays: &[RelayUrl],
    ) -> NostrResult<Output<EventId>> {
        let signer = self.require_signer().await?;
        self.ephemeral
            .publish_metadata_with_signer(metadata, relays, signer)
            .await
    }

    #[expect(dead_code, reason = "phase 5+")]
    pub(crate) async fn publish_relay_list(
        &self,
        relay_list: &[RelayUrl],
        relay_type: RelayType,
        target_relays: &[RelayUrl],
    ) -> NostrResult<()> {
        let signer = self.require_signer().await?;
        self.ephemeral
            .publish_relay_list_with_signer(relay_list, relay_type, target_relays, signer)
            .await
    }

    #[expect(dead_code, reason = "phase 5+")]
    pub(crate) async fn publish_follow_list(
        &self,
        follow_list: &[PublicKey],
        target_relays: &[RelayUrl],
        created_at: Option<Timestamp>,
    ) -> NostrResult<()> {
        let signer = self.require_signer().await?;
        self.ephemeral
            .publish_follow_list_with_signer_at(follow_list, target_relays, created_at, signer)
            .await
    }

    #[expect(dead_code, reason = "phase 14")]
    pub(crate) async fn publish_key_package(
        &self,
        encoded_key_package: &str,
        relays: &[RelayUrl],
        tags: &[Tag],
    ) -> NostrResult<Output<EventId>> {
        let signer = self.require_signer().await?;
        self.ephemeral
            .publish_key_package_with_signer(encoded_key_package, relays, tags, signer)
            .await
    }

    #[expect(dead_code, reason = "phase 7+")]
    pub(crate) async fn publish_event_deletion(
        &self,
        event_ids: &[EventId],
        relays: &[RelayUrl],
    ) -> NostrResult<Output<EventId>> {
        if event_ids.is_empty() {
            return Err(NostrManagerError::WhitenoiseInstance(
                "Cannot publish event deletion with empty event_ids list".to_string(),
            ));
        }
        let signer = self.require_signer().await?;
        if event_ids.len() == 1 {
            self.ephemeral
                .publish_event_deletion_with_signer(&event_ids[0], relays, signer)
                .await
        } else {
            self.ephemeral
                .publish_batch_event_deletion_with_signer(event_ids, relays, signer)
                .await
        }
    }

    // ── Publish helpers (event already signed) ──────────────────────

    #[expect(dead_code, reason = "phase 7+")]
    pub(crate) async fn publish_event(
        &self,
        event: Event,
        relays: &[RelayUrl],
        quorum_threshold: Option<usize>,
    ) -> NostrResult<Output<EventId>> {
        match quorum_threshold {
            Some(threshold) => {
                self.ephemeral
                    .publish_event_with_quorum(event, &self.account_pubkey, relays, threshold)
                    .await
            }
            None => {
                self.ephemeral
                    .publish_event_to(event, &self.account_pubkey, relays)
                    .await
            }
        }
    }

    #[expect(dead_code, reason = "phase 7+")]
    pub(crate) async fn publish_message_event(
        &self,
        event: Event,
        relays: &[RelayUrl],
        event_id: &str,
        group_id: &GroupId,
        database: &Database,
        stream_manager: &Arc<MessageStreamManager>,
    ) -> MessagePublishResult {
        self.ephemeral
            .publish_message_event(
                event,
                &self.account_pubkey,
                relays,
                event_id,
                group_id,
                database,
                stream_manager,
            )
            .await
    }

    // ── Fetch helpers (anonymous scope, no signer needed) ───────────

    #[expect(dead_code, reason = "phase 7+")]
    pub(crate) async fn fetch_metadata(
        &self,
        relays: &[RelayUrl],
        pubkey: PublicKey,
    ) -> NostrResult<Option<Event>> {
        self.ephemeral.fetch_metadata_from(relays, pubkey).await
    }

    #[expect(dead_code, reason = "phase 7+")]
    pub(crate) async fn fetch_user_relays(
        &self,
        pubkey: PublicKey,
        relay_type: RelayType,
        relays: &[RelayUrl],
    ) -> NostrResult<Option<Event>> {
        self.ephemeral
            .fetch_user_relays(pubkey, relay_type, relays)
            .await
    }

    #[expect(dead_code, reason = "phase 7+")]
    pub(crate) async fn fetch_user_key_package(
        &self,
        pubkey: PublicKey,
        relays: &[RelayUrl],
    ) -> NostrResult<Option<Event>> {
        self.ephemeral.fetch_user_key_package(pubkey, relays).await
    }

    // ── Relay warming ───────────────────────────────────────────────

    pub(crate) async fn warm_relays(&self, relays: &[RelayUrl]) -> NostrResult<()> {
        self.ephemeral
            .warm_relays_for_account(self.account_pubkey, relays)
            .await
    }
}

/// Scoped handle into the shared group relay plane.
///
/// One shared group relay session serves all accounts. This handle manages
/// subscriptions for one account only, baking in the account identity so
/// callers never pass a pubkey.
pub(crate) struct AccountGroupHandle {
    account_pubkey: PublicKey,
    relay_control: Arc<RelayControlPlane>,
}

impl AccountGroupHandle {
    pub(crate) fn new(account_pubkey: PublicKey, relay_control: Arc<RelayControlPlane>) -> Self {
        Self {
            account_pubkey,
            relay_control,
        }
    }

    pub(crate) async fn sync_subscriptions(
        &self,
        group_specs: &[GroupSubscriptionSpec],
        since: Option<Timestamp>,
    ) -> NostrResult<()> {
        self.relay_control
            .sync_account_group_subscriptions(self.account_pubkey, group_specs, since)
            .await
    }

    /// Snapshot the current group-plane state for rollback during activation.
    pub(crate) async fn save_state(&self) -> Option<Vec<GroupSubscriptionSpec>> {
        self.relay_control
            .group_plane_account_state(&self.account_pubkey)
            .await
    }

    /// Remove this account from the group plane entirely.
    pub(crate) async fn remove(&self) {
        self.relay_control
            .remove_account_from_group_plane(&self.account_pubkey)
            .await;
    }

    pub(crate) async fn has_active_subscription(&self) -> bool {
        self.relay_control
            .has_group_subscription(&self.account_pubkey)
            .await
    }

    pub(crate) async fn group_count(&self) -> usize {
        self.relay_control
            .group_plane_account_group_count(&self.account_pubkey)
            .await
    }

    /// Remove this account's ephemeral relay scopes.
    pub(crate) async fn remove_ephemeral_scope(&self) {
        self.relay_control
            .remove_account_ephemeral_scope(&self.account_pubkey)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nostr_sdk::Keys;
    use tokio::sync::RwLock;

    use super::*;
    use crate::relay_control::observability::{RelayObservability, RelayObservabilityConfig};
    use crate::whitenoise::session::test_helpers::test_db;

    fn test_ephemeral_plane(database: Arc<Database>) -> EphemeralPlane {
        let (event_sender, _) = tokio::sync::mpsc::channel(16);
        EphemeralPlane::new(
            crate::relay_control::ephemeral::EphemeralPlaneConfig::default(),
            database,
            event_sender,
            RelayObservability::new(RelayObservabilityConfig::default()),
        )
    }

    // ── AccountEphemeralHandle ──────────────────────────────────────

    #[tokio::test]
    async fn ephemeral_handle_require_signer_returns_error_when_none() {
        let db = test_db().await;
        let plane = test_ephemeral_plane(db);
        let pk = Keys::generate().public_key();
        let signer: SharedSigner = Arc::new(RwLock::new(None));

        let handle = AccountEphemeralHandle::new(pk, plane, signer);
        let result = handle.require_signer().await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ephemeral_handle_require_signer_returns_signer_when_set() {
        let db = test_db().await;
        let plane = test_ephemeral_plane(db);
        let keys = Keys::generate();
        let pk = keys.public_key();
        let signer: SharedSigner = Arc::new(RwLock::new(Some(Arc::new(keys))));

        let handle = AccountEphemeralHandle::new(pk, plane, signer);
        let result = handle.require_signer().await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn ephemeral_handle_sees_signer_updates() {
        let db = test_db().await;
        let plane = test_ephemeral_plane(db);
        let pk = Keys::generate().public_key();
        let signer: SharedSigner = Arc::new(RwLock::new(None));

        let handle = AccountEphemeralHandle::new(pk, plane, signer.clone());

        // Initially no signer.
        assert!(handle.require_signer().await.is_err());

        // Simulate external signer registration (writes to the shared slot).
        *signer.write().await = Some(Arc::new(Keys::generate()));

        // Handle sees the update.
        assert!(handle.require_signer().await.is_ok());
    }

    // ── AccountGroupHandle ─────────────────────────────────────────

    #[tokio::test]
    async fn group_handle_delegates_group_count() {
        let db = test_db().await;
        let (event_sender, _) = tokio::sync::mpsc::channel(16);
        let relay_control = Arc::new(RelayControlPlane::new(db, vec![], event_sender, [0u8; 16]));
        let pk = Keys::generate().public_key();
        let handle = AccountGroupHandle::new(pk, relay_control);

        assert_eq!(handle.group_count().await, 0);
    }
}
