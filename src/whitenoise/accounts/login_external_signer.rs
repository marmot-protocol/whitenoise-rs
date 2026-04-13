use nostr_sdk::prelude::*;

use super::{Account, AccountType, LoginError, LoginResult, LoginStatus};
use crate::RelayType;
use crate::perf_instrument;
use crate::whitenoise::relays::Relay;
use crate::whitenoise::users::User;
use crate::whitenoise::{Whitenoise, WhitenoiseError};

impl Whitenoise {
    // -----------------------------------------------------------------------
    // Multi-step login for external signers
    // -----------------------------------------------------------------------

    /// Step 1 for external signer login.
    ///
    /// Behaves like [`Whitenoise::login_start`] but takes a public key and a
    /// [`NostrSigner`] instead of a private key string.
    #[perf_instrument("accounts")]
    pub async fn login_external_signer_start<S>(
        &self,
        pubkey: PublicKey,
        signer: S,
    ) -> core::result::Result<LoginResult, LoginError>
    where
        S: NostrSigner + Clone + 'static,
    {
        tracing::debug!(
            target: "whitenoise::accounts",
            "Starting external signer login for {}",
            pubkey.to_hex()
        );

        self.validate_signer_pubkey(&pubkey, &signer)
            .await
            .map_err(LoginError::from)?;

        // Create/update the account for this pubkey.
        let (mut account, _) = self
            .setup_external_signer_account_without_relays(pubkey)
            .await?;

        let default_relays = self.load_default_relays().await.map_err(LoginError::from)?;

        let discovered = self
            .try_discover_relay_lists(&mut account, &default_relays)
            .await?;

        if discovered.is_complete() {
            // Happy path -- complete the login using the external signer.
            self.complete_external_signer_login(
                &account,
                discovered.relays(RelayType::Inbox),
                signer,
            )
            .await?;
            tracing::info!(
                target: "whitenoise::accounts",
                "Login complete for {}",
                pubkey.to_hex()
            );
            Ok(LoginResult {
                account,
                status: LoginStatus::Complete,
            })
        } else {
            // Stash the signer and partial discovery results so continuation
            // methods can use them.
            self.insert_external_signer(pubkey, signer)
                .await
                .map_err(LoginError::from)?;
            tracing::info!(
                target: "whitenoise::accounts",
                "Relay lists incomplete for {} (nip65={}, inbox={}, key_package={}); awaiting user decision",
                pubkey.to_hex(),
                discovered.found(RelayType::Nip65),
                discovered.found(RelayType::Inbox),
                discovered.found(RelayType::KeyPackage),
            );
            self.account_manager.stash_pending_login(pubkey, discovered);
            Ok(LoginResult {
                account,
                status: LoginStatus::NeedsRelayLists,
            })
        }
    }

