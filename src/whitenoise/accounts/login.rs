use chrono::{DateTime, Utc};
use nostr_sdk::prelude::*;

use super::Account;
use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::error::Result;

impl Whitenoise {
    /// Creates a new identity (account) for the user.
    ///
    /// This method generates a new keypair, sets up the account with default relay lists,
    /// and fully configures the account for use in Whitenoise.
    #[perf_instrument("accounts")]
    pub async fn create_identity(&self) -> Result<Account> {
        let keys = Keys::generate();
        tracing::debug!(target: "whitenoise::accounts", "Generated new keypair: {}", keys.public_key().to_hex());

        let account = self.create_identity_with_keys_inner(&keys, true).await?;

        tracing::debug!(target: "whitenoise::accounts", "Successfully created new identity: {}", account.pubkey.to_hex());
        Ok(account)
    }

    /// Creates a new identity that intentionally skips the initial key-package
    /// publish.
    ///
    /// **Integration-test only.** Production accounts must publish a key
    /// package as part of `create_identity` so other peers can reach them.
    /// This entry point exists for fixtures (e.g. the legacy-capability KP
    /// fixture) that need to be the *sole* publisher of an account's key
    /// packages so the test controls exactly which capability profile lands
    /// on the relays. Skipping the auto-publish here means the fixture's
    /// hand-crafted KP is the only one resolvers can find — no
    /// delete-and-republish gymnastics required.
    ///
    /// Aside from skipping the KP publish, behaviour matches `create_identity`:
    /// keys are stored, the account record is persisted, default relay lists
    /// are configured and published, and subscriptions are activated.
    #[cfg(feature = "integration-tests")]
    #[perf_instrument("accounts")]
    pub async fn create_identity_without_initial_key_package(&self) -> Result<Account> {
        let keys = Keys::generate();
        tracing::debug!(
            target: "whitenoise::accounts",
            "Generated new keypair (no-initial-KP variant): {}",
            keys.public_key().to_hex()
        );

        let account = self.create_identity_with_keys_inner(&keys, false).await?;

        tracing::debug!(
            target: "whitenoise::accounts",
            "Successfully created new identity without initial key package: {}",
            account.pubkey.to_hex()
        );
        Ok(account)
    }

    #[perf_instrument("accounts")]
    async fn create_identity_with_keys_inner(
        &self,
        keys: &Keys,
        publish_initial_key_package: bool,
    ) -> Result<Account> {
        let mut account = self.create_base_account_with_private_key(keys).await?;
        tracing::debug!(target: "whitenoise::accounts", "Keys stored in secret store and account saved to database");

        // A brand new identity has no history to catch up on — mark as synced
        // immediately. Without this, `last_synced_at = NULL` poisons
        // `compute_global_since_timestamp()` for ALL accounts, forcing
        // global subscriptions to use `since=None` (unbounded re-fetch).
        let now_ms = Utc::now().timestamp_millis();
        Account::update_last_synced_max(&account.pubkey, now_ms, &self.shared.database).await?;
        account.last_synced_at = DateTime::from_timestamp_millis(now_ms);

        let user = account.user(&self.shared.database).await?;

        let relays = self
            .setup_relays_for_new_account(&mut account, &user)
            .await?;
        tracing::debug!(target: "whitenoise::accounts", "Relays setup");

        self.activate_new_account(&account, &relays, publish_initial_key_package)
            .await?;
        tracing::debug!(target: "whitenoise::accounts", "Account persisted and activated");

        Ok(account)
    }

