use nostr_sdk::prelude::*;

use super::Account;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::error::Result;

impl Whitenoise {
    /// Creates a new identity (account) for the user.
    ///
    /// This method generates a new keypair, sets up the account with default relay lists,
    /// and fully configures the account for use in Whitenoise.
    pub async fn create_identity(&self) -> Result<Account> {
        let keys = Keys::generate();
        tracing::debug!(target: "whitenoise::accounts", "Generated new keypair: {}", keys.public_key().to_hex());

        let mut account = self.create_base_account_with_private_key(&keys).await?;
        tracing::debug!(target: "whitenoise::accounts", "Keys stored in secret store and account saved to database");

        let user = account.user(&self.database).await?;

        let relays = self
            .setup_relays_for_new_account(&mut account, &user)
            .await?;
        tracing::debug!(target: "whitenoise::accounts", "Relays setup");

        self.activate_account(&account, &user, true, &relays, &relays, &relays)
            .await?;
        tracing::debug!(target: "whitenoise::accounts", "Account persisted and activated");

        tracing::debug!(target: "whitenoise::accounts", "Successfully created new identity: {}", account.pubkey.to_hex());
        Ok(account)
    }

    /// Logs in an existing user using a private key (nsec or hex format).
    ///
    /// This method parses the private key, checks if the account exists locally,
    /// and sets up the account for use. If the account doesn't exist locally,
    /// it treats it as an existing account and fetches data from the network.
    ///
    /// # Arguments
    ///
    /// * `nsec_or_hex_privkey` - The user's private key as a nsec string or hex-encoded string.
    pub async fn login(&self, nsec_or_hex_privkey: String) -> Result<Account> {
        let keys = Keys::parse(&nsec_or_hex_privkey)?;
        let pubkey = keys.public_key();
        tracing::debug!(target: "whitenoise::accounts", "Logging in with pubkey: {}", pubkey.to_hex());

        // If this account is already logged in, return it as-is. The session
        // (relay connections, subscriptions, cancellation channel, background
        // tasks) was set up during the original login and is still active —
        // re-running activate_account would create duplicate subscriptions and
        // needlessly kill in-progress background tasks.
        if let Ok(existing) = Account::find_by_pubkey(&pubkey, &self.database).await {
            tracing::debug!(
                target: "whitenoise::accounts",
                "Account {} is already logged in, returning existing account",
                pubkey.to_hex()
            );
            return Ok(existing);
        }

        let mut account = self.create_base_account_with_private_key(&keys).await?;
        tracing::debug!(target: "whitenoise::accounts", "Keys stored in secret store and account saved to database");

        // Always check for existing relay lists when logging in, even if the user is
        // newly created in our database, because the keypair might already exist in
        // the Nostr ecosystem with published relay lists from other apps
        let (nip65_relays, inbox_relays, key_package_relays) =
            self.setup_relays_for_existing_account(&mut account).await?;
        tracing::debug!(target: "whitenoise::accounts", "Relays setup");

        let user = account.user(&self.database).await?;
        self.activate_account(
            &account,
            &user,
            false,
            &nip65_relays,
            &inbox_relays,
            &key_package_relays,
        )
        .await?;
        tracing::debug!(target: "whitenoise::accounts", "Account persisted and activated");

        tracing::debug!(target: "whitenoise::accounts", "Successfully logged in: {}", account.pubkey.to_hex());
        Ok(account)
    }

    /// Logs in using an external signer (e.g., Amber via NIP-55).
    ///
    /// This method creates an account for the given public key without storing any
    /// private key locally. It performs the complete setup:
    ///
    /// 1. Creates/updates the account for the given public key
    /// 2. Sets up relays (fetches existing from network or uses defaults)
    /// 3. Registers the external signer for ongoing use (e.g., giftwrap decryption)
    /// 4. Publishes relay lists if using defaults (using the signer)
    /// 5. Publishes the MLS key package (using the signer)
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The user's public key obtained from the external signer.
    /// * `signer` - The external signer to use for signing operations. Must implement `Clone`.
    pub async fn login_with_external_signer(
        &self,
        pubkey: PublicKey,
        signer: impl NostrSigner + Clone + 'static,
    ) -> Result<Account> {
        tracing::debug!(
            target: "whitenoise::accounts",
            "Logging in with external signer, pubkey: {}",
            pubkey.to_hex()
        );

        // If this account is already logged in, return it as-is (see comment
        // in login() for rationale on not re-activating).
        if let Ok(existing) = Account::find_by_pubkey(&pubkey, &self.database).await {
            tracing::debug!(
                target: "whitenoise::accounts",
                "Account {} is already logged in, returning existing account",
                pubkey.to_hex()
            );
            return Ok(existing);
        }

        self.validate_signer_pubkey(&pubkey, &signer).await?;

        let (account, relay_setup) = self.setup_external_signer_account(pubkey).await?;

        // Register the signer before activating the account so that subscription
        // setup can use it for NIP-42 AUTH on relays that require it.
        self.insert_external_signer(pubkey, signer.clone()).await?;

        let user = account.user(&self.database).await?;
        self.activate_account_without_publishing(
            &account,
            &user,
            &relay_setup.nip65_relays,
            &relay_setup.inbox_relays,
            &relay_setup.key_package_relays,
        )
        .await?;

        self.publish_relay_lists_with_signer(&relay_setup, signer.clone())
            .await?;

        tracing::debug!(
            target: "whitenoise::accounts",
            "Publishing MLS key package"
        );
        self.publish_key_package_for_account_with_signer(&account, signer)
            .await?;

        tracing::info!(
            target: "whitenoise::accounts",
            "Successfully logged in with external signer: {}",
            account.pubkey.to_hex()
        );

        Ok(account)
    }