    /// Step 2a for external signer: publish default relay lists for any that are missing,
    /// then complete login.
    ///
    /// Uses the partial discovery results stashed during step 1 to determine which
    /// of the three relay list kinds (10002, 10050, 10051) were already found on the
    /// network.  Default relays are assigned and published **only for the missing
    /// ones**; existing lists are left untouched.
    #[perf_instrument("accounts")]
    pub async fn login_external_signer_publish_default_relays(
        &self,
        pubkey: &PublicKey,
    ) -> core::result::Result<LoginResult, LoginError> {
        let discovered = self
            .account_manager
            .get_pending_login(pubkey)
            .ok_or(LoginError::NoLoginInProgress)?;

        let signer = self
            .get_external_signer(pubkey)
            .ok_or(LoginError::Internal(
                "External signer not found for pending login".to_string(),
            ))?;

        tracing::debug!(
            target: "whitenoise::accounts",
            "Publishing missing default relay lists for {} (nip65_missing={}, inbox_missing={}, key_package_missing={})",
            pubkey.to_hex(),
            !discovered.found(RelayType::Nip65),
            !discovered.found(RelayType::Inbox),
            !discovered.found(RelayType::KeyPackage),
        );

        let account = Account::find_by_pubkey(pubkey, &self.database)
            .await
            .map_err(LoginError::from)?;
        let default_relays = self.load_default_relays().await.map_err(LoginError::from)?;
        let user = account
            .user(&self.database)
            .await
            .map_err(LoginError::from)?;

        // Use discovered NIP-65 relays as the publish target when available,
        // otherwise defaults.
        let publish_to_urls = Relay::urls(discovered.relays_or(RelayType::Nip65, &default_relays));
        let default_urls = Relay::urls(&default_relays);

        // For each relay type: publish defaults only for the ones that were
        // missing. Already-found lists were already persisted to the DB by
        // try_discover_relay_lists in step 1, so no second write is needed.
        for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
            if !discovered.found(relay_type) {
                // Missing — assign defaults in the DB and publish via the signer.
                user.add_relays(&default_relays, relay_type, &self.database)
                    .await
                    .map_err(LoginError::from)?;
                if let Err(error) = self
                    .relay_control
                    .publish_relay_list_with_signer(
                        &default_urls,
                        relay_type,
                        &publish_to_urls,
                        std::sync::Arc::new(signer.clone()),
                    )
                    .await
                {
                    if discovered.nip65.is_none() || relay_type == RelayType::Nip65 {
                        return Err(LoginError::from(WhitenoiseError::from(error)));
                    }

                    tracing::warn!(
                        target: "whitenoise::accounts",
                        pubkey = %pubkey,
                        ?relay_type,
                        "Failed to publish default relay list via external signer to preserved NIP-65 relays; continuing login with local relay state: {error}"
                    );
                }
            } else {
                tracing::debug!(
                    target: "whitenoise::accounts",
                    "Skipping publish for {:?} — already exists on network",
                    relay_type,
                );
            }
        }

        self.complete_external_signer_login(
            &account,
            discovered.relays_or(RelayType::Inbox, &default_relays),
            signer,
        )
        .await?;