    #[cfg(test)]
    pub(crate) async fn create_test_identity_with_keys(&self, keys: &Keys) -> Result<Account> {
        self.create_identity_with_keys_inner(keys, true).await
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
    #[perf_instrument("accounts")]
    pub async fn login(&self, nsec_or_hex_privkey: String) -> Result<Account> {
        let keys = Keys::parse(&nsec_or_hex_privkey)?;
        let pubkey = keys.public_key();
        tracing::debug!(target: "whitenoise::accounts", "Logging in with pubkey: {}", pubkey.to_hex());

        // If this account is already logged in, return it as-is. The session
        // (relay connections, subscriptions, cancellation channel, background
        // tasks) was set up during the original login and is still active —
        // re-running activate_account would create duplicate subscriptions and
        // needlessly kill in-progress background tasks.
        if let Ok(existing) = Account::find_by_pubkey(&pubkey, &self.shared.database).await {
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
        let (_nip65_relays, inbox_relays, key_package_relays) =
            self.setup_relays_for_existing_account(&mut account).await?;
        tracing::debug!(target: "whitenoise::accounts", "Relays setup");

        self.activate_account(&account, false, &inbox_relays, &key_package_relays)
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
        if let Ok(existing) = Account::find_by_pubkey(&pubkey, &self.shared.database).await {
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

        self.activate_account_without_publishing(&account, &relay_setup.inbox_relays)
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
        let account = Account::find_by_pubkey(pubkey, &self.shared.database).await?;
        let ephemeral_warm_relays = self
            .account_ephemeral_warm_relay_urls(&account)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    target: "whitenoise::accounts",
                    account_pubkey = %pubkey,
                    "Failed to collect ephemeral warm relays during logout: {error}"
                );
                Vec::new()
            });

        // Signal cancellation first so in-flight handlers (e.g. contact-list
        // guard) see the flag, but keep the session visible until subscription
        // teardown completes — handlers that check get_session() during teardown
        // need the entry to exist.
        if let Some(session) = self.account_manager.get_session(pubkey) {
            session.cancel();
            session.deactivate_subscriptions().await;
        }

        self.account_manager.remove_session(pubkey);

        // Evict rate-limiter entries for this account to prevent unbounded growth.
        // Runs after subscription teardown to minimise the repopulation window.
        self.shared
            .token_request_timestamps
            .retain(|(account_pk, _, _, _), _| account_pk != pubkey);

        if !ephemeral_warm_relays.is_empty()
            && let Err(error) = self
                .shared
                .relay_control
                .unwarm_ephemeral_relays(&ephemeral_warm_relays)
                .await
        {
            tracing::warn!(
                target: "whitenoise::accounts",
                account_pubkey = %pubkey,
                "Failed to unwarm anonymous ephemeral relays during logout: {error}"
            );
        }

        // Delete the account from the database
        account.delete(&self.shared.database).await?;
        self.delete_mdk_storage_for_account(pubkey).await?;

        // Sync discovery subscriptions with remaining accounts (tears down on last logout)
        if let Err(e) = self.sync_discovery_subscriptions().await {
            tracing::warn!(
                target: "whitenoise::accounts",
                account_pubkey = %pubkey,
                "Failed to refresh discovery subscriptions after logout: {e}"
            );
        }

        // Remove the private key from the secret store
        // For local accounts this is required; for external accounts this is best-effort cleanup
        let result = self
            .shared
            .secrets_store
            .remove_private_key_for_pubkey(pubkey);
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
        let accounts = Account::all(&self.shared.database).await?;
        Ok(accounts.len())
    }

    /// Retrieves all accounts stored in the database.
    ///
    /// This method returns all accounts that have been created or imported into
    /// the Whitenoise instance. Each account represents a distinct identity with
    /// its own keypair, relay configurations, and associated data.
    pub async fn all_accounts(&self) -> Result<Vec<Account>> {
        Account::all(&self.shared.database).await
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
        Account::find_by_pubkey(pubkey, &self.shared.database).await
    }
}

#[cfg(test)]
mod tests {
    use mdk_sqlite_storage::keyring;
    use nostr_sdk::prelude::*;

