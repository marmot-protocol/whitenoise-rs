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
            match self
                .delete_key_package_for_account(
                    account,
                    &key_package_event_id,
                    false, // For now we don't want to delete the key packages from MLS storage
                )
                .await
            {
                Ok(true) => {
                    tracing::debug!(
                        target: "whitenoise::event_processor::process_welcome",
                        "Deleted used key package from relays"
                    );
                }
                Ok(false) => {
                    tracing::warn!(
                        target: "whitenoise::event_processor::process_welcome",
                        "Key package event {key_package_event_id} not found on relays; publishing replacement regardless"
                    );
                }
                Err(err @ WhitenoiseError::AccountMissingKeyPackageRelays) => {
                    return Err(err);
                }
                Err(err) => {
                    tracing::warn!(
                        target: "whitenoise::event_processor::process_welcome",
                        "Failed to delete key package {key_package_event_id}: {err}"
                    );
                }
            }
        } else {
            tracing::warn!(
                target: "whitenoise::event_processor::process_welcome",
                "No key package event id found in welcome event; publishing replacement regardless"
            );
        }

        self.publish_key_package_for_account(account).await?;
        tracing::debug!(
            target: "whitenoise::event_processor::process_welcome",
            "Published new key package after processing welcome"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::{relays::Relay, test_utils::*};
    use nostr_sdk::Url;
    use tokio::time::{Duration, sleep};

    async fn relays_running() -> bool {
        let relay_urls = ["ws://localhost:8080", "ws://localhost:7777"];
        for relay in relay_urls {
            if let Ok(url) = RelayUrl::parse(relay) {
                let parsed_url: &Url = (&url).into();
                // RelayUrl no longer exposes host/port helpers; derive them from the underlying Url.
                let host = match parsed_url.host_str() {
                    Some(host) => host,
                    None => return false,
                };
                let port = match parsed_url.port_or_known_default() {
                    Some(port) => port,
                    None => return false,
                };
                let addr = format!("{host}:{port}");
                if tokio::time::timeout(
                    Duration::from_millis(200),
                    tokio::net::TcpStream::connect(&addr),
                )
                .await
                .is_err()
                {
                    return false;
                }
            } else {
                return false;
            }
        }
        true
    }

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
        if !relays_running().await {
            eprintln!("Skipping test_handle_giftwrap_welcome_success: relays not running");
            return;
        }
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
        if !relays_running().await {
            eprintln!("Skipping test_handle_giftwrap_non_welcome_ok: relays not running");
            return;
        }
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
    async fn test_handle_giftwrap_welcome_republishes_missing_keypackage() {
        if !relays_running().await {
            eprintln!(
                "Skipping test_handle_giftwrap_welcome_republishes_missing_keypackage: relays not running"
            );
            return;
        }
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let creator_account = whitenoise.create_identity().await.unwrap();
        let members = setup_multiple_test_accounts(&whitenoise, 1).await;
        let (member_account, member_keys) = (&members[0].0, &members[0].1);

        let relays = Relay::urls(
            &member_account
                .key_package_relays(&whitenoise)
                .await
                .unwrap(),
        );

        // Ensure the member currently has a published key package and delete it from relays
        if let Some(existing_key_package) = whitenoise
            .nostr
            .fetch_user_key_package(member_account.pubkey, &relays)
            .await
            .unwrap()
        {
            whitenoise
                .nostr
                .publish_event_deletion_with_signer(
                    &existing_key_package.id,
                    &relays,
                    member_keys.clone(),
                )
                .await
                .unwrap();
            // Give relays a moment to apply the deletion before we proceed
            sleep(Duration::from_millis(50)).await;
        }

        let missing_before = whitenoise
            .nostr
            .fetch_user_key_package(member_account.pubkey, &relays)
            .await
            .unwrap();
        assert!(
            missing_before.is_none(),
            "Key package should be missing prior to processing welcome"
        );

        let giftwrap_event =
            build_welcome_giftwrap(&whitenoise, &creator_account, member_account.pubkey).await;

        whitenoise
            .handle_giftwrap(member_account, giftwrap_event)
            .await
            .unwrap();

        let new_key_package = whitenoise
            .nostr
            .fetch_user_key_package(member_account.pubkey, &relays)
            .await
            .unwrap();
        assert!(
            new_key_package.is_some(),
            "Key package should be republished after processing welcome"
        );
    }
}
