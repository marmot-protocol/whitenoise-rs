use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::Utc;
use mdk_core::GroupId;
use nostr_sdk::prelude::*;

use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    accounts_groups::AccountGroup,
    chat_list_streaming::ChatListUpdateTrigger,
    database::published_key_packages::PublishedKeyPackage,
    error::{Result, WhitenoiseError},
    group_information::{GroupInformation, GroupType},
    relays::Relay,
};

impl Whitenoise {
    pub async fn handle_giftwrap(&self, account: &Account, event: Event) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::event_handlers::handle_giftwrap",
            "Giftwrap received for account: {}",
            account.pubkey.to_hex()
        );

        // For external signer accounts, use the registered signer.
        // For local accounts, use the keys from the secrets store.
        let unwrapped = if account.uses_external_signer() {
            let signer = self.get_external_signer(&account.pubkey).ok_or_else(|| {
                WhitenoiseError::Configuration(format!(
                    "No external signer registered for account: {}",
                    account.pubkey.to_hex()
                ))
            })?;
            tracing::debug!(
                target: "whitenoise::event_handlers::handle_giftwrap",
                "Using external signer for giftwrap decryption"
            );
            // Arc<dyn NostrSigner> implements NostrSigner, so we can pass it directly
            extract_rumor(&signer, &event).await.map_err(|e| {
                WhitenoiseError::Configuration(format!(
                    "Failed to decrypt giftwrap with external signer: {}",
                    e
                ))
            })?
        } else {
            let keys = self
                .secrets_store
                .get_nostr_keys_for_pubkey(&account.pubkey)?;
            extract_rumor(&keys, &event).await.map_err(|e| {
                WhitenoiseError::Configuration(format!("Failed to decrypt giftwrap: {}", e))
            })?
        };

        match unwrapped.rumor.kind {
            Kind::MlsWelcome => {
                self.process_welcome(account, event, unwrapped.rumor)
                    .await?;
            }
            _ => {
                tracing::debug!(
                    target: "whitenoise::event_handlers::handle_giftwrap",
                    "Received unhandled giftwrap of kind {:?}",
                    unwrapped.rumor.kind
                );
            }
        }

        Ok(())
    }

    async fn process_welcome(
        &self,
        account: &Account,
        event: Event,
        rumor: UnsignedEvent,
    ) -> Result<()> {
        // Extract key package event ID from the rumor tags early — needed for pre-check
        let key_package_event_id: Option<EventId> = rumor
            .tags
            .iter()
            .find(|tag| {
                tag.kind() == TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E))
            })
            .and_then(|tag| tag.content())
            .and_then(|content| EventId::parse(content).ok());

        // Pre-check: do we have this key package and is its key material still available?
        // This avoids expensive MLS crypto operations when the KP is unknown or deleted.
        if let Some(ref kp_event_id) = key_package_event_id {
            match PublishedKeyPackage::find_by_event_id(
                &account.pubkey,
                &kp_event_id.to_hex(),
                &self.database,
            )
            .await
            {
                Ok(Some(pkg)) if pkg.key_material_deleted => {
                    tracing::warn!(
                        target: "whitenoise::event_processor::process_welcome",
                        "Key material already deleted for this key package, skipping Welcome"
                    );
                    return Ok(());
                }
                Ok(None) => {
                    tracing::warn!(
                        target: "whitenoise::event_processor::process_welcome",
                        "Unknown key package referenced in Welcome, skipping"
                    );
                    return Ok(());
                }
                Ok(Some(_)) => {} // Good — KP exists, key material available
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::event_processor::process_welcome",
                        "Failed to look up key package: {}, proceeding anyway",
                        e
                    );
                    // Don't block on DB lookup failure — fall through to MLS
                }
            }
        }

        let mdk = self.create_mdk_for_account(account.pubkey)?;

        // Process the welcome to get group info (but don't accept yet)
        let welcome = mdk
            .process_welcome(&event.id, &rumor)
            .map_err(WhitenoiseError::MdkCoreError)?;
        tracing::debug!(target: "whitenoise::event_processor::process_welcome", "Processed welcome event");

        let group_id = welcome.mls_group_id.clone();
        let group_name = welcome.group_name.clone();
        let welcomer_pubkey = welcome.welcomer;

        // For DM groups (empty name), the welcomer is the other participant.
        // In the Marmot protocol, DM welcomes are always sent by the initiator,
        // who is the only other member in a two-party DM group.
        let dm_peer_pubkey = if GroupInformation::infer_group_type_from_group_name(&group_name)
            == GroupType::DirectMessage
        {
            Some(welcomer_pubkey)
        } else {
            None
        };

        let account_group = AccountGroup {
            id: None,
            account_pubkey: account.pubkey,
            mls_group_id: group_id.clone(),
            user_confirmation: None,
            welcomer_pubkey: Some(welcomer_pubkey),
            last_read_message_id: None,
            pin_order: None,
            dm_peer_pubkey,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        account_group.save(&self.database).await?;
        tracing::debug!(target: "whitenoise::event_processor::process_welcome", "New AccountGroup created and saved");

        // Now accept the welcome to finalize MLS membership
        mdk.accept_welcome(&welcome)
            .map_err(WhitenoiseError::MdkCoreError)?;
        tracing::debug!(target: "whitenoise::event_processor::process_welcome", "Auto-accepted welcome, MLS membership finalized");

        // Spawn background task for remaining operations (DB writes, network calls)
        // All operations are idempotent and failures are logged but don't stop other operations
        tokio::spawn(Self::background_finalize_welcome(
            account.clone(),
            group_id,
            group_name,
            key_package_event_id,
            welcomer_pubkey,
        ));

        Ok(())
    }

    /// Background task wrapper that gets Whitenoise instance and delegates to core logic.
    /// This thin wrapper exists because tokio::spawn requires 'static lifetime.
    async fn background_finalize_welcome(
        account: Account,
        group_id: GroupId,
        group_name: String,
        key_package_event_id: Option<EventId>,
        welcomer_pubkey: PublicKey,
    ) {
        let Ok(whitenoise) = Whitenoise::get_instance() else {
            tracing::error!(
                target: "whitenoise::event_processor::process_welcome::background",
                "Failed to get Whitenoise instance"
            );
            return;
        };

        Self::finalize_welcome_with_instance(
            whitenoise,
            &account,
            &group_id,
            &group_name,
            key_package_event_id,
            welcomer_pubkey,
        )
        .await;
    }

    /// Core welcome finalization logic. Testable because it takes Whitenoise as a parameter.
    /// Handles DB writes, network calls, and other non-critical operations.
    /// All operations are idempotent and failures are logged but don't stop other operations.
    ///
    /// # Sequencing
    ///
    /// 1. **Subscription setup** — awaited first so the relay connection is live
    ///    before we do anything else.
    /// 2. **Catch-up fetch** — only if subscriptions succeeded.  Fetches recent
    ///    group messages from relays and queues them through the event processor.
    ///    This ensures messages sent between the Welcome being dispatched and our
    ///    subscription becoming active are available before the self-update
    ///    advances the epoch.
    /// 3. **Independent ops** (group info, key rotation, image sync, welcomer
    ///    user lookup) — run concurrently; failures are logged but do not block.
    /// 4. **Self-update** — runs only if subscription setup succeeded.  If setup
    ///    failed, the self-update is skipped and the reason is logged so the
    ///    caller can diagnose the problem.
    pub(crate) async fn finalize_welcome_with_instance(
        whitenoise: &Whitenoise,
        account: &Account,
        group_id: &GroupId,
        group_name: &str,
        key_package_event_id: Option<EventId>,
        welcomer_pubkey: PublicKey,
    ) {
        // Get signer early - needed for subscriptions
        let signer = match whitenoise.get_signer_for_account(account) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    account = %account.pubkey.to_hex(),
                    error = %e,
                    "Failed to get signer; aborting welcome finalization"
                );
                return;
            }
        };

        // --- Step 1: subscription setup (must happen before catch-up and self-update) ---
        let subscription_ok = match Self::setup_group_subscriptions(whitenoise, account, signer)
            .await
        {
            Ok(()) => {
                tracing::debug!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    account = %account.pubkey.to_hex(),
                    "Group subscriptions established"
                );
                true
            }
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    account = %account.pubkey.to_hex(),
                    group = %hex::encode(group_id.as_slice()),
                    error = %e,
                    reason = "subscription_setup_failed",
                    "Subscription setup failed; skipping catch-up and self-update to avoid epoch mismatch"
                );
                false
            }
        };

        // --- Step 2: bounded catch-up fetch (only when subscriptions are live) ---
        if subscription_ok {
            Self::catchup_group_messages(whitenoise, account, group_id).await;
        }

        // --- Step 3: independent operations (run concurrently regardless of subscription status) ---
        let (group_info_result, key_rotation_result, image_sync_result, welcomer_user_result) = tokio::join!(
            Self::create_group_info(whitenoise, group_id, group_name),
            Self::rotate_key_package(whitenoise, account, key_package_event_id),
            Self::sync_group_image(whitenoise, account, group_id),
            Self::ensure_welcomer_user_exists(whitenoise, welcomer_pubkey),
        );

        if let Err(e) = group_info_result {
            tracing::error!(
                target: "whitenoise::event_processor::process_welcome::background",
                group = %hex::encode(group_id.as_slice()),
                error = %e,
                "Failed to create GroupInformation"
            );
        } else {
            whitenoise
                .emit_chat_list_update(account, group_id, ChatListUpdateTrigger::NewGroup)
                .await;
            whitenoise
                .emit_group_invite_notification_if_enabled(
                    account,
                    group_id,
                    group_name,
                    welcomer_pubkey,
                )
                .await;
        }

        if let Err(e) = key_rotation_result {
            tracing::error!(
                target: "whitenoise::event_processor::process_welcome::background",
                account = %account.pubkey.to_hex(),
                error = %e,
                "Failed to rotate key package"
            );
        }

        if let Err(e) = image_sync_result {
            tracing::warn!(
                target: "whitenoise::event_processor::process_welcome::background",
                error = %e,
                "Failed to sync group image cache"
            );
        }

        if let Err(e) = welcomer_user_result {
            tracing::error!(
                target: "whitenoise::event_processor::process_welcome::background",
                account = %account.pubkey.to_hex(),
                error = %e,
                "Failed to ensure welcomer user exists"
            );
        }

        // --- Step 4: self-update (only if subscriptions are live) ---
        //
        // The self-update advances the group epoch.  It runs only when
        // subscriptions are live (step 1 succeeded) AND after the catch-up
        // step has queued in-flight messages and waited for the event processor
        // to drain them (step 2).  This replaces the original unconditional
        // 5 s sleep with a structured sequence that is both faster and more
        // reliable.
        if subscription_ok
            && let Err(e) = Self::perform_self_update(whitenoise, account, group_id).await
        {
            tracing::error!(
                target: "whitenoise::event_processor::process_welcome::background",
                account = %account.pubkey.to_hex(),
                group = %hex::encode(group_id.as_slice()),
                error = %e,
                "Failed to perform post-welcome self-update"
            );
        }

        tracing::debug!(
            target: "whitenoise::event_processor::process_welcome::background",
            account = %account.pubkey.to_hex(),
            group = %hex::encode(group_id.as_slice()),
            "Completed post-welcome processing"
        );
    }

    /// Fetch recent group messages from relays and queue them for processing,
    /// then wait a bounded time for them to be handled before returning.
    ///
    /// The messages are queued through `event_sender` using the account's
    /// `{hash}_mls_messages` subscription ID so that the existing event
    /// processor pipeline handles them in the same way as live subscription
    /// events (deduplication, retry, etc.).
    ///
    /// After queuing, a short sleep (`CATCHUP_DRAIN_WAIT`) gives the event
    /// processor time to work through the queue before `perform_self_update`
    /// advances the epoch.  This is intentionally brief — the subscription is
    /// already live at this point, so any in-flight messages are either in the
    /// channel or will arrive within normal relay latency.
    ///
    /// This is a **best-effort** operation: failures are logged but never
    /// propagate to the caller.
    async fn catchup_group_messages(
        whitenoise: &Whitenoise,
        account: &Account,
        group_id: &GroupId,
    ) {
        // Derive the nostr_group_id (hex-encoded) and group relay URLs.
        let (nostr_group_id, relay_urls) =
            match Self::get_group_nostr_id_and_relays(whitenoise, account, group_id) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::event_processor::process_welcome::background",
                        account = %account.pubkey.to_hex(),
                        group = %hex::encode(group_id.as_slice()),
                        error = %e,
                        "Skipping catch-up: could not resolve group relay info"
                    );
                    return;
                }
            };

        let events = whitenoise
            .nostr
            .fetch_group_messages_catchup(&nostr_group_id, &relay_urls)
            .await;

        let count = events.len();
        tracing::debug!(
            target: "whitenoise::event_processor::process_welcome::background",
            account = %account.pubkey.to_hex(),
            group = %hex::encode(group_id.as_slice()),
            count,
            "Catch-up: queuing fetched group messages for processing"
        );

        // Queue events through the standard pipeline so deduplication,
        // retry logic, and MLS state management apply correctly.
        let pubkey_hash = whitenoise.nostr.create_pubkey_hash(&account.pubkey);
        let subscription_id = format!("{}_mls_messages", pubkey_hash);

        for event in events {
            use crate::types::ProcessableEvent;
            let processable =
                ProcessableEvent::new_nostr_event(event, Some(subscription_id.clone()));
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                whitenoise.event_sender.send(processable),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    // Channel closed — receiver is gone; no point continuing.
                    tracing::warn!(
                        target: "whitenoise::event_processor::process_welcome::background",
                        account = %account.pubkey.to_hex(),
                        subscription_id = %subscription_id,
                        error = %e,
                        "Catch-up: event_sender closed, stopping queue"
                    );
                    break;
                }
                Err(_elapsed) => {
                    // Channel full for >100 ms — drop this event and keep going
                    // so the join flow is not blocked indefinitely.
                    tracing::warn!(
                        target: "whitenoise::event_processor::process_welcome::background",
                        account = %account.pubkey.to_hex(),
                        subscription_id = %subscription_id,
                        "Catch-up: event_sender full, dropping catch-up event"
                    );
                }
            }
        }

        // Give the event processor time to drain the queued messages before
        // the self-update advances the epoch.  This replaces the original
        // unconditional 5 s sleep with a shorter, purposeful wait that only
        // runs when subscriptions are live.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    /// Returns the Nostr group ID (hex) and relay URLs for a group.
    fn get_group_nostr_id_and_relays(
        whitenoise: &Whitenoise,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<(String, Vec<RelayUrl>)> {
        let mdk = whitenoise.create_mdk_for_account(account.pubkey)?;
        let group = mdk
            .get_group(group_id)?
            .ok_or(WhitenoiseError::GroupNotFound)?;
        let nostr_group_id = hex::encode(group.nostr_group_id);
        let relay_urls = mdk.get_relays(group_id)?;
        Ok((nostr_group_id, relay_urls.into_iter().collect()))
    }

    /// Create GroupInformation for the welcome
    async fn create_group_info(
        whitenoise: &Whitenoise,
        group_id: &GroupId,
        group_name: &str,
    ) -> Result<()> {
        GroupInformation::create_for_group(whitenoise, group_id, None, group_name).await?;
        Ok(())
    }

    /// Set up Nostr subscriptions for group messages
    async fn setup_group_subscriptions(
        whitenoise: &Whitenoise,
        account: &Account,
        signer: Arc<dyn NostrSigner>,
    ) -> Result<()> {
        let (group_ids, group_relays) =
            Self::get_group_subscription_info(whitenoise, &account.pubkey)?;

        // Create relay records (idempotent)
        for relay in &group_relays {
            if let Err(e) = Relay::find_or_create_by_url(relay, &whitenoise.database).await {
                tracing::warn!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    "Failed to create relay record for {}: {}",
                    relay,
                    e
                );
            }
        }

        whitenoise
            .nostr
            .setup_group_messages_subscriptions_with_signer(
                account.pubkey,
                &group_relays,
                &group_ids,
                signer,
            )
            .await?;

        Ok(())
    }

    /// Handle key package rotation after welcome.
    ///
    /// Marks the consumed key package in the published_key_packages table,
    /// then deletes it from relays and publishes a fresh replacement.
    async fn rotate_key_package(
        whitenoise: &Whitenoise,
        account: &Account,
        key_package_event_id: Option<EventId>,
    ) -> Result<()> {
        let Some(kp_event_id) = key_package_event_id else {
            tracing::debug!(
                target: "whitenoise::event_processor::process_welcome::background",
                "No key package event id found in welcome event"
            );
            return Ok(());
        };

        // Mark the key package as consumed so the maintenance task knows
        // to clean up local key material after the quiet period.
        if let Err(e) = PublishedKeyPackage::mark_consumed(
            &account.pubkey,
            &kp_event_id.to_hex(),
            &whitenoise.database,
        )
        .await
        {
            tracing::warn!(
                target: "whitenoise::event_processor::process_welcome::background",
                "Failed to mark key package as consumed: {}",
                e
            );
        }

        // Publish new key package first so the account is never left with zero
        // key packages on relays. If this fails, the old one stays available.
        whitenoise.publish_key_package_for_account(account).await?;
        tracing::debug!(
            target: "whitenoise::event_processor::process_welcome::background",
            "Published new key package"
        );

        // Now delete the used key package. Failure here is non-fatal — the
        // scheduler will clean it up during routine maintenance.
        match whitenoise
            .delete_key_package_for_account(account, &kp_event_id, false)
            .await
        {
            Ok(true) => {
                tracing::debug!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    "Deleted used key package from relays"
                );
            }
            Ok(false) => {
                tracing::debug!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    "Key package already deleted, skipping"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    "Failed to delete used key package, scheduler will clean up: {}",
                    e
                );
            }
        }

        Ok(())
    }

    /// Sync group image cache if needed
    async fn sync_group_image(
        whitenoise: &Whitenoise,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<()> {
        whitenoise
            .sync_group_image_cache_if_needed(account, group_id)
            .await
    }

    async fn ensure_welcomer_user_exists(
        whitenoise: &Whitenoise,
        welcomer_pubkey: PublicKey,
    ) -> Result<()> {
        whitenoise
            .find_or_create_user_by_pubkey(&welcomer_pubkey, crate::UserSyncMode::Background)
            .await?;
        Ok(())
    }

    /// Perform MLS self-update after joining a group (MIP-02 requirement).
    ///
    /// Rotates the member's leaf node key material so the group no longer
    /// relies on the KeyPackage that was publicly available on relays.
    /// This is a security-critical operation for forward secrecy.
    ///
    /// Per MIP-03, the evolution event is published to relays (with retry)
    /// *before* merging the pending commit locally. This ensures we only
    /// advance local state after confirming the relay accepted the event.
    /// If all publish attempts fail, the pending commit is never merged and
    /// the group state remains unchanged.
    async fn perform_self_update(
        whitenoise: &Whitenoise,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<()> {
        let relay_urls = {
            let mdk = whitenoise.create_mdk_for_account(account.pubkey)?;
            Self::ensure_group_relays(&mdk, group_id)?
        };

        let evolution_event = {
            let mdk = whitenoise.create_mdk_for_account(account.pubkey)?;
            let update_result = mdk.self_update(group_id)?;
            update_result.evolution_event
        };

        whitenoise
            .publish_and_merge_commit(evolution_event, &account.pubkey, group_id, &relay_urls)
            .await?;

        tracing::info!(
            target: "whitenoise::event_processor::process_welcome::background",
            "Self-update completed for account {} in group {}",
            account.pubkey.to_hex(),
            hex::encode(group_id.as_slice())
        );

        Ok(())
    }

    /// Helper to get group subscription info (group IDs and relay URLs) for an account.
    fn get_group_subscription_info(
        whitenoise: &Whitenoise,
        pubkey: &PublicKey,
    ) -> Result<(Vec<String>, Vec<RelayUrl>)> {
        let mdk = whitenoise.create_mdk_for_account(*pubkey)?;
        let groups = mdk.get_groups()?;
        let mut group_relays_set = BTreeSet::new();
        let group_ids = groups
            .iter()
            .map(|g| hex::encode(g.nostr_group_id))
            .collect::<Vec<_>>();

        for group in &groups {
            let relays = mdk.get_relays(&group.mls_group_id)?;
            group_relays_set.extend(relays);
        }

        Ok((group_ids, group_relays_set.into_iter().collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::accounts_groups::AccountGroup;
    use crate::whitenoise::relays::Relay;
    use crate::whitenoise::test_utils::*;

    // Builds a real MLS Welcome rumor for `member_pubkey` by creating a group with `creator_account`
    async fn build_welcome_giftwrap(
        whitenoise: &Whitenoise,
        creator_account: &Account,
        member_pubkey: PublicKey,
    ) -> Event {
        // Fetch a real key package event for the member from relays
        let relays_urls = Relay::urls(
            &creator_account
                .key_package_relays(whitenoise)
                .await
                .unwrap(),
        );
        let key_pkg_event = whitenoise
            .nostr
            .fetch_user_key_package(member_pubkey, &relays_urls)
            .await
            .unwrap()
            .expect("member must have a published key package");

        // Create the group via mdk directly to obtain welcome rumor
        let mdk = whitenoise
            .create_mdk_for_account(creator_account.pubkey)
            .unwrap();
        let create_group_result = mdk
            .create_group(
                &creator_account.pubkey,
                vec![key_pkg_event],
                create_nostr_group_config_data(vec![creator_account.pubkey]),
            )
            .unwrap();

        let welcome_rumor = create_group_result
            .welcome_rumors
            .first()
            .expect("welcome rumor exists")
            .clone();

        // Use the creator's real keys as signer to build the giftwrap
        let creator_signer = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&creator_account.pubkey)
            .unwrap();

        EventBuilder::gift_wrap(&creator_signer, &member_pubkey, welcome_rumor, vec![])
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_handle_giftwrap_welcome_success() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create creator and one member account; setup publishes key packages and contacts
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        // Build a real MLS Welcome giftwrap addressed to the member
        let giftwrap_event =
            build_welcome_giftwrap(&whitenoise, &creator_account, member_account.pubkey).await;

        // Member should successfully process welcome
        let result = whitenoise
            .handle_giftwrap(&member_account, giftwrap_event)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_giftwrap_creates_account_group_synchronously() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create creator and one member account; setup publishes key packages and contacts
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        // Build a real MLS Welcome giftwrap addressed to the member
        let giftwrap_event =
            build_welcome_giftwrap(&whitenoise, &creator_account, member_account.pubkey).await;

        // Member processes the welcome
        let result = whitenoise
            .handle_giftwrap(&member_account, giftwrap_event)
            .await;
        assert!(result.is_ok());

        // CRITICAL: AccountGroup must exist immediately after handle_giftwrap returns
        // (not just after background task completes). This prevents race conditions
        // where Flutter polls groups() before the AccountGroup record exists.
        let mdk = whitenoise
            .create_mdk_for_account(member_account.pubkey)
            .unwrap();
        let groups = mdk.get_groups().unwrap();
        assert!(!groups.is_empty(), "Member should have at least one group");

        let group_id = &groups[0].mls_group_id;
        let account_group = AccountGroup::get(&whitenoise, &member_account.pubkey, group_id)
            .await
            .unwrap();

        assert!(
            account_group.is_some(),
            "AccountGroup must exist synchronously after handle_giftwrap"
        );

        let ag = account_group.unwrap();
        assert!(
            ag.is_pending(),
            "AccountGroup should be pending (user_confirmation = NULL)"
        );

        // Verify welcomer_pubkey is set to the creator's pubkey
        assert_eq!(
            ag.welcomer_pubkey,
            Some(creator_account.pubkey),
            "AccountGroup.welcomer_pubkey should be the group creator's pubkey"
        );
    }

    #[tokio::test]
    async fn test_handle_giftwrap_non_welcome_ok() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Build a non-welcome rumor and giftwrap it to the account
        let sender_keys = create_test_keys();
        let mut rumor = UnsignedEvent::new(
            sender_keys.public_key(), // Use sender's pubkey (must match seal signer)
            Timestamp::now(),
            Kind::TextNote,
            vec![],
            "not a welcome".to_string(),
        );
        rumor.ensure_id();

        let giftwrap_event = EventBuilder::gift_wrap(&sender_keys, &account.pubkey, rumor, vec![])
            .await
            .unwrap();

        let result = whitenoise.handle_giftwrap(&account, giftwrap_event).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);
    }

    #[tokio::test]
    async fn test_get_group_subscription_info_no_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // New account has no groups
        let result = Whitenoise::get_group_subscription_info(&whitenoise, &account.pubkey);
        assert!(result.is_ok());

        let (group_ids, relays) = result.unwrap();
        assert!(group_ids.is_empty());
        assert!(relays.is_empty());
    }

    #[tokio::test]
    async fn test_get_group_subscription_info_with_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create creator and member accounts
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        // Create a group
        let config = create_nostr_group_config_data(vec![creator_account.pubkey]);
        whitenoise
            .create_group(&creator_account, vec![member_pubkey], config, None)
            .await
            .unwrap();

        // Creator should now have one group with relays
        let result = Whitenoise::get_group_subscription_info(&whitenoise, &creator_account.pubkey);
        assert!(result.is_ok());

        let (group_ids, relays) = result.unwrap();
        assert_eq!(group_ids.len(), 1, "Creator should have one group");
        assert!(!relays.is_empty(), "Group should have relays");
    }

    #[tokio::test]
    async fn test_finalize_welcome_with_instance_completes() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = mdk_core::GroupId::from_slice(&[42; 32]);
        let group_name = "Test Group";
        let welcomer_pubkey = whitenoise.create_identity().await.unwrap().pubkey;

        // Pre-create AccountGroup to simulate synchronous creation in process_welcome
        AccountGroup::get_or_create(&whitenoise, &account.pubkey, &group_id, None)
            .await
            .unwrap();

        // Run finalize_welcome_with_instance - it should complete without panic
        // Some operations may fail (e.g., group not in MLS) but the function handles errors gracefully
        Whitenoise::finalize_welcome_with_instance(
            &whitenoise,
            &account,
            &group_id,
            group_name,
            None,
            welcomer_pubkey,
        )
        .await;

        // Verify AccountGroup still exists and is pending
        let account_group = AccountGroup::get(&whitenoise, &account.pubkey, &group_id)
            .await
            .unwrap();
        assert!(account_group.is_some(), "AccountGroup should exist");
        assert!(
            account_group.unwrap().is_pending(),
            "AccountGroup should still be pending"
        );
    }

    #[tokio::test]
    async fn test_finalize_welcome_with_instance_idempotent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = mdk_core::GroupId::from_slice(&[44; 32]);
        let group_name = "Idempotent Test Group";
        let welcomer_pubkey = whitenoise.create_identity().await.unwrap().pubkey;

        // Pre-create AccountGroup to simulate synchronous creation in process_welcome
        AccountGroup::get_or_create(&whitenoise, &account.pubkey, &group_id, None)
            .await
            .unwrap();

        // Run twice - should not panic
        Whitenoise::finalize_welcome_with_instance(
            &whitenoise,
            &account,
            &group_id,
            group_name,
            None,
            welcomer_pubkey,
        )
        .await;

        Whitenoise::finalize_welcome_with_instance(
            &whitenoise,
            &account,
            &group_id,
            group_name,
            None,
            welcomer_pubkey,
        )
        .await;

        // Should still have exactly one AccountGroup
        let visible = AccountGroup::visible_for_account(&whitenoise, &account.pubkey)
            .await
            .unwrap();
        let matching: Vec<_> = visible
            .iter()
            .filter(|ag| ag.mls_group_id == group_id)
            .collect();
        assert_eq!(matching.len(), 1, "Should have exactly one AccountGroup");
    }

    #[tokio::test]
    async fn test_self_update_after_welcome_advances_epoch() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create creator and one member account
        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_account = members[0].0.clone();

        // Build and process a real MLS Welcome
        let giftwrap_event =
            build_welcome_giftwrap(&whitenoise, &creator_account, member_account.pubkey).await;
        whitenoise
            .handle_giftwrap(&member_account, giftwrap_event)
            .await
            .unwrap();

        // Get the group and record the epoch after welcome acceptance (before background tasks)
        let mdk = whitenoise
            .create_mdk_for_account(member_account.pubkey)
            .unwrap();
        let groups = mdk.get_groups().unwrap();
        assert!(!groups.is_empty(), "Member should have at least one group");
        let group = &groups[0];
        let group_id = group.mls_group_id.clone();
        let epoch_after_welcome = group.epoch;

        // Run finalize_welcome_with_instance which includes perform_self_update
        Whitenoise::finalize_welcome_with_instance(
            &whitenoise,
            &member_account,
            &group_id,
            &group.name,
            None,
            creator_account.pubkey,
        )
        .await;

        // Re-read the group and verify epoch advanced
        let mdk = whitenoise
            .create_mdk_for_account(member_account.pubkey)
            .unwrap();
        let updated_group = mdk.get_group(&group_id).unwrap().expect("group must exist");
        assert_eq!(
            updated_group.epoch,
            epoch_after_welcome + 1,
            "Epoch should advance by 1 after self-update (was {}, now {})",
            epoch_after_welcome,
            updated_group.epoch
        );
    }

    /// When the account's signing key is not in the secrets store,
    /// `finalize_welcome_with_instance` must return early without panicking.
    /// This covers the signer-not-found early-return path (lines 238-245).
    #[tokio::test]
    async fn test_finalize_welcome_no_signer_returns_early() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let group_id = mdk_core::GroupId::from_slice(&[99; 32]);
        let welcomer_pubkey = whitenoise.create_identity().await.unwrap().pubkey;

        // Pre-create AccountGroup so the function has a record to work with
        AccountGroup::get_or_create(&whitenoise, &account.pubkey, &group_id, None)
            .await
            .unwrap();

        // Remove the private key so get_signer_for_account returns an error
        whitenoise
            .secrets_store
            .remove_private_key_for_pubkey(&account.pubkey)
            .unwrap();

        // Should complete without panic despite missing signer
        Whitenoise::finalize_welcome_with_instance(
            &whitenoise,
            &account,
            &group_id,
            "Test Group",
            None,
            welcomer_pubkey,
        )
        .await;

        // AccountGroup must still exist (early return does not destroy it)
        let ag = AccountGroup::get(&whitenoise, &account.pubkey, &group_id)
            .await
            .unwrap();
        assert!(ag.is_some(), "AccountGroup must survive an early return");
    }

    /// `get_group_nostr_id_and_relays` must return the hex-encoded Nostr group
    /// ID and at least one relay URL when called with a real MLS group.
    /// This covers the happy path of the helper (lines 451-457).
    #[tokio::test]
    async fn test_get_group_nostr_id_and_relays_with_real_group() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        // Create a real group so the MLS state exists
        let config = create_nostr_group_config_data(vec![creator_account.pubkey]);
        whitenoise
            .create_group(&creator_account, vec![member_pubkey], config, None)
            .await
            .unwrap();

        let mdk = whitenoise
            .create_mdk_for_account(creator_account.pubkey)
            .unwrap();
        let groups = mdk.get_groups().unwrap();
        assert!(!groups.is_empty(), "Creator should have one group");
        let group_id = &groups[0].mls_group_id;

        let result =
            Whitenoise::get_group_nostr_id_and_relays(&whitenoise, &creator_account, group_id);

        assert!(result.is_ok(), "Should resolve group relay info");
        let (nostr_group_id, relay_urls) = result.unwrap();
        assert_eq!(
            nostr_group_id.len(),
            64,
            "Nostr group ID should be 32 bytes hex-encoded (64 chars)"
        );
        assert!(
            !relay_urls.is_empty(),
            "Group should have at least one relay"
        );
    }

    /// `get_group_nostr_id_and_relays` must return an error when the group does
    /// not exist in the MLS state — exercising the `GroupNotFound` error path.
    #[tokio::test]
    async fn test_get_group_nostr_id_and_relays_group_not_found() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let fake_group_id = mdk_core::GroupId::from_slice(&[77; 32]);

        let result =
            Whitenoise::get_group_nostr_id_and_relays(&whitenoise, &account, &fake_group_id);

        assert!(
            result.is_err(),
            "Should return error for non-existent group"
        );
    }

    /// `catchup_group_messages` must return early without panicking when the
    /// group does not exist in MLS state.  This exercises the error branch in
    /// `get_group_nostr_id_and_relays` (GroupNotFound → log + return).
    #[tokio::test]
    async fn test_catchup_group_messages_unknown_group_returns_early() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        // Use a group ID that was never created in MLS state.
        let fake_group_id = mdk_core::GroupId::from_slice(&[55; 32]);

        // Must complete without panic despite the group not existing.
        Whitenoise::catchup_group_messages(&whitenoise, &account, &fake_group_id).await;
    }

    /// `catchup_group_messages` must complete without panic when called with a
    /// real group.  No events are returned (no live relay) but the function
    /// must traverse the full path: resolve group info → fetch (empty) →
    /// skip queuing loop → drain sleep.  Covers lines 384-442.
    #[tokio::test]
    async fn test_catchup_group_messages_real_group_no_events() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let member_pubkey = members[0].0.pubkey;

        let config = create_nostr_group_config_data(vec![creator_account.pubkey]);
        whitenoise
            .create_group(&creator_account, vec![member_pubkey], config, None)
            .await
            .unwrap();

        let mdk = whitenoise
            .create_mdk_for_account(creator_account.pubkey)
            .unwrap();
        let groups = mdk.get_groups().unwrap();
        let group_id = &groups[0].mls_group_id;

        // Should complete without panic; relay returns no events in test env
        Whitenoise::catchup_group_messages(&whitenoise, &creator_account, group_id).await;
    }
}