    use crate::RelayType;
    use crate::whitenoise::accounts::Account;
    use crate::whitenoise::key_packages::{MLS_KEY_PACKAGE_KIND, MLS_KEY_PACKAGE_KIND_LEGACY};
    use crate::whitenoise::relays::Relay;
    use crate::whitenoise::test_utils::*;
    use crate::whitenoise::{Whitenoise, WhitenoiseConfig};

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Verify that an account has all three relay lists properly configured with defaults.
    async fn verify_account_relay_lists_setup(
        whitenoise: &crate::whitenoise::Whitenoise,
        account: &Account,
    ) {
        let default_relays = Relay::defaults();
        let default_relay_count = default_relays.len();

        assert_eq!(
            account
                .nip65_relays(&whitenoise.shared)
                .await
                .unwrap()
                .len(),
            default_relay_count,
            "Account should have default NIP-65 relays configured"
        );
        assert_eq!(
            account
                .inbox_relays(&whitenoise.shared)
                .await
                .unwrap()
                .len(),
            default_relay_count,
            "Account should have default inbox relays configured"
        );
        assert_eq!(
            account
                .key_package_relays(&whitenoise.shared)
                .await
                .unwrap()
                .len(),
            default_relay_count,
            "Account should have default key package relays configured"
        );

        let default_relays_vec: Vec<RelayUrl> = Relay::urls(&default_relays);
        let nip65_relay_urls: Vec<RelayUrl> =
            Relay::urls(&account.nip65_relays(&whitenoise.shared).await.unwrap());
        let inbox_relay_urls: Vec<RelayUrl> =
            Relay::urls(&account.inbox_relays(&whitenoise.shared).await.unwrap());
        let key_package_relay_urls: Vec<RelayUrl> = Relay::urls(
            &account
                .key_package_relays(&whitenoise.shared)
                .await
                .unwrap(),
        );
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

    /// Verify that an account has a key package published.
    #[allow(deprecated)]
    async fn verify_account_key_package_exists(
        whitenoise: &crate::whitenoise::Whitenoise,
        account: &Account,
    ) {
        let key_package_events = whitenoise
            .fetch_all_key_packages_for_account(account)
            .await
            .unwrap();

        assert!(
            !key_package_events.is_empty(),
            "Account should have a key package published to relays"
        );

        for event in &key_package_events {
            assert_eq!(
                event.pubkey, account.pubkey,
                "Key package should be authored by the account's public key"
            );
        }

        assert!(
            key_package_events
                .iter()
                .any(|event| event.kind == MLS_KEY_PACKAGE_KIND),
            "Account should publish a canonical key package (kind 30443)"
        );
        assert!(
            key_package_events
                .iter()
                .any(|event| event.kind == MLS_KEY_PACKAGE_KIND_LEGACY),
            "Account should publish a legacy key package twin (kind 443)"
        );
    }

    // -----------------------------------------------------------------------
    // login / create_identity tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_login_after_delete_all_data() {
        let (whitenoise, data_temp, logs_temp) = create_mock_whitenoise().await;

        let account = setup_login_account(&whitenoise).await;
        let secret_hex = account.1.secret_key().to_secret_hex();
        whitenoise.delete_all_data().await.unwrap();
        drop(whitenoise);

        // After delete_all_data the database pool is closed, so a fresh
        // instance is required for subsequent logins.
        let config = WhitenoiseConfig::new(data_temp.path(), logs_temp.path(), "wn.test.relogin");
        let whitenoise = Whitenoise::new(config).await.unwrap();
        let _acc = whitenoise.login(secret_hex).await.unwrap();
    }

    #[tokio::test]
    async fn test_create_identity_publishes_relay_lists() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a new identity
        let account = whitenoise.create_identity().await.unwrap();

        // Give the events time to be published and processed
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let nip65_relays = account.nip65_relays(&whitenoise.shared).await.unwrap();
        let nip65_relay_urls = Relay::urls(&nip65_relays);
        // Check that all three event types were published
        let inbox_events = whitenoise
            .shared
            .relay_control
            .fetch_user_relays(account.pubkey, RelayType::Inbox, &nip65_relay_urls)
            .await
            .unwrap();

        let key_package_relays_events = whitenoise
            .shared
            .relay_control
            .fetch_user_relays(account.pubkey, RelayType::KeyPackage, &nip65_relay_urls)
            .await
            .unwrap();