    /// Logs out the user associated with the given account.
    ///
    /// This method performs the following steps:
    /// - Removes the account from the database.
    /// - Removes the private key from the secret store (for local accounts only).
    /// - Updates the active account if the logged-out account was active.
    /// - Removes the account from the in-memory accounts list.
    ///
    /// - NB: This method does not remove the MLS database for the account. If the user logs back in, the MLS database will be re-initialized and used again.
    ///
    /// # Arguments
    ///
    /// * `account` - The account to log out.
    pub async fn logout(&self, pubkey: &PublicKey) -> Result<()> {
        let account = Account::find_by_pubkey(pubkey, &self.database).await?;

        // Cancel any running background tasks (contact list user fetches, etc.)
        // before tearing down subscriptions and relay connections.
        if let Some((_, cancel_tx)) = self.background_task_cancellation.remove(pubkey) {
            let _ = cancel_tx.send(true);
        }

        // Unsubscribe from account-specific subscriptions before logout
        if let Err(e) = self.nostr.unsubscribe_account_subscriptions(pubkey).await {
            tracing::warn!(
                target: "whitenoise::accounts",
                "Failed to unsubscribe from account subscriptions for {}: {}",
                pubkey, e
            );
            // Don't fail logout if unsubscribe fails
        }

        // Delete the account from the database
        account.delete(&self.database).await?;

        // Remove the private key from the secret store
        // For local accounts this is required; for external accounts this is best-effort cleanup
        let result = self.secrets_store.remove_private_key_for_pubkey(pubkey);
        match (account.has_local_key(), result) {
            (true, Err(e)) => return Err(e.into()), // Local account MUST have key
            (false, Err(e)) => tracing::debug!("Expected - no key for external account: {}", e),
            _ => {}
        }
        Ok(())
    }

    /// Returns the total number of accounts stored in the database.
    ///
    /// This method queries the database to count all accounts that have been created
    /// or imported into the Whitenoise instance. This includes both active accounts
    /// and any accounts that may have been created but are not currently in use.
    ///
    /// # Returns
    ///
    /// Returns the count of accounts as a `usize`. Returns 0 if no accounts exist.
    pub async fn get_accounts_count(&self) -> Result<usize> {
        let accounts = Account::all(&self.database).await?;
        Ok(accounts.len())
    }

    /// Retrieves all accounts stored in the database.
    ///
    /// This method returns all accounts that have been created or imported into
    /// the Whitenoise instance. Each account represents a distinct identity with
    /// its own keypair, relay configurations, and associated data.
    pub async fn all_accounts(&self) -> Result<Vec<Account>> {
        Account::all(&self.database).await
    }

    /// Finds and returns an account by its public key.
    ///
    /// This method searches the database for an account with the specified public key.
    /// Public keys are unique identifiers in Nostr, so this will return at most one account.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The public key of the account to find
    pub async fn find_account_by_pubkey(&self, pubkey: &PublicKey) -> Result<Account> {
        Account::find_by_pubkey(pubkey, &self.database).await
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};

    use super::*;
    use crate::RelayType;
    use crate::whitenoise::accounts::{Account, AccountType, ExternalSignerRelaySetup};
    use crate::whitenoise::relays::Relay;
    use crate::whitenoise::test_utils::*;

    #[tokio::test]
    #[ignore]
    async fn test_login_after_delete_all_data() {
        let whitenoise = test_get_whitenoise().await;

        let account = setup_login_account(whitenoise).await;
        whitenoise.delete_all_data().await.unwrap();
        let _acc = whitenoise
            .login(account.1.secret_key().to_secret_hex())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_load_accounts() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Test loading empty database
        let accounts = Account::all(&whitenoise.database).await.unwrap();
        assert!(accounts.is_empty());

        // Create test accounts and save them to database
        let (account1, keys1) = create_test_account(&whitenoise).await;
        let (account2, keys2) = create_test_account(&whitenoise).await;

        // Save accounts to database
        account1.save(&whitenoise.database).await.unwrap();
        account2.save(&whitenoise.database).await.unwrap();

        // Store keys in secrets store (required for background fetch)
        whitenoise.secrets_store.store_private_key(&keys1).unwrap();
        whitenoise.secrets_store.store_private_key(&keys2).unwrap();

        // Load accounts from database
        let loaded_accounts = Account::all(&whitenoise.database).await.unwrap();
        assert_eq!(loaded_accounts.len(), 2);
        let pubkeys: Vec<PublicKey> = loaded_accounts.iter().map(|a| a.pubkey).collect();
        assert!(pubkeys.contains(&account1.pubkey));
        assert!(pubkeys.contains(&account2.pubkey));

        // Verify account data is correctly loaded
        let loaded_account1 = loaded_accounts
            .iter()
            .find(|a| a.pubkey == account1.pubkey)
            .unwrap();
        assert_eq!(loaded_account1.pubkey, account1.pubkey);
        assert_eq!(loaded_account1.user_id, account1.user_id);
        assert_eq!(loaded_account1.last_synced_at, account1.last_synced_at);
        // Allow for small precision differences in timestamps due to database storage
        let created_diff = (loaded_account1.created_at - account1.created_at)
            .num_milliseconds()
            .abs();
        let updated_diff = (loaded_account1.updated_at - account1.updated_at)
            .num_milliseconds()
            .abs();
        assert!(
            created_diff <= 1,
            "Created timestamp difference too large: {}ms",
            created_diff
        );
        assert!(
            updated_diff <= 1,
            "Updated timestamp difference too large: {}ms",
            updated_diff
        );
    }

    #[tokio::test]
    async fn test_create_identity_publishes_relay_lists() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a new identity
        let account = whitenoise.create_identity().await.unwrap();

        // Give the events time to be published and processed
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let nip65_relays = account.nip65_relays(&whitenoise).await.unwrap();
        let nip65_relay_urls = Relay::urls(&nip65_relays);
        // Check that all three event types were published
        let inbox_events = whitenoise
            .nostr
            .fetch_user_relays(account.pubkey, RelayType::Inbox, &nip65_relay_urls)
            .await
            .unwrap();

        let key_package_relays_events = whitenoise
            .nostr
            .fetch_user_relays(account.pubkey, RelayType::KeyPackage, &nip65_relay_urls)
            .await
            .unwrap();

        let key_package_events = whitenoise
            .nostr
            .fetch_user_key_package(
                account.pubkey,
                &Relay::urls(&account.nip65_relays(&whitenoise).await.unwrap()),
            )
            .await
            .unwrap();

