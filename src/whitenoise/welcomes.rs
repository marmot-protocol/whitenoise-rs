use mdk_core::prelude::*;
use nostr_sdk::prelude::*;

use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    error::{Result, WhitenoiseError},
};

impl Whitenoise {
    /// Finds a specific welcome message by its event ID for a given public key.
    ///
    /// This method retrieves a welcome message that was previously received and stored
    /// in the nostr-mls system. Welcome messages are used to invite users to join
    /// MLS groups in the Nostr ecosystem.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The public key of the account to search welcomes for
    /// * `welcome_event_id` - The event ID of the welcome message to find (as a hex string)
    pub async fn find_welcome_by_event_id(
        &self,
        pubkey: &PublicKey,
        welcome_event_id: String,
    ) -> Result<welcome_types::Welcome> {
        let welcome_event_id = EventId::parse(&welcome_event_id).map_err(|_e| {
            WhitenoiseError::InvalidEvent("Couldn't parse welcome event ID".to_string())
        })?;
        let account = Account::find_by_pubkey(pubkey, &self.database).await?;
        let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
        let welcome = mdk
            .get_welcome(&welcome_event_id)?
            .ok_or(WhitenoiseError::WelcomeNotFound)?;
        Ok(welcome)
    }

    /// Retrieves all pending welcome messages for a given public key.
    ///
    /// This method returns a list of all welcome messages that have been received
    /// but not yet accepted or declined by the user. Pending welcomes represent
    /// group invitations that are waiting for the user's response.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The public key of the account to get pending welcomes for
    pub async fn pending_welcomes(
        &self,
        pubkey: &PublicKey,
    ) -> Result<Vec<welcome_types::Welcome>> {
        let account = Account::find_by_pubkey(pubkey, &self.database).await?;

        let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
        let welcomes = mdk.get_pending_welcomes()?;
        Ok(welcomes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::*;

    #[tokio::test]
    #[ignore]
    async fn test_receive_welcomes() {
        let whitenoise = test_get_whitenoise().await;
        let (creator_account, _creator_keys) = setup_login_account(whitenoise).await;

        // Setup member accounts
        let member_accounts = setup_multiple_test_accounts(whitenoise, 2).await;
        let member_pubkeys: Vec<PublicKey> =
            member_accounts.iter().map(|(acc, _)| acc.pubkey).collect();

        // Setup admin accounts (creator + one member as admin)
        let admin_pubkeys = vec![creator_account.pubkey, member_pubkeys[0]];
        let config = create_nostr_group_config_data(admin_pubkeys.clone());

        let group = whitenoise
            .create_group(&creator_account, member_pubkeys.clone(), config, None)
            .await;
        assert!(group.is_ok());
        let result1 = whitenoise
            .pending_welcomes(&creator_account.pubkey)
            .await
            .unwrap();
        assert!(result1.is_empty()); // creator should not receive welcome messages
        whitenoise.logout(&creator_account.pubkey).await.unwrap();

        let admin_key = &member_accounts[0].1;
        let regular_key = &member_accounts[1].1;

        tracing::info!("Logging into account {}", admin_key.public_key.to_hex());
        let account = whitenoise
            .login(admin_key.secret_key().to_secret_hex())
            .await
            .unwrap();
        // Give some time for the event processor to process welcome messages
        // sleep(Duration::from_secs(3));
        let result = whitenoise.pending_welcomes(&account.pubkey).await.unwrap();
        assert!(!result.is_empty(), "{:?}", result);
        whitenoise.logout(&admin_key.public_key).await.unwrap();

        tracing::info!("Logging into account {}", regular_key.public_key.to_hex());
        let account = whitenoise
            .login(regular_key.secret_key().to_secret_hex())
            .await
            .unwrap();
        // Give some time for the event processor to process welcome messages
        let result = whitenoise.pending_welcomes(&account.pubkey).await.unwrap();
        assert!(!result.is_empty());
    }

    #[tokio::test]
    async fn test_find_welcome_by_event_id_invalid_event_id() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        // Invalid event ID should return error
        let result = whitenoise
            .find_welcome_by_event_id(&account.pubkey, "invalid_event_id".to_string())
            .await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WhitenoiseError::InvalidEvent(_)
        ));
    }

    #[tokio::test]
    async fn test_pending_welcomes_empty_for_new_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let welcomes = whitenoise.pending_welcomes(&account.pubkey).await.unwrap();

        assert!(welcomes.is_empty());
    }
}