        self.account_manager.take_pending_login(pubkey);
        tracing::info!(
            target: "whitenoise::accounts",
            "Login complete for {}",
            pubkey.to_hex()
        );
        Ok(LoginResult {
            account,
            status: LoginStatus::Complete,
        })
    }

    /// Step 2b for external signer: search a custom relay for existing relay lists.
    #[perf_instrument("accounts")]
    pub async fn login_external_signer_with_custom_relay(
        &self,
        pubkey: &PublicKey,
        relay_url: RelayUrl,
    ) -> core::result::Result<LoginResult, LoginError> {
        if !self.account_manager.has_pending_login(pubkey) {
            return Err(LoginError::NoLoginInProgress);
        }

        let signer = self
            .get_external_signer(pubkey)
            .ok_or(LoginError::Internal(
                "External signer not found for pending login".to_string(),
            ))?;

        let mut account = Account::find_by_pubkey(pubkey, &self.database)
            .await
            .map_err(LoginError::from)?;

        let custom_relay = self
            .find_or_create_relay_by_url(&relay_url)
            .await
            .map_err(LoginError::from)?;
        let source_relays = vec![custom_relay];

        let discovered = self
            .try_discover_relay_lists(&mut account, &source_relays)
            .await?;

        // Same upfront-merge pattern as login_with_custom_relay: update the
        // stash before attempting completion so a failed complete_external_signer_login
        // leaves the stash accurate for any subsequent retry.
        let merged = self.merge_into_stash(pubkey, discovered)?;

        if merged.is_complete() {
            self.sync_discovered_relay_lists(&account, &merged).await?;
            self.complete_external_signer_login(&account, merged.relays(RelayType::Inbox), signer)
                .await?;
            self.account_manager.take_pending_login(pubkey);
            tracing::info!(
                target: "whitenoise::accounts",
                "Login complete for {} (found lists on {})",
                pubkey.to_hex(),
                relay_url
            );
            Ok(LoginResult {
                account,
                status: LoginStatus::Complete,
            })
        } else {
            tracing::info!(
                target: "whitenoise::accounts",
                "Relay lists still incomplete after {} for {} (nip65={}, inbox={}, key_package={})",
                relay_url,
                pubkey.to_hex(),
                merged.found(RelayType::Nip65),
                merged.found(RelayType::Inbox),
                merged.found(RelayType::KeyPackage),
            );
            Ok(LoginResult {
                account,
                status: LoginStatus::NeedsRelayLists,
            })
        }
    }

    /// Activate an external-signer account after relay lists have been resolved.
    #[perf_instrument("accounts")]
    pub(super) async fn complete_external_signer_login<S>(
        &self,
        account: &Account,
        inbox_relays: &[Relay],
        signer: S,
    ) -> core::result::Result<(), LoginError>
    where
        S: NostrSigner + Clone + 'static,
    {
        // Register the signer before activating the account so that subscription
        // setup can use it for NIP-42 AUTH on relays that require it.
        self.insert_external_signer(account.pubkey, signer.clone())
            .await
            .map_err(LoginError::from)?;

        self.activate_account_without_publishing(account, inbox_relays)
            .await
            .map_err(LoginError::from)?;

        self.publish_key_package_for_account_with_signer(account, signer)
            .await
            .map_err(LoginError::from)?;

        Ok(())
    }

    /// Create/update an external signer account record without setting up relays.
    /// Used by the multi-step external signer login flow.
    pub(super) async fn setup_external_signer_account_without_relays(
        &self,
        pubkey: PublicKey,
    ) -> core::result::Result<(Account, User), LoginError> {
        let account = match Account::find_by_pubkey(&pubkey, &self.database).await {
            Ok(existing) => {
                let mut account_mut = existing.clone();
                if account_mut.account_type != AccountType::External {
                    tracing::info!(
                        target: "whitenoise::accounts",
                        "Migrating account from {:?} to External",
                        account_mut.account_type
                    );
                    account_mut.account_type = AccountType::External;
                    account_mut = self
                        .persist_account(&account_mut)
                        .await
                        .map_err(LoginError::from)?;
                }
                let _ = self
                    .secrets_store
                    .remove_private_key_for_pubkey(&account_mut.pubkey);
                account_mut
            }
            Err(_) => {
                let account = Account::new_external(self, pubkey)
                    .await
                    .map_err(LoginError::from)?;
                self.persist_account(&account)
                    .await
                    .map_err(LoginError::from)?
            }
        };

        let user = account
            .user(&self.database)
            .await
            .map_err(LoginError::from)?;
        Ok((account, user))
    }

    /// Sets up an external signer account (creates or updates) and configures relays.
    ///
    /// Only used by `login_with_external_signer_for_test` — the production
    /// multi-step flow uses `setup_external_signer_account_without_relays`
    /// instead.
    pub(super) async fn setup_external_signer_account(
        &self,
        pubkey: PublicKey,
    ) -> crate::whitenoise::error::Result<(Account, super::ExternalSignerRelaySetup)> {
        // Check if account already exists
        let mut account =
            if let Ok(existing) = Account::find_by_pubkey(&pubkey, &self.database).await {
                tracing::debug!(
                    target: "whitenoise::accounts",
                    "Found existing account"
                );

                let mut account_mut = existing.clone();

                // Handle migration from Local to External account type
                if account_mut.account_type != AccountType::External {
                    tracing::info!(
                        target: "whitenoise::accounts",
                        "Migrating account from {:?} to External",
                        account_mut.account_type
                    );
                    account_mut.account_type = AccountType::External;
                    account_mut = self.persist_account(&account_mut).await?;
                }

                // Best-effort removal of any local keys
                let _ = self
                    .secrets_store
                    .remove_private_key_for_pubkey(&account_mut.pubkey);

                account_mut
            } else {
                // Create new external signer account
                tracing::debug!(
                    target: "whitenoise::accounts",
                    "Creating new external signer account"
                );
                let account = Account::new_external(self, pubkey).await?;
                self.persist_account(&account).await?
            };

        // Setup relays
        let relay_setup = self
            .setup_relays_for_external_signer_account(&mut account)
            .await?;

        Ok((account, relay_setup))
    }

    /// Validates that an external signer's pubkey matches the expected pubkey.
    ///
    /// This prevents publishing relay lists or key packages under a wrong identity.
    pub(crate) async fn validate_signer_pubkey(
        &self,
        expected_pubkey: &PublicKey,
        signer: &impl NostrSigner,
    ) -> crate::whitenoise::error::Result<()> {
        let signer_pubkey = signer.get_public_key().await.map_err(|e| {
            WhitenoiseError::Internal(format!("Failed to get signer pubkey: {}", e))
        })?;
        if signer_pubkey != *expected_pubkey {
            return Err(WhitenoiseError::Internal(format!(
                "External signer pubkey mismatch: expected {}, got {}",
                expected_pubkey.to_hex(),
                signer_pubkey.to_hex()
            )));
        }
        Ok(())
    }

    /// Publishes relay lists using an external signer based on the relay setup configuration.
    ///
    /// Publishes NIP-65, inbox, and key package relay lists only if they need to be
    /// published (i.e., using defaults rather than existing user-configured lists).
    pub(super) async fn publish_relay_lists_with_signer(
        &self,
        relay_setup: &super::ExternalSignerRelaySetup,
        signer: impl NostrSigner + 'static,
    ) -> crate::whitenoise::error::Result<()> {
        let nip65_urls = Relay::urls(&relay_setup.nip65_relays);
        let signer = std::sync::Arc::new(signer);

        // Publish all missing relay lists concurrently — each is an
        // independent Nostr event targeting the same relay set.
        let nip65_publish = async {
            if relay_setup.should_publish_nip65 {
                tracing::debug!(
                    target: "whitenoise::accounts",
                    "Publishing NIP-65 relay list (defaults)"
                );
                self.relay_control
                    .publish_relay_list_with_signer(
                        &nip65_urls,
                        RelayType::Nip65,
                        &nip65_urls,
                        signer.clone(),
                    )
                    .await
            } else {
                Ok(())
            }
        };
        let inbox_publish = async {
            if relay_setup.should_publish_inbox {
                tracing::debug!(
                    target: "whitenoise::accounts",
                    "Publishing inbox relay list (defaults)"
                );
                self.relay_control
                    .publish_relay_list_with_signer(
                        &Relay::urls(&relay_setup.inbox_relays),
                        RelayType::Inbox,
                        &nip65_urls,
                        signer.clone(),
                    )
                    .await
            } else {
                Ok(())
            }
        };
        let key_package_publish = async {
            if relay_setup.should_publish_key_package {
                tracing::debug!(
                    target: "whitenoise::accounts",
                    "Publishing key package relay list (defaults)"
                );
                self.relay_control
                    .publish_relay_list_with_signer(
                        &Relay::urls(&relay_setup.key_package_relays),
                        RelayType::KeyPackage,
                        &nip65_urls,
                        signer.clone(),
                    )
                    .await
            } else {
                Ok(())
            }
        };

        let (r1, r2, r3) = tokio::join!(nip65_publish, inbox_publish, key_package_publish);
        r1?;
        for (relay_type, result) in [(RelayType::Inbox, r2), (RelayType::KeyPackage, r3)] {
            if let Err(error) = result {
                tracing::warn!(
                    target: "whitenoise::accounts",
                    ?relay_type,
                    "Failed to publish relay list with signer; continuing with local relay state: {error}"
                );
            }
        }

        Ok(())
    }

    /// Test-only: Sets up an external signer account without publishing.
    ///
    /// This is used by tests that only need to verify account creation/migration
    /// logic without needing a real signer for publishing. The provided keys are
    /// used as the mock signer (their pubkey must match `keys.public_key()`).
    #[cfg(test)]
    pub(crate) async fn login_with_external_signer_for_test(
        &self,
        keys: Keys,
    ) -> crate::whitenoise::error::Result<Account> {
        let pubkey = keys.public_key();
        let (account, relay_setup) = self.setup_external_signer_account(pubkey).await?;

        // Register the keys as a signer so subscription setup can proceed.
        // In production, the real signer is registered before activation.
        self.insert_external_signer(pubkey, keys).await?;

        self.activate_account_without_publishing(&account, &relay_setup.inbox_relays)
            .await?;

        Ok(account)
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::prelude::*;

    use crate::whitenoise::accounts::{Account, AccountType, ExternalSignerRelaySetup};
    use crate::whitenoise::test_utils::*;

    /// Test that login_with_external_signer rejects mismatched signer pubkey
    #[tokio::test]
    async fn test_login_with_external_signer_rejects_mismatched_pubkey() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let expected_keys = Keys::generate();
        let wrong_keys = Keys::generate();
        let expected_pubkey = expected_keys.public_key();

        let result = whitenoise
            .login_with_external_signer(expected_pubkey, wrong_keys)
            .await;

        assert!(result.is_err(), "Should reject mismatched signer pubkey");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("pubkey mismatch"),
            "Error should mention pubkey mismatch, got: {}",
            err_msg
        );
    }

    /// Test that login_with_external_signer accepts matching signer pubkey
    #[tokio::test]
    async fn test_login_with_external_signer_accepts_matching_pubkey() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let result = whitenoise.login_with_external_signer(pubkey, keys).await;

        // If it fails, it should NOT be due to pubkey mismatch
        if let Err(ref e) = result {
            let err_msg = format!("{}", e);
            assert!(
                !err_msg.contains("pubkey mismatch"),
                "Should not fail due to pubkey mismatch when keys match"
            );
        }
        // Success is also acceptable (if relays are connected)
    }

    /// Test that external signer login doesn't store any private key
    #[tokio::test]
    async fn test_login_with_external_signer_has_no_local_keys() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let _account = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        let result = whitenoise.secrets_store.get_nostr_keys_for_pubkey(&pubkey);
        assert!(
            result.is_err(),
            "External signer account should not have local keys"
        );
    }

    /// Test setup_external_signer_account handles fresh account
    #[tokio::test]
    async fn test_setup_external_signer_account_fresh() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let (account, relay_setup) = whitenoise
            .setup_external_signer_account(pubkey)
            .await
            .unwrap();

        assert_eq!(account.account_type, AccountType::External);
        assert_eq!(account.pubkey, pubkey);
        assert!(
            relay_setup.should_publish_nip65
                || relay_setup.should_publish_inbox
                || relay_setup.should_publish_key_package
                || !relay_setup.nip65_relays.is_empty(),
            "Relay setup should have relays or publish flags"
        );
    }

    /// Test login_with_external_signer_for_test is idempotent
    #[tokio::test]
    async fn test_login_with_external_signer_idempotent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        let account1 = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();
        let account2 = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        assert_eq!(account1.pubkey, account2.pubkey);
        assert_eq!(account1.account_type, account2.account_type);

        let count = whitenoise.get_accounts_count().await.unwrap();
        assert_eq!(
            count, 1,
            "Should have exactly one account after duplicate login"
        );
    }

    #[tokio::test]
    async fn test_login_with_external_signer_double_login_returns_existing_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let first_account = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        let second_account = whitenoise
            .login_with_external_signer(pubkey, keys)
            .await
            .unwrap();

        assert_eq!(first_account.id, second_account.id);
        assert_eq!(first_account.pubkey, second_account.pubkey);

        let count = whitenoise.get_accounts_count().await.unwrap();
        assert_eq!(count, 1, "Double login should not create a second account");
    }

    /// Test that Account helper methods work correctly for external accounts
    #[tokio::test]
    async fn test_external_account_helper_methods() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        let account = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        assert!(
            account.uses_external_signer(),
            "External account should report using external signer"
        );
        assert!(
            !account.has_local_key(),
            "External account should not have local key"
        );
    }

    /// Test login_with_external_signer creates new external account
    #[tokio::test]
    async fn test_login_with_external_signer_new_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = create_test_keys();
        let pubkey = keys.public_key();

        let account = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        assert_eq!(account.pubkey, pubkey);
        assert_eq!(account.account_type, AccountType::External);
        assert!(account.id.is_some(), "Account should be persisted");
        assert!(account.uses_external_signer(), "Should use external signer");
        assert!(!account.has_local_key(), "Should not have local key");

        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&pubkey)
                .is_err(),
            "External account should not have stored private key"
        );

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

        let account1 = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();
        assert!(account1.id.is_some());

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

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

        let keys = create_test_keys();
        let pubkey = keys.public_key();

        let (local_account, _) = Account::new(&whitenoise, Some(keys.clone())).await.unwrap();
        assert_eq!(local_account.account_type, AccountType::Local);
        whitenoise.persist_account(&local_account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

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

        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        whitenoise.persist_account(&account).await.unwrap();

        whitenoise.secrets_store.store_private_key(&keys).unwrap();
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&pubkey)
                .is_ok()
        );

        whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&pubkey)
                .is_err(),
            "Stale key should be removed during external signer login"
        );
    }

    /// Test that validate_signer_pubkey succeeds when pubkeys match
    #[tokio::test]
    async fn test_validate_signer_pubkey_matching() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

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

        let result = whitenoise
            .validate_signer_pubkey(&expected_pubkey, &wrong_keys)
            .await;

        assert!(result.is_err(), "Should fail with mismatched pubkeys");
        let err_msg = format!("{}", result.unwrap_err());
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

        let relay_setup = ExternalSignerRelaySetup {
            nip65_relays: vec![],
            inbox_relays: vec![],
            key_package_relays: vec![],
            should_publish_nip65: false,
            should_publish_inbox: false,
            should_publish_key_package: false,
        };

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

        let _account = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        let default_relays = whitenoise.load_default_relays().await.unwrap();

        let relay_setup = ExternalSignerRelaySetup {
            nip65_relays: default_relays.clone(),
            inbox_relays: default_relays.clone(),
            key_package_relays: default_relays,
            should_publish_nip65: true,
            should_publish_inbox: true,
            should_publish_key_package: true,
        };

        let result = whitenoise
            .publish_relay_lists_with_signer(&relay_setup, keys)
            .await;

        // Accept either success or acceptable network errors in the test environment
        if let Err(ref e) = result {
            let err_msg = format!("{}", e);
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

    // -----------------------------------------------------------------------
    // setup_external_signer_account_without_relays tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_setup_external_signer_account_new() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        let (account, user) = whitenoise
            .setup_external_signer_account_without_relays(pubkey)
            .await
            .unwrap();

        assert_eq!(account.pubkey, pubkey);
        assert_eq!(account.account_type, AccountType::External);
        assert!(account.id.is_some());
        assert_eq!(user.pubkey, pubkey);
    }

    #[tokio::test]
    async fn test_setup_external_signer_account_existing_external() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        let first = Account::new_external(&whitenoise, pubkey).await.unwrap();
        whitenoise.persist_account(&first).await.unwrap();

        let (account, _user) = whitenoise
            .setup_external_signer_account_without_relays(pubkey)
            .await
            .unwrap();

        assert_eq!(account.pubkey, pubkey);
        assert_eq!(account.account_type, AccountType::External);
    }

    #[tokio::test]
    async fn test_setup_external_signer_account_migrates_local_to_external() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (local_account, keys) = Account::new(&whitenoise, None).await.unwrap();
        let local_account = whitenoise.persist_account(&local_account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();
        assert_eq!(local_account.account_type, AccountType::Local);

        let (account, _user) = whitenoise
            .setup_external_signer_account_without_relays(local_account.pubkey)
            .await
            .unwrap();

        assert_eq!(account.account_type, AccountType::External);
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&account.pubkey)
                .is_err(),
            "Private key should be removed after migration to external"
        );
    }
}
