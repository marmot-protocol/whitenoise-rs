use std::collections::BTreeSet;

use nostr_sdk::prelude::*;

use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    accounts_groups::AccountGroup,
    error::{Result, WhitenoiseError},
    group_information::GroupInformation,
    relays::Relay,
};

impl Whitenoise {
    pub async fn handle_giftwrap(&self, account: &Account, event: Event) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::event_handlers::handle_giftwrap",
            "Giftwrap received for account: {}",
            account.pubkey.to_hex()
        );

        let keys = self
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)?;

        let unwrapped = extract_rumor(&keys, &event).await.map_err(|e| {
            WhitenoiseError::Configuration(format!("Failed to decrypt giftwrap: {}", e))
        })?;

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
        // Critical synchronous section: process and auto-accept MLS welcome
        // This must complete for MLS state consistency
        let (group_id, group_name) = {
            let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;

            let welcome = mdk
                .process_welcome(&event.id, &rumor)
                .map_err(WhitenoiseError::MdkCoreError)?;
            tracing::debug!(target: "whitenoise::event_processor::process_welcome", "Processed welcome event");

            // Auto-accept the welcome to finalize MLS membership
            mdk.accept_welcome(&welcome)
                .map_err(WhitenoiseError::MdkCoreError)?;
            tracing::debug!(target: "whitenoise::event_processor::process_welcome", "Auto-accepted welcome, MLS membership finalized");

            (welcome.mls_group_id, welcome.group_name)
        };

        // Extract key package event ID for rotation
        let key_package_event_id: Option<EventId> = rumor
            .tags
            .iter()
            .find(|tag| {
                tag.kind() == TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E))
            })
            .and_then(|tag| tag.content())
            .and_then(|content| EventId::parse(content).ok());

        // Clone data needed for background task
        let account_clone = account.clone();
        let data_dir = self.config.data_dir.clone();

        // Spawn background task for remaining operations (DB writes, network calls)
        // All operations are idempotent and failures are logged but don't stop other operations
        tokio::spawn(async move {
            let whitenoise = match Whitenoise::get_instance() {
                Ok(wn) => wn,
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::event_processor::process_welcome::background",
                        "Failed to get Whitenoise instance: {}",
                        e
                    );
                    return;
                }
            };

            // 1. Create AccountGroup record (user_confirmation = NULL for pending at app level)
            match AccountGroup::get_or_create(whitenoise, &account_clone.pubkey, &group_id).await {
                Ok((ag, created)) => {
                    tracing::debug!(
                        target: "whitenoise::event_processor::process_welcome::background",
                        "AccountGroup ready for account {} and group {} (created: {}, confirmation: {:?})",
                        account_clone.pubkey.to_hex(),
                        hex::encode(group_id.as_slice()),
                        created,
                        ag.user_confirmation
                    );
                }
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::event_processor::process_welcome::background",
                        "Failed to create AccountGroup for account {} and group {}: {}",
                        account_clone.pubkey.to_hex(),
                        hex::encode(group_id.as_slice()),
                        e
                    );
                }
            }

            // 2. Create GroupInformation so the group is visible in UI
            if let Err(e) =
                GroupInformation::create_for_group(whitenoise, &group_id, None, &group_name).await
            {
                tracing::error!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    "Failed to create GroupInformation for group {}: {}",
                    hex::encode(group_id.as_slice()),
                    e
                );
            }

            // 3. Set up subscriptions for group messages
            let keys = match whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&account_clone.pubkey)
            {
                Ok(k) => k,
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::event_processor::process_welcome::background",
                        "Failed to get keys for account {}: {}",
                        account_clone.pubkey.to_hex(),
                        e
                    );
                    return;
                }
            };

            let (group_ids, group_relays) =
                match Self::get_group_subscription_info(&data_dir, &account_clone.pubkey) {
                    Ok(info) => info,
                    Err(e) => {
                        tracing::error!(
                            target: "whitenoise::event_processor::process_welcome::background",
                            "Failed to get group subscription info for account {}: {}",
                            account_clone.pubkey.to_hex(),
                            e
                        );
                        return;
                    }
                };

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

            if let Err(e) = whitenoise
                .nostr
                .setup_group_messages_subscriptions_with_signer(
                    account_clone.pubkey,
                    &group_relays,
                    &group_ids,
                    keys,
                )
                .await
            {
                tracing::error!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    "Failed to setup group subscriptions for account {}: {}",
                    account_clone.pubkey.to_hex(),
                    e
                );
            }

            // 4. Handle key package rotation
            if let Some(kp_event_id) = key_package_event_id {
                match whitenoise
                    .delete_key_package_for_account(&account_clone, &kp_event_id, false)
                    .await
                {
                    Ok(true) => {
                        tracing::debug!(target: "whitenoise::event_processor::process_welcome::background", "Deleted used key package from relays");
                        if let Err(e) = whitenoise
                            .publish_key_package_for_account(&account_clone)
                            .await
                        {
                            tracing::error!(
                                target: "whitenoise::event_processor::process_welcome::background",
                                "Failed to publish new key package for account {}: {}",
                                account_clone.pubkey.to_hex(),
                                e
                            );
                        } else {
                            tracing::debug!(target: "whitenoise::event_processor::process_welcome::background", "Published new key package");
                        }
                    }
                    Ok(false) => {
                        tracing::debug!(target: "whitenoise::event_processor::process_welcome::background", "Key package already deleted, skipping publish");
                    }
                    Err(e) => {
                        tracing::error!(
                            target: "whitenoise::event_processor::process_welcome::background",
                            "Failed to delete key package for account {}: {}",
                            account_clone.pubkey.to_hex(),
                            e
                        );
                    }
                }
            } else {
                tracing::debug!(target: "whitenoise::event_processor::process_welcome::background", "No key package event id found in welcome event");
            }

            // 5. Sync group image cache
            if let Err(e) = whitenoise
                .sync_group_image_cache_if_needed(&account_clone, &group_id)
                .await
            {
                tracing::warn!(
                    target: "whitenoise::event_processor::process_welcome::background",
                    "Failed to sync group image cache: {}",
                    e
                );
            }

            tracing::debug!(
                target: "whitenoise::event_processor::process_welcome::background",
                "Completed post-welcome processing for account {} and group {}",
                account_clone.pubkey.to_hex(),
                hex::encode(group_id.as_slice())
            );
        });

        Ok(())
    }

    /// Helper to get group subscription info (group IDs and relay URLs) for an account.
    fn get_group_subscription_info(
        data_dir: &std::path::Path,
        pubkey: &PublicKey,
    ) -> Result<(Vec<String>, Vec<RelayUrl>)> {
        let mdk = Account::create_mdk(*pubkey, data_dir)?;
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
        let mdk = Account::create_mdk(creator_account.pubkey, &whitenoise.config.data_dir).unwrap();
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
    async fn test_handle_giftwrap_non_welcome_ok() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Build a non-welcome rumor and giftwrap it to the account
        let mut rumor = UnsignedEvent::new(
            account.pubkey,
            Timestamp::now(),
            Kind::TextNote,
            vec![],
            "not a welcome".to_string(),
        );
        rumor.ensure_id();

        // Any signer works; encryption targets receiver's pubkey
        let sender_keys = create_test_keys();
        let giftwrap_event = EventBuilder::gift_wrap(&sender_keys, &account.pubkey, rumor, vec![])
            .await
            .unwrap();

        let result = whitenoise.handle_giftwrap(&account, giftwrap_event).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_get_group_subscription_info_no_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // New account has no groups
        let result =
            Whitenoise::get_group_subscription_info(&whitenoise.config.data_dir, &account.pubkey);
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
        let result = Whitenoise::get_group_subscription_info(
            &whitenoise.config.data_dir,
            &creator_account.pubkey,
        );
        assert!(result.is_ok());

        let (group_ids, relays) = result.unwrap();
        assert_eq!(group_ids.len(), 1, "Creator should have one group");
        assert!(!relays.is_empty(), "Group should have relays");
    }
}
