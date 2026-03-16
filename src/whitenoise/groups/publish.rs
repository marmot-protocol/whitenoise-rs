use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::prelude::*;

use crate::{
    perf_span,
    whitenoise::{
        Whitenoise,
        error::{Result, WhitenoiseError},
    },
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
        let _span = perf_span!("groups::publish_event_with_retry");
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
        let _span = perf_span!("groups::publish_and_merge_commit");
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

#[cfg(test)]
mod tests {
    use nostr_sdk::prelude::*;

    use crate::whitenoise::test_utils::*;

    /// When relay publish fails, `publish_and_merge_commit` must clear the
    /// pending commit (rollback) and return the publish error. The MLS group
    /// epoch must NOT advance.
    #[tokio::test]
    async fn test_publish_and_merge_commit_rollback_on_publish_failure() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let admin_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        wait_for_key_package_publication(&whitenoise, &[&member_account]).await;

        let group = whitenoise
            .create_group(
                &admin_account,
                vec![member_account.pubkey],
                create_nostr_group_config_data(vec![admin_account.pubkey]),
                None,
            )
            .await
            .unwrap();
        let group_id = &group.mls_group_id;

        // Create a pending commit via self-update
        let mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let update_result = mdk.self_update(group_id).unwrap();
        let epoch_before = mdk.get_group(group_id).unwrap().unwrap().epoch;

        // Use an unreachable relay so publish fails
        let bad_relay = RelayUrl::parse("ws://127.0.0.1:1").unwrap();

        let result = whitenoise
            .publish_and_merge_commit(
                update_result.evolution_event,
                &admin_account.pubkey,
                group_id,
                &[bad_relay],
            )
            .await;

        assert!(result.is_err(), "Should fail when relay is unreachable");

        // Epoch must not have advanced — the pending commit was rolled back
        let mdk = whitenoise
            .create_mdk_for_account(admin_account.pubkey)
            .unwrap();
        let epoch_after = mdk.get_group(group_id).unwrap().unwrap().epoch;
        assert_eq!(
            epoch_before, epoch_after,
            "Epoch must not advance after failed publish (rollback)"
        );
    }
}
