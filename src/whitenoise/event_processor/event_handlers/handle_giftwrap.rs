use nostr_sdk::prelude::*;

use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    error::{Result, WhitenoiseError},
};

impl Whitenoise {
    pub async fn handle_giftwrap(&self, account: &Account, event: Event) -> Result<()> {
        tracing::info!(
            target: "whitenoise::event_handlers::handle_giftwrap",
            "Giftwrap received for account: {} - processing not yet implemented",
            account.pubkey.to_hex()
        );

        let signer = self.get_signer_for_account(account)?;

        let unwrapped = extract_rumor(&signer, &event).await.map_err(|e| {
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
        // Process the welcome message - lock scope is minimal
        let group_id = {
            let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
            let welcome = mdk
                .process_welcome(&event.id, &rumor)
                .map_err(WhitenoiseError::MdkCoreError)?;
            tracing::debug!(target: "whitenoise::event_processor::process_welcome", "Processed welcome event");
            welcome.mls_group_id
        }; // mdk lock released here

        // After processing welcome, proactively cache the group image if it has one
        // This ensures the image is ready when the UI displays the group
        // Spawn as background task to avoid blocking event processing
        Whitenoise::background_sync_group_image_cache_if_needed(account, &group_id);

        let key_package_event_id: Option<EventId> = rumor
            .tags
            .iter()
            .find(|tag| {
                tag.kind() == TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::E))
            })
            .and_then(|tag| tag.content())
            .and_then(|content| EventId::parse(content).ok());

        if let Some(key_package_event_id) = key_package_event_id {
            let deleted = self
                .delete_key_package_for_account(
                    account,
                    &key_package_event_id,
                    false, // For now we don't want to delete the key packages from MLS storage
                )
                .await?;

            if deleted {
                tracing::debug!(target: "whitenoise::event_processor::process_welcome", "Deleted used key package from relays");
                self.publish_key_package_for_account(account).await?;
                tracing::debug!(target: "whitenoise::event_processor::process_welcome", "Published new key package");
            } else {
                tracing::debug!(target: "whitenoise::event_processor::process_welcome", "Key package already deleted, skipping publish");
            }
        } else {
            tracing::warn!(target: "whitenoise::event_processor::process_welcome", "No key package event id found in welcome event");
        }

        Ok(())
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

        // Use the creator's signer to build the giftwrap
        let creator_signer = whitenoise.get_signer_for_account(creator_account).unwrap();

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
}