        // Verify that the relay list events were published
        assert!(
            inbox_events.is_some(),
            "Inbox relays list (kind 10050) should be published for new accounts"
        );
        assert!(
            key_package_relays_events.is_some(),
            "Key package relays list (kind 10051) should be published for new accounts"
        );
        assert!(
            key_package_events.is_some(),
            "Key package (kind 443) should be published for new accounts"
        );
    }

    /// Helper function to verify that an account has all three relay lists properly configured
    async fn verify_account_relay_lists_setup(whitenoise: &Whitenoise, account: &Account) {
        // Verify all three relay lists are set up with default relays
        let default_relays = Relay::defaults();
        let default_relay_count = default_relays.len();

        // Check relay database state
        assert_eq!(
            account.nip65_relays(whitenoise).await.unwrap().len(),
            default_relay_count,
            "Account should have default NIP-65 relays configured"
        );
        assert_eq!(
            account.inbox_relays(whitenoise).await.unwrap().len(),
            default_relay_count,
            "Account should have default inbox relays configured"
        );
        assert_eq!(
            account.key_package_relays(whitenoise).await.unwrap().len(),
            default_relay_count,
            "Account should have default key package relays configured"
        );

        let default_relays_vec: Vec<RelayUrl> = Relay::urls(&default_relays);
        let nip65_relay_urls: Vec<RelayUrl> =
            Relay::urls(&account.nip65_relays(whitenoise).await.unwrap());
        let inbox_relay_urls: Vec<RelayUrl> =
            Relay::urls(&account.inbox_relays(whitenoise).await.unwrap());
        let key_package_relay_urls: Vec<RelayUrl> =
            Relay::urls(&account.key_package_relays(whitenoise).await.unwrap());
        for default_relay in default_relays_vec.iter() {
            assert!(
                nip65_relay_urls.contains(default_relay),
                "NIP-65 relays should contain default relay: {}",
                default_relay
            );
            assert!(
                inbox_relay_urls.contains(default_relay),
                "Inbox relays should contain default relay: {}",
                default_relay
            );
            assert!(
                key_package_relay_urls.contains(default_relay),
                "Key package relays should contain default relay: {}",
                default_relay
            );
        }
    }

    /// Helper function to verify that an account has a key package published
    async fn verify_account_key_package_exists(whitenoise: &Whitenoise, account: &Account) {
        // Check if key package exists by trying to fetch it
        let key_package_event = whitenoise
            .nostr
            .fetch_user_key_package(
                account.pubkey,
                &Relay::urls(&account.key_package_relays(whitenoise).await.unwrap()),
            )
            .await
            .unwrap();

        assert!(
            key_package_event.is_some(),
            "Account should have a key package published to relays"
        );

        // If key package exists, verify it's authored by the correct account
        if let Some(event) = key_package_event {
            assert_eq!(
                event.pubkey, account.pubkey,
                "Key package should be authored by the account's public key"
            );
            assert_eq!(
                event.kind,
                Kind::MlsKeyPackage,
                "Event should be a key package (kind 443)"
            );
        }
    }

    #[tokio::test]
    async fn test_create_identity_sets_up_all_requirements() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a new identity
        let account = whitenoise.create_identity().await.unwrap();

        // Give the events time to be published and processed
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Verify all three relay lists are properly configured
        verify_account_relay_lists_setup(&whitenoise, &account).await;

        // Verify key package is published
        verify_account_key_package_exists(&whitenoise, &account).await;
    }

    #[tokio::test]
    async fn test_login_existing_account_sets_up_all_requirements() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an account through login (simulating an existing account)
        let keys = create_test_keys();
        let account = whitenoise
            .login(keys.secret_key().to_secret_hex())
            .await
            .unwrap();

        // Give the events time to be published and processed
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Verify all three relay lists are properly configured
        verify_account_relay_lists_setup(&whitenoise, &account).await;

        // Verify key package is published
        verify_account_key_package_exists(&whitenoise, &account).await;
    }

    #[tokio::test]
    async fn test_login_with_existing_relay_lists_preserves_them() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // First, create an account and let it publish relay lists
        let keys = create_test_keys();
        let account1 = whitenoise
            .login(keys.secret_key().to_secret_hex())
            .await
            .unwrap();

        // Give time for initial setup
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Verify initial setup is correct
        verify_account_relay_lists_setup(&whitenoise, &account1).await;
        verify_account_key_package_exists(&whitenoise, &account1).await;

        // Logout the account
        whitenoise.logout(&account1.pubkey).await.unwrap();

        // Login again with the same keys (simulating returning user)
        let account2 = whitenoise
            .login(keys.secret_key().to_secret_hex())
            .await
            .unwrap();

        // Give time for login process
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Verify that relay lists are still properly configured
        verify_account_relay_lists_setup(&whitenoise, &account2).await;

        // Verify key package still exists (should not publish a new one)
        verify_account_key_package_exists(&whitenoise, &account2).await;

        // Accounts should be equivalent (same pubkey, same basic setup)
        assert_eq!(
            account1.pubkey, account2.pubkey,
            "Same keys should result in same account"
        );
    }

    #[tokio::test]
    async fn test_login_double_login_returns_existing_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = create_test_keys();
        let first_account = whitenoise
            .login(keys.secret_key().to_secret_hex())
            .await
            .unwrap();

        // Login again with the same key without logging out
        let second_account = whitenoise
            .login(keys.secret_key().to_secret_hex())
            .await
            .unwrap();

        // Should return the same account, not create a new one
        assert_eq!(first_account.id, second_account.id);
        assert_eq!(first_account.pubkey, second_account.pubkey);

        // Should still have exactly one account in the database
        let count = whitenoise.get_accounts_count().await.unwrap();
        assert_eq!(count, 1, "Double login should not create a second account");
    }

    #[tokio::test]
    async fn test_create_identity_creates_cancellation_channel() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();

        assert!(
            whitenoise
                .background_task_cancellation
                .contains_key(&account.pubkey),
            "activate_account should create a cancellation channel"
        );
    }

    #[tokio::test]
    async fn test_logout_signals_and_removes_cancellation_channel() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();

        // Subscribe to the cancellation channel before logout
        let cancel_rx = whitenoise
            .background_task_cancellation
            .get(&account.pubkey)
            .expect("cancellation channel should exist after login")
            .value()
            .subscribe();

        // Initially not cancelled
        assert!(
            !*cancel_rx.borrow(),
            "should not be cancelled before logout"
        );

        whitenoise.logout(&account.pubkey).await.unwrap();

        // After logout, the channel should have been signalled
        assert!(
            *cancel_rx.borrow(),
            "logout should signal cancellation to running background tasks"
        );

        // And the entry should be removed from the map
        assert!(
            !whitenoise
                .background_task_cancellation
                .contains_key(&account.pubkey),
            "logout should remove the cancellation channel entry"
        );
    }

    #[tokio::test]
    async fn test_multiple_accounts_each_have_proper_setup() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create multiple accounts
        let mut accounts = Vec::new();
        for i in 0..3 {
            let keys = create_test_keys();
            let account = whitenoise
                .login(keys.secret_key().to_secret_hex())
                .await
                .unwrap();
            accounts.push((account, keys));

            tracing::info!("Created account {}: {}", i, accounts[i].0.pubkey.to_hex());
        }

        // Give time for all accounts to be set up
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

        // Verify each account has proper setup
        for (i, (account, _)) in accounts.iter().enumerate() {
            tracing::info!("Verifying account {}: {}", i, account.pubkey.to_hex());

            // Verify all three relay lists are properly configured
            verify_account_relay_lists_setup(&whitenoise, account).await;

            // Verify key package is published
            verify_account_key_package_exists(&whitenoise, account).await;
        }

        // Verify accounts are distinct
        for i in 0..accounts.len() {
            for j in i + 1..accounts.len() {
                assert_ne!(
                    accounts[i].0.pubkey, accounts[j].0.pubkey,
                    "Each account should have a unique public key"
                );
            }
        }
    }

    #[test]
    fn test_since_timestamp_none_when_never_synced() {
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(account.since_timestamp(10).is_none());
    }

    #[test]
    fn test_since_timestamp_applies_buffer() {
        let now = Utc::now();
        let last = now - TimeDelta::seconds(100);
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: Some(last),
            created_at: now,
            updated_at: now,
        };
        let ts = account.since_timestamp(10).unwrap();
        let expected_secs = (last.timestamp().max(0) as u64).saturating_sub(10);
        assert_eq!(ts.as_secs(), expected_secs);
    }

    #[test]
    fn test_since_timestamp_floors_at_zero() {
        // Choose a timestamp very close to the epoch so that buffer would underflow
        let epochish = chrono::DateTime::<Utc>::from_timestamp(5, 0).unwrap();
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: Some(epochish),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let ts = account.since_timestamp(10).unwrap();
        assert_eq!(ts.as_secs(), 0);
    }

    #[test]
    fn test_since_timestamp_clamps_future_to_now_minus_buffer() {
        let now = Utc::now();
        let future = now + chrono::TimeDelta::seconds(3600 * 24); // 24h in the future
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: Some(future),
            created_at: now,
            updated_at: now,
        };
        let buffer = 10u64;
        // Capture time before and after to bound the internal now() used by the function
        let before = Utc::now();
        let ts = account.since_timestamp(buffer).unwrap();
        let after = Utc::now();

        let before_secs = before.timestamp().max(0) as u64;
        let after_secs = after.timestamp().max(0) as u64;

        let min_expected = before_secs.saturating_sub(buffer);
        let max_expected = after_secs.saturating_sub(buffer);

        let actual = ts.as_secs();
        assert!(actual >= min_expected && actual <= max_expected);
    }

    #[tokio::test]
    async fn test_update_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_test_account(&whitenoise).await;
        account.save(&whitenoise.database).await.unwrap();

        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        let default_relays = whitenoise.load_default_relays().await.unwrap();
        whitenoise
            .add_relays_to_account(&account, &default_relays, RelayType::Nip65)
            .await
            .unwrap();

        let test_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let new_metadata = Metadata::new()
            .name(format!("updated_user_{}", test_timestamp))
            .display_name(format!("Updated User {}", test_timestamp))
            .about("Updated metadata for testing");

        let result = account.update_metadata(&new_metadata, &whitenoise).await;
        result.expect("Failed to update metadata. Are test relays running on localhost:8080 and localhost:7777?");

        let user = account.user(&whitenoise.database).await.unwrap();
        assert_eq!(user.metadata.name, new_metadata.name);
        assert_eq!(user.metadata.display_name, new_metadata.display_name);
        assert_eq!(user.metadata.about, new_metadata.about);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let nip65_relays = account.nip65_relays(&whitenoise).await.unwrap();
        let nip65_relay_urls = Relay::urls(&nip65_relays);
        let fetched_metadata = whitenoise
            .nostr
            .fetch_metadata_from(&nip65_relay_urls, account.pubkey)
            .await
            .expect("Failed to fetch metadata from relays");

        if let Some(event) = fetched_metadata {
            let published_metadata = Metadata::from_json(&event.content).unwrap();
            assert_eq!(published_metadata.name, new_metadata.name);
            assert_eq!(published_metadata.display_name, new_metadata.display_name);
            assert_eq!(published_metadata.about, new_metadata.about);
        }
    }

    #[tokio::test]
    async fn test_logout_local_account_removes_key() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a local account directly in the database (bypassing relay setup)
        let (account, keys) = create_test_account(&whitenoise).await;
        account.save(&whitenoise.database).await.unwrap();

        assert_eq!(
            account.account_type,
            AccountType::Local,
            "Account should be Local type"
        );

        // Store the key in secrets store
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        // Verify the key is stored
        let stored_keys = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey);
        assert!(stored_keys.is_ok(), "Key should be stored after login");

        // Logout should remove the key
        whitenoise.logout(&account.pubkey).await.unwrap();

        // Verify the key was removed
        let stored_keys_after = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey);
        assert!(
            stored_keys_after.is_err(),
            "Key should be removed after logout"
        );
    }

    #[tokio::test]
    async fn test_logout_external_account_cleans_stale_keys() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an external account directly in the database
        let keys = create_test_keys();
        let pubkey = keys.public_key();

        // Create external account manually (bypassing relay setup)
        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        account.save(&whitenoise.database).await.unwrap();

        assert_eq!(
            account.account_type,
            AccountType::External,
            "Account should be External type"
        );

        // Manually store a stale key (simulating orphaned key from failed migration)
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        // Verify the stale key is stored
        let stored_keys = whitenoise.secrets_store.get_nostr_keys_for_pubkey(&pubkey);
        assert!(stored_keys.is_ok(), "Stale key should be stored");

        // Logout should clean up the stale key via best-effort removal
        whitenoise.logout(&pubkey).await.unwrap();

        // Verify the stale key was removed
        let stored_keys_after = whitenoise.secrets_store.get_nostr_keys_for_pubkey(&pubkey);
        assert!(
            stored_keys_after.is_err(),
            "Stale key should be removed after logout"
        );
    }

    #[tokio::test]
    async fn test_logout_external_account_without_key_succeeds() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create an external account directly in the database
        let keys = create_test_keys();
        let pubkey = keys.public_key();

        // Create external account manually (bypassing relay setup)
        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        account.save(&whitenoise.database).await.unwrap();

        assert_eq!(
            account.account_type,
            AccountType::External,
            "Account should be External type"
        );

        // Don't store any key - verify there's no key
        let stored_keys = whitenoise.secrets_store.get_nostr_keys_for_pubkey(&pubkey);
        assert!(stored_keys.is_err(), "No key should be stored");

        // Logout should succeed even with no key to remove
        let result = whitenoise.logout(&pubkey).await;
        assert!(
            result.is_ok(),
            "Logout should succeed for external account without stored key"
        );
    }

    #[test]
    fn test_has_local_key_returns_true_for_local_account() {
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(
            account.has_local_key(),
            "Local account should have local key"
        );
    }

    #[test]
    fn test_has_local_key_returns_false_for_external_account() {
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: AccountType::External,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(
            !account.has_local_key(),
            "External account should not have local key"
        );
    }

    #[test]
    fn test_account_type_from_str_local() {
        let result: std::result::Result<AccountType, String> = "local".parse();
        assert_eq!(result.unwrap(), AccountType::Local);

        // Test case insensitivity
        let result: std::result::Result<AccountType, String> = "LOCAL".parse();
        assert_eq!(result.unwrap(), AccountType::Local);

        let result: std::result::Result<AccountType, String> = "Local".parse();
        assert_eq!(result.unwrap(), AccountType::Local);
    }

    #[test]
    fn test_account_type_from_str_external() {
        let result: std::result::Result<AccountType, String> = "external".parse();
        assert_eq!(result.unwrap(), AccountType::External);

        // Test case insensitivity
        let result: std::result::Result<AccountType, String> = "EXTERNAL".parse();
        assert_eq!(result.unwrap(), AccountType::External);

        let result: std::result::Result<AccountType, String> = "External".parse();
        assert_eq!(result.unwrap(), AccountType::External);
    }

    #[test]
    fn test_account_type_from_str_invalid() {
        let result: std::result::Result<AccountType, String> = "invalid".parse();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Unknown account type: invalid");

        let result: std::result::Result<AccountType, String> = "".parse();
        assert!(result.is_err());

        let result: std::result::Result<AccountType, String> = "123".parse();
        assert!(result.is_err());
    }

    #[test]
    fn test_account_type_display() {
        assert_eq!(format!("{}", AccountType::Local), "local");
        assert_eq!(format!("{}", AccountType::External), "external");
    }

    #[test]
    fn test_account_type_default() {
        let default_type = AccountType::default();
        assert_eq!(default_type, AccountType::Local);
    }

    #[test]
    fn test_uses_external_signer_returns_true_for_external() {
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: AccountType::External,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(
            account.uses_external_signer(),
            "External account should use external signer"
        );
    }

    #[test]
    fn test_uses_external_signer_returns_false_for_local() {
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(
            !account.uses_external_signer(),
            "Local account should not use external signer"
        );
    }

    #[tokio::test]
    async fn test_new_external_creates_external_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = create_test_keys();
        let pubkey = keys.public_key();

        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();

        assert_eq!(account.account_type, AccountType::External);
        assert_eq!(account.pubkey, pubkey);
        assert!(account.id.is_none()); // Not persisted yet
        assert!(account.last_synced_at.is_none());
    }

    /// Test that external account helper methods work correctly (uses_external_signer, has_local_key)
    #[tokio::test]
    async fn test_external_account_uses_external_signer_and_has_local_key() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Create external account directly (doesn't need relay)
        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();

        // Test helper methods
        assert!(
            account.uses_external_signer(),
            "External account should report using external signer"
        );
        assert!(
            !account.has_local_key(),
            "External account should not have local key"
        );
    }

    /// Test that local account helper methods return correct values
    #[tokio::test]
    async fn test_local_account_uses_external_signer_and_has_local_key() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create local account
        let (account, _keys) = Account::new(&whitenoise, None).await.unwrap();

        // Test helper methods
        assert!(
            !account.uses_external_signer(),
            "Local account should not report using external signer"
        );
        assert!(
            account.has_local_key(),
            "Local account should report having local key"
        );
    }

    /// Test Account struct serialization/deserialization with JSON
    #[tokio::test]
    async fn test_account_json_serialization_roundtrip() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Create both types of accounts
        let external_account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        let (local_account, _) = Account::new(&whitenoise, None).await.unwrap();

        // Serialize and deserialize external account
        let external_json = serde_json::to_string(&external_account).unwrap();
        let external_deserialized: Account = serde_json::from_str(&external_json).unwrap();
        assert_eq!(external_account.pubkey, external_deserialized.pubkey);
        assert_eq!(
            external_account.account_type,
            external_deserialized.account_type
        );
        assert_eq!(external_deserialized.account_type, AccountType::External);

        // Serialize and deserialize local account
        let local_json = serde_json::to_string(&local_account).unwrap();
        let local_deserialized: Account = serde_json::from_str(&local_json).unwrap();
        assert_eq!(local_account.pubkey, local_deserialized.pubkey);
        assert_eq!(local_account.account_type, local_deserialized.account_type);
        assert_eq!(local_deserialized.account_type, AccountType::Local);
    }

    /// Test Account debug formatting includes account_type
    #[tokio::test]
    async fn test_account_debug_includes_account_type() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let external_account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        let (local_account, _) = Account::new(&whitenoise, None).await.unwrap();

        let external_debug = format!("{:?}", external_account);
        let local_debug = format!("{:?}", local_account);

        assert!(
            external_debug.contains("External"),
            "External account debug should contain 'External': {}",
            external_debug
        );
        assert!(
            local_debug.contains("Local"),
            "Local account debug should contain 'Local': {}",
            local_debug
        );
    }

    /// Test Account::new_external properly sets up external account without relay fetch
    #[tokio::test]
    async fn test_new_external_sets_correct_fields() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();

        // Verify all fields are correctly set
        assert_eq!(account.pubkey, pubkey);
        assert_eq!(account.account_type, AccountType::External);
        assert!(account.id.is_none(), "New account should not be persisted");
        assert!(account.last_synced_at.is_none());
        assert!(account.user_id > 0, "Should have a valid user_id");
    }

    /// Test external account persists correctly to database
    #[tokio::test]
    async fn test_external_account_database_roundtrip() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Create and persist external account
        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        let persisted = whitenoise.persist_account(&account).await.unwrap();

        // Verify persisted correctly
        assert!(persisted.id.is_some());
        assert_eq!(persisted.account_type, AccountType::External);

        // Find it back
        let found = Account::find_by_pubkey(&pubkey, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(found.pubkey, pubkey);
        assert_eq!(found.account_type, AccountType::External);
    }

    /// Test that secrets store doesn't have keys for external accounts
    #[tokio::test]
    async fn test_external_account_no_keys_in_secrets_store() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Create external account (doesn't store any keys)
        let _account = Account::new_external(&whitenoise, pubkey).await.unwrap();

        // Verify no keys in secrets store for this pubkey
        let result = whitenoise.secrets_store.get_nostr_keys_for_pubkey(&pubkey);
        assert!(
            result.is_err(),
            "External account creation should not store any keys"
        );
    }

    #[tokio::test]
    async fn test_new_creates_local_account_with_generated_keys() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (account, keys) = Account::new(&whitenoise, None).await.unwrap();

        assert_eq!(account.account_type, AccountType::Local);
        assert_eq!(account.pubkey, keys.public_key());
        assert!(account.id.is_none()); // Not persisted yet
        assert!(account.last_synced_at.is_none());
    }

    #[tokio::test]
    async fn test_new_creates_local_account_with_provided_keys() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let provided_keys = create_test_keys();

        let (account, keys) = Account::new(&whitenoise, Some(provided_keys.clone()))
            .await
            .unwrap();

        assert_eq!(account.account_type, AccountType::Local);
        assert_eq!(account.pubkey, provided_keys.public_key());
        assert_eq!(keys.public_key(), provided_keys.public_key());
        assert_eq!(keys.secret_key(), provided_keys.secret_key());
    }

    /// Comprehensive test for account relay operations including all relay types
    #[tokio::test]
    async fn test_account_relay_operations() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create and persist account
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = whitenoise.persist_account(&account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        // New account should have no relays
        assert!(account.nip65_relays(&whitenoise).await.unwrap().is_empty());
        assert!(account.inbox_relays(&whitenoise).await.unwrap().is_empty());
        assert!(
            account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty()
        );

        // Load and add default relays
        let default_relays = whitenoise.load_default_relays().await.unwrap();
        #[cfg(debug_assertions)]
        assert_eq!(default_relays.len(), 2);

        // Add relays for all types
        whitenoise
            .add_relays_to_account(&account, &default_relays, RelayType::Nip65)
            .await
            .unwrap();
        whitenoise
            .add_relays_to_account(&account, &default_relays, RelayType::Inbox)
            .await
            .unwrap();
        whitenoise
            .add_relays_to_account(&account, &default_relays, RelayType::KeyPackage)
            .await
            .unwrap();

        // Verify all relay types have relays
        let nip65 = account.relays(RelayType::Nip65, &whitenoise).await.unwrap();
        let inbox = account.relays(RelayType::Inbox, &whitenoise).await.unwrap();
        let kp = account
            .relays(RelayType::KeyPackage, &whitenoise)
            .await
            .unwrap();
        assert_eq!(nip65.len(), default_relays.len());
        assert_eq!(inbox.len(), default_relays.len());
        assert_eq!(kp.len(), default_relays.len());

        // Test add_relay and remove_relay on individual relays
        let test_relay = Relay::find_or_create_by_url(
            &RelayUrl::parse("wss://test.relay.example").unwrap(),
            &whitenoise.database,
        )
        .await
        .unwrap();
        account
            .add_relay(&test_relay, RelayType::Nip65, &whitenoise)
            .await
            .unwrap();
        let relays_after_add = account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(relays_after_add.len(), default_relays.len() + 1);

        account
            .remove_relay(&test_relay, RelayType::Nip65, &whitenoise)
            .await
            .unwrap();
        let relays_after_remove = account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(relays_after_remove.len(), default_relays.len());
    }

    /// Comprehensive test for account CRUD operations
    #[tokio::test]
    async fn test_account_crud_operations() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Initially empty
        assert_eq!(whitenoise.get_accounts_count().await.unwrap(), 0);
        assert!(whitenoise.all_accounts().await.unwrap().is_empty());

        // Create and persist accounts
        let (account1, _) = create_test_account(&whitenoise).await;
        let account1 = whitenoise.persist_account(&account1).await.unwrap();
        assert!(account1.id.is_some());

        let (account2, _) = create_test_account(&whitenoise).await;
        let _account2 = whitenoise.persist_account(&account2).await.unwrap();

        // Verify counts and retrieval
        assert_eq!(whitenoise.get_accounts_count().await.unwrap(), 2);
        let all = whitenoise.all_accounts().await.unwrap();
        assert_eq!(all.len(), 2);

        // Find by pubkey
        let found = whitenoise
            .find_account_by_pubkey(&account1.pubkey)
            .await
            .unwrap();
        assert_eq!(found.pubkey, account1.pubkey);

        // Not found for random pubkey
        let random_pk = Keys::generate().public_key();
        assert!(whitenoise.find_account_by_pubkey(&random_pk).await.is_err());

        // Test user and metadata retrieval
        let user = account1.user(&whitenoise.database).await.unwrap();
        assert_eq!(user.pubkey, account1.pubkey);

        let metadata = account1.metadata(&whitenoise).await.unwrap();
        assert!(metadata.name.is_none() || metadata.name.as_deref() == Some(""));
    }

    /// Test account creation for both local and external account types
    #[tokio::test]
    async fn test_account_type_creation() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Local account with generated keys
        let (local_gen, keys_gen) = Account::new(&whitenoise, None).await.unwrap();
        assert_eq!(local_gen.account_type, AccountType::Local);
        assert_eq!(local_gen.pubkey, keys_gen.public_key());

        // Local account with provided keys
        let provided = create_test_keys();
        let (local_prov, keys_prov) = Account::new(&whitenoise, Some(provided.clone()))
            .await
            .unwrap();
        assert_eq!(local_prov.account_type, AccountType::Local);
        assert_eq!(keys_prov.public_key(), provided.public_key());

        // External account
        let ext_pubkey = Keys::generate().public_key();
        let external = Account::new_external(&whitenoise, ext_pubkey)
            .await
            .unwrap();
        assert_eq!(external.account_type, AccountType::External);
        assert_eq!(external.pubkey, ext_pubkey);
        assert!(!external.has_local_key());
        assert!(external.uses_external_signer());
    }

    #[test]
    fn test_create_mdk_success() {
        // Initialize mock keyring so this test passes on headless CI (e.g. Ubuntu)
        crate::whitenoise::Whitenoise::initialize_mock_keyring_store();

        let temp_dir = tempfile::TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let result = Account::create_mdk(pubkey, temp_dir.path(), "com.whitenoise.test");
        assert!(result.is_ok(), "create_mdk failed: {:?}", result.err());
    }

    /// Test logout removes keys correctly for both account types
    #[tokio::test]
    async fn test_logout_key_cleanup() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Local account logout removes key
        let (local_account, keys) = create_test_account(&whitenoise).await;
        local_account.save(&whitenoise.database).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&local_account.pubkey)
                .is_ok()
        );
        whitenoise.logout(&local_account.pubkey).await.unwrap();
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&local_account.pubkey)
                .is_err()
        );

        // External account logout with stale key cleans up
        let ext_keys = create_test_keys();
        let ext_account = Account::new_external(&whitenoise, ext_keys.public_key())
            .await
            .unwrap();
        ext_account.save(&whitenoise.database).await.unwrap();
        whitenoise
            .secrets_store
            .store_private_key(&ext_keys)
            .unwrap(); // Stale key

        whitenoise.logout(&ext_account.pubkey).await.unwrap();
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&ext_account.pubkey)
                .is_err()
        );

        // External account logout without key succeeds
        let ext2 = Account::new_external(&whitenoise, Keys::generate().public_key())
            .await
            .unwrap();
        ext2.save(&whitenoise.database).await.unwrap();
        assert!(whitenoise.logout(&ext2.pubkey).await.is_ok());
    }

    /// Test upload_profile_picture uploads to blossom server and returns URL
    /// Requires blossom server running on localhost:3000
    #[ignore]
    #[tokio::test]
    async fn test_upload_profile_picture() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create and persist account with stored keys
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = whitenoise.persist_account(&account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        // Use the test image file
        let test_image_path = ".test/fake_image.png";

        // Check if blossom server is available
        let blossom_url = nostr_sdk::Url::parse("http://localhost:3000").unwrap();

        let result = account
            .upload_profile_picture(
                test_image_path,
                crate::types::ImageType::Png,
                blossom_url,
                &whitenoise,
            )
            .await;

        // Test should succeed if blossom server is running
        assert!(
            result.is_ok(),
            "upload_profile_picture should succeed. Error: {:?}",
            result.err()
        );

        let url = result.unwrap();
        assert!(
            url.starts_with("http://localhost:3000"),
            "Returned URL should be from blossom server"
        );
    }

    /// Test upload_profile_picture fails gracefully with non-existent file
    #[ignore]
    #[tokio::test]
    async fn test_upload_profile_picture_nonexistent_file() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (account, keys) = create_test_account(&whitenoise).await;
        let account = whitenoise.persist_account(&account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        let blossom_url = nostr_sdk::Url::parse("http://localhost:3000").unwrap();

        let result = account
            .upload_profile_picture(
                "/nonexistent/path/image.png",
                crate::types::ImageType::Png,
                blossom_url,
                &whitenoise,
            )
            .await;

        assert!(
            result.is_err(),
            "upload_profile_picture should fail for non-existent file"
        );
    }

    /// Test login_with_external_signer creates new external account
    #[tokio::test]
    async fn test_login_with_external_signer_new_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = create_test_keys();
        let pubkey = keys.public_key();

        // Login with external signer (new account)
        let account = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        // Verify account was created correctly
        assert_eq!(account.pubkey, pubkey);
        assert_eq!(account.account_type, AccountType::External);
        assert!(account.id.is_some(), "Account should be persisted");
        assert!(account.uses_external_signer(), "Should use external signer");
        assert!(!account.has_local_key(), "Should not have local key");

        // Verify no private key was stored
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&pubkey)
                .is_err(),
            "External account should not have stored private key"
        );

        // Verify relay lists are set up
        let nip65 = account.nip65_relays(&whitenoise).await.unwrap();
        let inbox = account.inbox_relays(&whitenoise).await.unwrap();
        let kp = account.key_package_relays(&whitenoise).await.unwrap();

        assert!(!nip65.is_empty(), "Should have NIP-65 relays");
        assert!(!inbox.is_empty(), "Should have inbox relays");
        assert!(!kp.is_empty(), "Should have key package relays");
    }

    /// Test login_with_external_signer with existing account re-establishes connections
    #[tokio::test]
    async fn test_login_with_external_signer_existing_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = create_test_keys();

        // First login - creates new account
        let account1 = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();
        assert!(account1.id.is_some());

        // Allow some time for setup
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Second login - should return existing account
        let account2 = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        assert_eq!(account1.pubkey, account2.pubkey);
        assert_eq!(account2.account_type, AccountType::External);
    }

    /// Test login_with_external_signer migrates local account to external
    #[tokio::test]
    async fn test_login_with_external_signer_migrates_local_to_external() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // First, create a local account directly
        let keys = create_test_keys();
        let pubkey = keys.public_key();

        let (local_account, _) = Account::new(&whitenoise, Some(keys.clone())).await.unwrap();
        assert_eq!(local_account.account_type, AccountType::Local);
        let _local_account = whitenoise.persist_account(&local_account).await.unwrap();

        // Store the key (simulating normal local account creation)
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        // Now login with external signer - should migrate
        let migrated = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        assert_eq!(migrated.pubkey, pubkey);
        assert_eq!(
            migrated.account_type,
            AccountType::External,
            "Account should be migrated to External"
        );

        // Verify local key was removed during migration
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&pubkey)
                .is_err(),
            "Local key should be removed during migration to external"
        );
    }

    /// Test login_with_external_signer removes stale keys on re-login
    #[tokio::test]
    async fn test_login_with_external_signer_removes_stale_keys() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = create_test_keys();
        let pubkey = keys.public_key();

        // Create external account first
        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        whitenoise.persist_account(&account).await.unwrap();

        // Manually store a "stale" key (simulating failed migration or orphaned key)
        whitenoise.secrets_store.store_private_key(&keys).unwrap();
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&pubkey)
                .is_ok()
        );

        // Login with external signer - should clean up stale key
        whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        // Verify stale key was removed
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&pubkey)
                .is_err(),
            "Stale key should be removed during external signer login"
        );
    }

    // AccountType Serialization Tests
    #[test]
    fn test_account_type_json_serialization() {
        // Test Local serializes correctly
        let local = AccountType::Local;
        let json = serde_json::to_string(&local).unwrap();
        assert_eq!(json, "\"Local\"");

        // Test External serializes correctly
        let external = AccountType::External;
        let json = serde_json::to_string(&external).unwrap();
        assert_eq!(json, "\"External\"");
    }

    #[test]
    fn test_account_type_json_deserialization() {
        // Test Local deserializes correctly
        let local: AccountType = serde_json::from_str("\"Local\"").unwrap();
        assert_eq!(local, AccountType::Local);

        // Test External deserializes correctly
        let external: AccountType = serde_json::from_str("\"External\"").unwrap();
        assert_eq!(external, AccountType::External);
    }

    #[test]
    fn test_account_type_serialization_roundtrip() {
        // Test roundtrip for Local
        let original_local = AccountType::Local;
        let serialized = serde_json::to_string(&original_local).unwrap();
        let deserialized: AccountType = serde_json::from_str(&serialized).unwrap();
        assert_eq!(original_local, deserialized);

        // Test roundtrip for External
        let original_external = AccountType::External;
        let serialized = serde_json::to_string(&original_external).unwrap();
        let deserialized: AccountType = serde_json::from_str(&serialized).unwrap();
        assert_eq!(original_external, deserialized);
    }

    #[test]
    fn test_account_type_equality_and_hash() {
        use std::collections::HashSet;

        // Test equality
        assert_eq!(AccountType::Local, AccountType::Local);
        assert_eq!(AccountType::External, AccountType::External);
        assert_ne!(AccountType::Local, AccountType::External);

        // Test hashability (can be used in HashSet)
        let mut set = HashSet::new();
        set.insert(AccountType::Local);
        set.insert(AccountType::External);
        assert_eq!(set.len(), 2);

        // Inserting duplicate should not increase size
        set.insert(AccountType::Local);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_account_type_clone() {
        let local = AccountType::Local;
        let cloned = local.clone();
        assert_eq!(local, cloned);

        let external = AccountType::External;
        let cloned = external.clone();
        assert_eq!(external, cloned);
    }

    #[test]
    fn test_account_type_debug() {
        let local = AccountType::Local;
        let debug_str = format!("{:?}", local);
        assert!(debug_str.contains("Local"));

        let external = AccountType::External;
        let debug_str = format!("{:?}", external);
        assert!(debug_str.contains("External"));
    }

    /// Test that validate_signer_pubkey succeeds when pubkeys match
    #[tokio::test]
    async fn test_validate_signer_pubkey_matching() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Should succeed when signer's pubkey matches expected pubkey
        let result = whitenoise.validate_signer_pubkey(&pubkey, &keys).await;
        assert!(result.is_ok(), "Should succeed with matching pubkeys");
    }

    /// Test that validate_signer_pubkey fails when pubkeys don't match
    #[tokio::test]
    async fn test_validate_signer_pubkey_mismatched() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let expected_keys = Keys::generate();
        let wrong_keys = Keys::generate();
        let expected_pubkey = expected_keys.public_key();

        // Should fail when signer's pubkey doesn't match expected pubkey
        let result = whitenoise
            .validate_signer_pubkey(&expected_pubkey, &wrong_keys)
            .await;

        assert!(result.is_err(), "Should fail with mismatched pubkeys");
        let err = result.unwrap_err();
        let err_msg = format!("{}", err);
        assert!(
            err_msg.contains("pubkey mismatch"),
            "Error should mention pubkey mismatch, got: {}",
            err_msg
        );
    }

    /// Test that publish_relay_lists_with_signer skips publishing when all flags are false
    #[tokio::test]
    async fn test_publish_relay_lists_with_signer_skips_when_flags_false() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        // Create relay setup with all publish flags set to false
        let relay_setup = ExternalSignerRelaySetup {
            nip65_relays: vec![],
            inbox_relays: vec![],
            key_package_relays: vec![],
            should_publish_nip65: false,
            should_publish_inbox: false,
            should_publish_key_package: false,
        };

        // Should succeed without attempting any network operations
        let result = whitenoise
            .publish_relay_lists_with_signer(&relay_setup, keys)
            .await;

        assert!(
            result.is_ok(),
            "Should succeed when no publishing is needed"
        );
    }

    /// Test that publish_relay_lists_with_signer attempts publishing when flags are true
    #[tokio::test]
    async fn test_publish_relay_lists_with_signer_publishes_when_flags_true() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        // Create an external account first - required for event tracking
        let _account = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        // Load default relays to have valid relay URLs
        let default_relays = whitenoise.load_default_relays().await.unwrap();

        // Create relay setup with all publish flags set to true
        let relay_setup = ExternalSignerRelaySetup {
            nip65_relays: default_relays.clone(),
            inbox_relays: default_relays.clone(),
            key_package_relays: default_relays,
            should_publish_nip65: true,
            should_publish_inbox: true,
            should_publish_key_package: true,
        };

        // This may fail due to relay connectivity in test environment,
        // but it exercises the code path that attempts publishing
        let result = whitenoise
            .publish_relay_lists_with_signer(&relay_setup, keys)
            .await;

        // We accept either success (if relays are connected) or
        // specific network errors (if relays are not connected)
        // The important thing is the method doesn't panic and
        // correctly handles the flags
        if let Err(ref e) = result {
            let err_msg = format!("{}", e);
            // These are acceptable errors in a test environment without relay connections
            let acceptable_errors = err_msg.contains("relay")
                || err_msg.contains("connection")
                || err_msg.contains("timeout")
                || err_msg.contains("Timeout");
            assert!(
                acceptable_errors || result.is_ok(),
                "Unexpected error: {}",
                err_msg
            );
        }
    }

    #[test]
    fn test_create_mdk_with_invalid_path() {
        let pubkey = Keys::generate().public_key();
        let file = tempfile::NamedTempFile::new().unwrap();
        let result = Account::create_mdk(pubkey, file.path(), "test.service");
        assert!(result.is_err());
    }

    // Account relay convenience method tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_account_relay_convenience_methods() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = whitenoise.persist_account(&account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        // Initially all relay types should be empty.
        assert!(account.nip65_relays(&whitenoise).await.unwrap().is_empty());
        assert!(account.inbox_relays(&whitenoise).await.unwrap().is_empty());
        assert!(
            account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty()
        );

        // Add relays to each type.
        let user = account.user(&whitenoise.database).await.unwrap();
        let url1 = RelayUrl::parse("wss://nip65.example.com").unwrap();
        let url2 = RelayUrl::parse("wss://inbox.example.com").unwrap();
        let url3 = RelayUrl::parse("wss://kp.example.com").unwrap();
        let relay1 = whitenoise.find_or_create_relay_by_url(&url1).await.unwrap();
        let relay2 = whitenoise.find_or_create_relay_by_url(&url2).await.unwrap();
        let relay3 = whitenoise.find_or_create_relay_by_url(&url3).await.unwrap();

        user.add_relays(&[relay1], RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        user.add_relays(&[relay2], RelayType::Inbox, &whitenoise.database)
            .await
            .unwrap();
        user.add_relays(&[relay3], RelayType::KeyPackage, &whitenoise.database)
            .await
            .unwrap();

        // Verify each convenience method returns the right relays.
        let nip65 = account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(nip65.len(), 1);
        assert_eq!(nip65[0].url, url1);

        let inbox = account.inbox_relays(&whitenoise).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].url, url2);

        let kp = account.key_package_relays(&whitenoise).await.unwrap();
        assert_eq!(kp.len(), 1);
        assert_eq!(kp[0].url, url3);

        // Also test the generic relays() method.
        let all_nip65 = account.relays(RelayType::Nip65, &whitenoise).await.unwrap();
        assert_eq!(all_nip65.len(), 1);
        assert_eq!(all_nip65[0].url, url1);
    }
}
