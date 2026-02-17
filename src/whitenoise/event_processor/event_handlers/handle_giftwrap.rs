use std::collections::BTreeSet;

use chrono::Utc;
use mdk_core::GroupId;
use nostr_sdk::prelude::*;

use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    accounts_groups::AccountGroup,
    chat_list_streaming::ChatListUpdateTrigger,
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

        // Extract key package event ID for rotation
        let key_package_event_id: Option<EventId> = rumor
            .tags
            .iter()
            .find(|tag| {
                tag.kind() == TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E))
            })
            .and_then(|tag| tag.content())
            .and_then(|content| EventId::parse(content).ok());

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
    pub(crate) async fn finalize_welcome_with_instance(
        whitenoise: &Whitenoise,
        account: &Account,
        group_id: &GroupId,
        group_name: &str,
        key_package_event_id: Option<EventId>,
        welcomer_pubkey: PublicKey,
    ) {
        // Get keys early - needed for subscriptions
        let keys = match whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)
        {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    "Failed to get keys for account {}: {}",
                    account.pubkey.to_hex(),
                    e
                );
                return;
            }
        };

        // Run independent operations concurrently
        let (
            group_info_result,
            subscription_result,
            key_rotation_result,
            image_sync_result,
            welcomer_user_result,
        ) = tokio::join!(
            Self::create_group_info(whitenoise, group_id, group_name),
            Self::setup_group_subscriptions(whitenoise, account, keys),
            Self::rotate_key_package(whitenoise, account, key_package_event_id),
            Self::sync_group_image(whitenoise, account, group_id),
            Self::ensure_welcomer_user_exists(whitenoise, welcomer_pubkey),
        );

        // Log any errors (operations are independent, so we log all failures)
        if let Err(e) = group_info_result {
            tracing::error!(
                target: "whitenoise::event_processor::process_welcome::background",
                "Failed to create GroupInformation for group {}: {}",
                hex::encode(group_id.as_slice()),
                e
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

        if let Err(e) = subscription_result {
            tracing::error!(
                target: "whitenoise::event_processor::process_welcome::background",
                "Failed to setup subscriptions for account {}: {}",
                account.pubkey.to_hex(),
                e
            );
        }

        if let Err(e) = key_rotation_result {
            tracing::error!(
                target: "whitenoise::event_processor::process_welcome::background",
                "Failed to rotate key package for account {}: {}",
                account.pubkey.to_hex(),
                e
            );
        }

        if let Err(e) = image_sync_result {
            tracing::warn!(
                target: "whitenoise::event_processor::process_welcome::background",
                "Failed to sync group image cache: {}",
                e
            );
        }

        if let Err(e) = welcomer_user_result {
            tracing::error!(
                target: "whitenoise::event_processor::process_welcome::background",
                "Failed to ensure welcomer user exists: {}",
                e
            );
        }

        tracing::debug!(
            target: "whitenoise::event_processor::process_welcome::background",
            "Completed post-welcome processing for account {} and group {}",
            account.pubkey.to_hex(),
            hex::encode(group_id.as_slice())
        );
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
        keys: Keys,
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
                keys,
            )
            .await?;

        Ok(())
    }

    /// Handle key package rotation after welcome
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
        // (not just after background task completes). This prevents race condition
        // where Flutter polls groups() and triggers lazy migration before AccountGroup exists.
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
}
