use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::prelude::*;

use crate::whitenoise::{
    Whitenoise,
    error::{Result, WhitenoiseError},
};

impl Whitenoise {
    /// Ensures that group relays are available for publishing evolution events.
    /// Returns the validated relay URLs.
    ///
    /// # Arguments
    /// * `mdk` - The NostrMls instance to get relays from
    /// * `group_id` - The ID of the group
    ///
    /// # Returns
    /// * `Ok(Vec<nostr_sdk::RelayUrl>)` - Vector of relay URLs
    /// * `Err(WhitenoiseError::GroupMissingRelays)` - If no relays are configured
    pub(crate) fn ensure_group_relays(
        mdk: &MDK<MdkSqliteStorage>,
        group_id: &GroupId,
    ) -> Result<Vec<nostr_sdk::RelayUrl>> {
        let group_relays = mdk.get_relays(group_id)?;

        if group_relays.is_empty() {
            return Err(WhitenoiseError::GroupMissingRelays);
        }

        Ok(group_relays.into_iter().collect())
    }

    /// Publishes a pre-signed event to relays with retry and exponential backoff.
    ///
    /// Relay-control ephemeral sessions own the bounded retry policy for
    /// one-off publishes. This wrapper keeps the group-evolution call sites on
    /// a single publish entry point while the rest of the control-plane
    /// migration lands.
    ///
    /// This is the single entry-point for publishing MLS protocol events
    /// (evolution commits, proposals, etc.) so that retry policy changes are
    /// made in one place. When a durable publish queue is introduced later,
    /// only this method needs to be replaced.
    pub(crate) async fn publish_event_with_retry(
        &self,
        event: Event,
        account_pubkey: &PublicKey,
        relay_urls: &[RelayUrl],
    ) -> Result<()> {
        self.relay_control
            .publish_event_to(event, account_pubkey, relay_urls)
            .await?;
        Ok(())
    }

    /// Publishes an evolution event and merges the pending commit on success.
    ///
    /// Per MIP-03 this is the canonical ordering for MLS state evolution:
    /// 1. Caller creates the pending commit via an MDK operation
    /// 2. This method publishes the evolution event (with retry)
    /// 3. Only after at least one relay accepts, the pending commit is merged
    ///
    /// # Publish failure and rollback
    ///
    /// If all publish attempts fail, the pending commit is cleared via
    /// `clear_pending_commit`, rolling back the MLS group to its pre-commit
    /// state. This ensures a failed publish never leaves the group stuck with
    /// a dangling pending commit that would block all subsequent operations.
    /// The error from the publish attempt is returned to the caller.
    pub(crate) async fn publish_and_merge_commit(
        &self,
        evolution_event: Event,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        relay_urls: &[RelayUrl],
    ) -> Result<()> {
        let mdk = self.create_mdk_for_account(*account_pubkey)?;

        if let Err(publish_err) = self
            .publish_event_with_retry(evolution_event, account_pubkey, relay_urls)
            .await
        {
            // Publish failed — roll back the pending commit so the group is
            // not left in a blocked state. Log but do not propagate the
            // clear error; the original publish error is what matters to the caller.
            if let Err(clear_err) = mdk.clear_pending_commit(group_id) {
                tracing::warn!(
                    target: "whitenoise::groups::publish_and_merge_commit",
                    "Failed to clear pending commit after publish failure for group {}: {}",
                    hex::encode(group_id.as_slice()),
                    clear_err,
                );
            }
            return Err(publish_err);
        }

        // Relay accepted — now safe to advance local MLS state
        mdk.merge_pending_commit(group_id)?;

        Ok(())
    }
}