        let key_package_events = whitenoise
            .shared
            .relay_control
            .fetch_user_key_package(
                account.pubkey,
                &Relay::urls(&account.nip65_relays(&whitenoise.shared).await.unwrap()),
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
            "Key package (kind 30443) should be published for new accounts"
        );
    }

    #[tokio::test]
    async fn test_create_identity_sets_up_all_requirements() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a new identity
        let account = whitenoise.create_identity().await.unwrap();

        // New identities must be marked as synced immediately so they don't
        // poison compute_global_since_timestamp() for other accounts.
        assert!(
            account.last_synced_at.is_some(),
            "New identity should have last_synced_at set to prevent global subscription poisoning"
        );
        let db_account = Account::find_by_pubkey(&account.pubkey, &whitenoise.shared.database)
            .await
            .unwrap();
        assert!(
            db_account.last_synced_at.is_some(),
            "New identity last_synced_at should be persisted in database"
        );

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
    async fn test_create_identity_creates_session() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();

        assert!(
            whitenoise
                .account_manager
                .get_session(&account.pubkey)
                .is_some(),
            "activate_account should create a session"
        );
    }

    #[tokio::test]
    async fn test_logout_signals_cancellation_and_removes_session() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();

        let cancel_rx = whitenoise
            .account_manager
            .get_session(&account.pubkey)
            .expect("session should exist after login")
            .subscribe_cancellation();

        assert!(
            !*cancel_rx.borrow(),
            "should not be cancelled before logout"
        );

        whitenoise.logout(&account.pubkey).await.unwrap();

        assert!(
            *cancel_rx.borrow(),
            "logout should signal cancellation to running background tasks"
        );

        assert!(
            whitenoise
                .account_manager
                .get_session(&account.pubkey)
                .is_none(),
            "logout should remove the session"
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

    #[tokio::test]
    async fn test_update_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_test_account(&whitenoise).await;
        account.save(&whitenoise.shared.database).await.unwrap();

        whitenoise
            .shared
            .secrets_store
            .store_private_key(&keys)
            .unwrap();

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

        let user = account.user(&whitenoise.shared.database).await.unwrap();
        assert_eq!(user.metadata.name, new_metadata.name);
        assert_eq!(user.metadata.display_name, new_metadata.display_name);
        assert_eq!(user.metadata.about, new_metadata.about);

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let nip65_relays = account.nip65_relays(&whitenoise.shared).await.unwrap();
        let nip65_relay_urls = Relay::urls(&nip65_relays);
        let fetched_metadata = whitenoise
            .shared
            .relay_control
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

    /// Test logout removes keys correctly for both account types
    #[tokio::test]
    async fn test_logout_key_cleanup() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Local account logout removes key
        let (local_account, keys) = create_test_account(&whitenoise).await;
        local_account
            .save(&whitenoise.shared.database)
            .await
            .unwrap();
        whitenoise
            .shared
            .secrets_store
            .store_private_key(&keys)
            .unwrap();

        assert!(
            whitenoise
                .shared
                .secrets_store
                .get_nostr_keys_for_pubkey(&local_account.pubkey)
                .is_ok()
        );
        whitenoise.logout(&local_account.pubkey).await.unwrap();
        assert!(
            whitenoise
                .shared
                .secrets_store
                .get_nostr_keys_for_pubkey(&local_account.pubkey)
                .is_err()
        );

        // External account logout with stale key cleans up
        let ext_keys = create_test_keys();
        let ext_account = Account::new_external(&whitenoise, ext_keys.public_key())
            .await
            .unwrap();
        ext_account.save(&whitenoise.shared.database).await.unwrap();
        whitenoise
            .shared
            .secrets_store
            .store_private_key(&ext_keys)
            .unwrap(); // Stale key

        whitenoise.logout(&ext_account.pubkey).await.unwrap();
        assert!(
            whitenoise
                .shared
                .secrets_store
                .get_nostr_keys_for_pubkey(&ext_account.pubkey)
                .is_err()
        );

        // External account logout without key succeeds
        let ext2 = Account::new_external(&whitenoise, Keys::generate().public_key())
            .await
            .unwrap();
        ext2.save(&whitenoise.shared.database).await.unwrap();
        assert!(whitenoise.logout(&ext2.pubkey).await.is_ok());
    }

    #[tokio::test]
    async fn test_logout_local_account_removes_key() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create a local account directly in the database (bypassing relay setup)
        let (account, keys) = create_test_account(&whitenoise).await;
        account.save(&whitenoise.shared.database).await.unwrap();

        assert_eq!(
            account.account_type,
            crate::whitenoise::accounts::AccountType::Local,
            "Account should be Local type"
        );

        // Store the key in secrets store
        whitenoise
            .shared
            .secrets_store
            .store_private_key(&keys)
            .unwrap();

        // Verify the key is stored
        let stored_keys = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey);
        assert!(stored_keys.is_ok(), "Key should be stored after login");

        // Logout should remove the key
        whitenoise.logout(&account.pubkey).await.unwrap();

        // Verify the key was removed
        let stored_keys_after = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey);
        assert!(
            stored_keys_after.is_err(),
            "Key should be removed after logout"
        );
    }

    #[tokio::test]
    async fn test_logout_removes_mdk_storage_and_key() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_test_account(&whitenoise).await;
        account.save(&whitenoise.shared.database).await.unwrap();
        whitenoise
            .shared
            .secrets_store
            .store_private_key(&keys)
            .unwrap();

        let mls_storage_dir =
            Account::mdk_storage_path(&account.pubkey, &whitenoise.config().data_dir);
        tokio::fs::create_dir_all(&mls_storage_dir).await.unwrap();
        tokio::fs::write(mls_storage_dir.join("storage.sqlite"), b"test")
            .await
            .unwrap();

        let keyring_service_id = whitenoise.keyring_service_id().to_string();
        let db_key_id = Account::mdk_db_key_id(&account.pubkey);
        keyring::get_or_create_db_key(&keyring_service_id, &db_key_id)
            .expect("Failed to create MDK database key");

        whitenoise.logout(&account.pubkey).await.unwrap();

        assert!(!mls_storage_dir.exists());
        assert!(
            keyring::get_db_key(&keyring_service_id, &db_key_id)
                .unwrap()
                .is_none(),
            "Account logout should remove the account-scoped MDK database key"
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
        account.save(&whitenoise.shared.database).await.unwrap();

        assert_eq!(
            account.account_type,
            crate::whitenoise::accounts::AccountType::External,
            "Account should be External type"
        );

        // Manually store a stale key (simulating orphaned key from failed migration)
        whitenoise
            .shared
            .secrets_store
            .store_private_key(&keys)
            .unwrap();

        // Verify the stale key is stored
        let stored_keys = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&pubkey);
        assert!(stored_keys.is_ok(), "Stale key should be stored");

        // Logout should clean up the stale key via best-effort removal
        whitenoise.logout(&pubkey).await.unwrap();

        // Verify the stale key was removed
        let stored_keys_after = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&pubkey);
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
        account.save(&whitenoise.shared.database).await.unwrap();

        assert_eq!(
            account.account_type,
            crate::whitenoise::accounts::AccountType::External,
            "Account should be External type"
        );

        // Don't store any key - verify there's no key
        let stored_keys = whitenoise
            .shared
            .secrets_store
            .get_nostr_keys_for_pubkey(&pubkey);
        assert!(stored_keys.is_err(), "No key should be stored");

        // Logout should succeed even with no key to remove
        let result = whitenoise.logout(&pubkey).await;
        assert!(
            result.is_ok(),
            "Logout should succeed for external account without stored key"
        );
    }

    #[tokio::test]
    async fn test_logout_syncs_discovery_subscriptions() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();
        // Flush fire-and-forget rebuild (worker handles this in production)
        whitenoise.sync_discovery_subscriptions().await.unwrap();

        // After login, global discovery subscriptions should be active
        assert!(
            whitenoise
                .is_global_subscriptions_operational()
                .await
                .unwrap(),
            "Global subscriptions should be operational after login"
        );

        whitenoise.logout(&account.pubkey).await.unwrap();

        // After logging out the last account, zero accounts with zero
        // subscriptions is the correct retired state (healthy).
        assert!(
            whitenoise
                .is_global_subscriptions_operational()
                .await
                .unwrap(),
            "Zero accounts with zero subscriptions should be healthy after logout"
        );
    }

    /// Verifies that logging out evicts all rate-limiter entries for that account,
    /// while preserving entries for other accounts.
    #[tokio::test]
    async fn test_logout_evicts_rate_limiter_entries() {
        use crate::whitenoise::push_notifications::TokenRateKind;
        use mdk_core::prelude::GroupId;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account_a = whitenoise.create_identity().await.unwrap();
        let account_b = whitenoise.create_identity().await.unwrap();

        let group_id = GroupId::from_slice(&[1u8; 32]);

        // Seed entries for both accounts
        whitenoise.shared.token_request_timestamps.insert(
            (
                account_a.pubkey,
                group_id.clone(),
                0,
                TokenRateKind::Request,
            ),
            std::time::Instant::now(),
        );
        whitenoise.shared.token_request_timestamps.insert(
            (
                account_b.pubkey,
                group_id.clone(),
                0,
                TokenRateKind::Request,
            ),
            std::time::Instant::now(),
        );

        assert_eq!(
            whitenoise.shared.token_request_timestamps.len(),
            2,
            "both entries should be present before logout"
        );

        whitenoise.logout(&account_a.pubkey).await.unwrap();

        // account_a's entry must be gone
        assert!(
            !whitenoise.shared.token_request_timestamps.contains_key(&(
                account_a.pubkey,
                group_id.clone(),
                0,
                TokenRateKind::Request
            )),
            "logout should remove rate-limiter entries for the logged-out account"
        );

        // account_b's entry must survive
        assert!(
            whitenoise.shared.token_request_timestamps.contains_key(&(
                account_b.pubkey,
                group_id,
                0,
                TokenRateKind::Request
            )),
            "logout should not remove rate-limiter entries for other accounts"
        );
    }

    /// Test upload_profile_picture uploads to blossom server and returns URL.
    /// Requires blossom server running on localhost:3000.
    #[ignore]
    #[tokio::test]
    async fn test_upload_profile_picture() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create and persist account with stored keys
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = whitenoise.persist_account(&account).await.unwrap();
        whitenoise
            .shared
            .secrets_store
            .store_private_key(&keys)
            .unwrap();

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

    /// Test upload_profile_picture fails gracefully with non-existent file.
    #[ignore]
    #[tokio::test]
    async fn test_upload_profile_picture_nonexistent_file() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (account, keys) = create_test_account(&whitenoise).await;
        let account = whitenoise.persist_account(&account).await.unwrap();
        whitenoise
            .shared
            .secrets_store
            .store_private_key(&keys)
            .unwrap();

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
}
