use nostr_sdk::prelude::*;

use super::{Account, AccountType, ExternalSignerRelaySetup, LoginError, LoginResult, LoginStatus};
use crate::RelayType;
use crate::whitenoise::error::Result;
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
                &discovered.nip65,
                &discovered.inbox,
                &discovered.key_package,
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
                !discovered.nip65.is_empty(),
                !discovered.inbox.is_empty(),
                !discovered.key_package.is_empty(),
            );
            self.pending_logins.insert(pubkey, discovered);
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
    pub async fn login_external_signer_publish_default_relays(
        &self,
        pubkey: &PublicKey,
    ) -> core::result::Result<LoginResult, LoginError> {
        let discovered = self
            .pending_logins
            .get(pubkey)
            .ok_or(LoginError::NoLoginInProgress)?
            .clone();

        let signer = self
            .get_external_signer(pubkey)
            .ok_or(LoginError::Internal(
                "External signer not found for pending login".to_string(),
            ))?;

        tracing::debug!(
            target: "whitenoise::accounts",
            "Publishing missing default relay lists for {} (nip65_missing={}, inbox_missing={}, key_package_missing={})",
            pubkey.to_hex(),
            discovered.nip65.is_empty(),
            discovered.inbox.is_empty(),
            discovered.key_package.is_empty(),
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
        // publish_to_urls is the target for relay-list events: use discovered
        // NIP-65 relays when available, otherwise defaults.
        let publish_to_urls = Relay::urls(discovered.relays_or(RelayType::Nip65, &default_relays));
        let default_urls = Relay::urls(&default_relays);

        // For each relay type: publish defaults only for the ones that were
        // missing. Already-found lists were already persisted to the DB by
        // try_discover_relay_lists in step 1, so no second write is needed.
        for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
            if discovered.relays(relay_type).is_empty() {
                // Missing — assign defaults in the DB and publish via the signer.
                user.add_relays(&default_relays, relay_type, &self.database)
                    .await
                    .map_err(LoginError::from)?;
                self.nostr
                    .publish_relay_list_with_signer(
                        &default_urls,
                        relay_type,
                        &publish_to_urls,
                        signer.clone(),
                    )
                    .await
                    .map_err(|e| LoginError::from(WhitenoiseError::from(e)))?;
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
            discovered.relays_or(RelayType::Nip65, &default_relays),
            discovered.relays_or(RelayType::Inbox, &default_relays),
            discovered.relays_or(RelayType::KeyPackage, &default_relays),
            signer,
        )
        .await?;

        self.pending_logins.remove(pubkey);
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
    pub async fn login_external_signer_with_custom_relay(
        &self,
        pubkey: &PublicKey,
        relay_url: RelayUrl,
    ) -> core::result::Result<LoginResult, LoginError> {
        if !self.pending_logins.contains_key(pubkey) {
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
            self.complete_external_signer_login(
                &account,
                &merged.nip65,
                &merged.inbox,
                &merged.key_package,
                signer,
            )
            .await?;
            self.pending_logins.remove(pubkey);
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
                !merged.nip65.is_empty(),
                !merged.inbox.is_empty(),
                !merged.key_package.is_empty(),
            );
            Ok(LoginResult {
                account,
                status: LoginStatus::NeedsRelayLists,
            })
        }
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

    // -----------------------------------------------------------------------
    // Original methods (preserved for backward compatibility during transition)
    // -----------------------------------------------------------------------

    /// Sets up an external signer account (creates or updates) and configures relays.
    /// Also used by tests that need account setup without publishing.
    pub(super) async fn setup_external_signer_account(
        &self,
        pubkey: PublicKey,
    ) -> Result<(Account, ExternalSignerRelaySetup)> {
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
    ) -> Result<()> {
        let signer_pubkey = signer.get_public_key().await.map_err(|e| {
            WhitenoiseError::Other(anyhow::anyhow!("Failed to get signer pubkey: {}", e))
        })?;
        if signer_pubkey != *expected_pubkey {
            return Err(WhitenoiseError::Other(anyhow::anyhow!(
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
        relay_setup: &ExternalSignerRelaySetup,
        signer: impl NostrSigner + Clone,
    ) -> Result<()> {
        let nip65_urls = Relay::urls(&relay_setup.nip65_relays);

        if relay_setup.should_publish_nip65 {
            tracing::debug!(
                target: "whitenoise::accounts",
                "Publishing NIP-65 relay list (defaults)"
            );
            self.nostr
                .publish_relay_list_with_signer(
                    &nip65_urls,
                    RelayType::Nip65,
                    &nip65_urls,
                    signer.clone(),
                )
                .await?;
        }

        if relay_setup.should_publish_inbox {
            tracing::debug!(
                target: "whitenoise::accounts",
                "Publishing inbox relay list (defaults)"
            );
            self.nostr
                .publish_relay_list_with_signer(
                    &Relay::urls(&relay_setup.inbox_relays),
                    RelayType::Inbox,
                    &nip65_urls,
                    signer.clone(),
                )
                .await?;
        }

        if relay_setup.should_publish_key_package {
            tracing::debug!(
                target: "whitenoise::accounts",
                "Publishing key package relay list (defaults)"
            );
            self.nostr
                .publish_relay_list_with_signer(
                    &Relay::urls(&relay_setup.key_package_relays),
                    RelayType::KeyPackage,
                    &nip65_urls,
                    signer.clone(),
                )
                .await?;
        }

        Ok(())
    }

    /// Test-only: Sets up an external signer account without publishing.
    ///
    /// This is used by tests that only need to verify account creation/migration
    /// logic without needing a real signer for publishing. The provided keys are
    /// used as the mock signer (their pubkey must match `keys.public_key()`).
    #[cfg(test)]
    pub(crate) async fn login_with_external_signer_for_test(&self, keys: Keys) -> Result<Account> {
        let pubkey = keys.public_key();
        let (account, relay_setup) = self.setup_external_signer_account(pubkey).await?;

        // Register the keys as a signer so subscription setup can proceed.
        // In production, the real signer is registered before activation.
        self.insert_external_signer(pubkey, keys).await?;

        let user = account.user(&self.database).await?;
        self.activate_account_without_publishing(
            &account,
            &user,
            &relay_setup.nip65_relays,
            &relay_setup.inbox_relays,
            &relay_setup.key_package_relays,
        )
        .await?;

        Ok(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RelayType;
    use crate::whitenoise::accounts::{Account, AccountType, DiscoveredRelayLists};
    use crate::whitenoise::test_utils::*;

    // -----------------------------------------------------------------------
    // Local test helpers (duplicated from login_multistep tests so each test
    // module is self-contained).
    // -----------------------------------------------------------------------

    /// Helper: create a nostr Client connected to the dev Docker relays and
    /// publish all three relay list events (10002, 10050, 10051) for the
    /// given keys.
    async fn publish_relay_lists_to_dev_relays(keys: &Keys) {
        let dev_relays = &["ws://localhost:8080", "ws://localhost:7777"];
        let client = Client::default();
        for relay in dev_relays {
            client.add_relay(*relay).await.unwrap();
        }
        client.connect().await;
        client.set_signer(keys.clone()).await;

        let relay_urls: Vec<String> = dev_relays.iter().map(|s| s.to_string()).collect();

        // NIP-65 (kind 10002): r tags
        let nip65_tags: Vec<Tag> = relay_urls
            .iter()
            .map(|url| {
                Tag::custom(
                    TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::R)),
                    [url.clone()],
                )
            })
            .collect();
        client
            .send_event_builder(EventBuilder::new(Kind::RelayList, "").tags(nip65_tags))
            .await
            .unwrap();

        // Inbox (kind 10050) and KeyPackage (kind 10051): relay tags
        let relay_tags: Vec<Tag> = relay_urls
            .iter()
            .map(|url| Tag::custom(TagKind::Relay, [url.clone()]))
            .collect();
        client
            .send_event_builder(EventBuilder::new(Kind::InboxRelays, "").tags(relay_tags.clone()))
            .await
            .unwrap();
        client
            .send_event_builder(EventBuilder::new(Kind::MlsKeyPackageRelays, "").tags(relay_tags))
            .await
            .unwrap();

        // Give the relays a moment to process.
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        client.disconnect().await;
    }

    /// Helper: create an external-signer account, register its signer, and
    /// insert a partial DiscoveredRelayLists into pending_logins so we can
    /// test the external-signer step-2a path in isolation.
    async fn setup_partial_pending_login_external_signer(
        whitenoise: &Whitenoise,
        keys: &Keys,
        discovered: DiscoveredRelayLists,
    ) {
        let pubkey = keys.public_key();
        whitenoise
            .setup_external_signer_account_without_relays(pubkey)
            .await
            .unwrap();
        whitenoise
            .insert_external_signer(pubkey, keys.clone())
            .await
            .unwrap();
        whitenoise.pending_logins.insert(pubkey, discovered);
    }

    /// Helper: like `setup_partial_pending_login_external_signer` but also
    /// writes relay associations to the DB.
    async fn setup_pending_login_external_signer_with_db_relays(
        whitenoise: &Whitenoise,
        keys: &Keys,
        discovered: DiscoveredRelayLists,
    ) {
        let pubkey = keys.public_key();
        let (account, _) = whitenoise
            .setup_external_signer_account_without_relays(pubkey)
            .await
            .unwrap();
        whitenoise
            .insert_external_signer(pubkey, keys.clone())
            .await
            .unwrap();
        let user = account.user(&whitenoise.database).await.unwrap();
        for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
            let relays = discovered.relays(relay_type);
            if !relays.is_empty() {
                user.add_relays(relays, relay_type, &whitenoise.database)
                    .await
                    .unwrap();
            }
        }
        whitenoise.pending_logins.insert(pubkey, discovered);
    }

    /// Test that login_with_external_signer rejects mismatched signer pubkey
    #[tokio::test]
    async fn test_login_with_external_signer_rejects_mismatched_pubkey() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create two different key pairs
        let expected_keys = Keys::generate();
        let wrong_keys = Keys::generate();
        let expected_pubkey = expected_keys.public_key();

        // Try to login with expected_pubkey but provide wrong_keys as signer
        // This should fail because the signer's pubkey doesn't match
        let result = whitenoise
            .login_with_external_signer(expected_pubkey, wrong_keys)
            .await;

        assert!(result.is_err(), "Should reject mismatched signer pubkey");
        let err = result.unwrap_err();
        let err_msg = format!("{}", err);
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

        // Login with matching pubkey and signer - validation should pass
        // Note: This test may still fail later due to relay connections,
        // but the pubkey validation itself should succeed
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

        // Login with external signer
        let _account = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        // Verify no keys stored in secrets store
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

        // Setup external signer account
        let (account, relay_setup) = whitenoise
            .setup_external_signer_account(pubkey)
            .await
            .unwrap();

        // Verify account created correctly
        assert_eq!(account.account_type, AccountType::External);
        assert_eq!(account.pubkey, pubkey);

        // For fresh account with no existing relays, should_publish flags should be true
        // (since defaults are used)
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

        // Login twice with same keys
        let account1 = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();
        let account2 = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        // Should be the same account
        assert_eq!(account1.pubkey, account2.pubkey);
        assert_eq!(account1.account_type, account2.account_type);

        // Should still only have one account
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

        // First login via test helper (sets up account in DB without needing relays)
        let first_account = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        // Second login via the real method — should hit the double-login guard
        // and return early without doing any relay work
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

    #[tokio::test]
    async fn test_login_external_signer_publish_defaults_without_start_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();
        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), LoginError::NoLoginInProgress));
    }

    #[tokio::test]
    async fn test_login_external_signer_custom_relay_without_start_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();
        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let result = whitenoise
            .login_external_signer_with_custom_relay(&pubkey, relay_url)
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), LoginError::NoLoginInProgress));
    }

    #[tokio::test]
    async fn test_login_external_signer_publish_defaults_no_signer() {
        // Pubkey is in pending_logins but no signer was registered.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        // Simulate pending state without a registered signer.
        whitenoise.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![],
                key_package: vec![],
            },
        );

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            LoginError::Internal(msg) => {
                assert!(
                    msg.contains("External signer not found"),
                    "Expected 'External signer not found' error, got: {}",
                    msg
                );
            }
            other => panic!("Expected LoginError::Internal, got: {:?}", other),
        }

        // Clean up
        whitenoise.pending_logins.remove(&pubkey);
    }

    #[tokio::test]
    async fn test_login_external_signer_custom_relay_no_signer() {
        // Pubkey is in pending_logins but no signer was registered.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        whitenoise.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![],
                key_package: vec![],
            },
        );

        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let result = whitenoise
            .login_external_signer_with_custom_relay(&pubkey, relay_url)
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            LoginError::Internal(msg) => {
                assert!(
                    msg.contains("External signer not found"),
                    "Expected 'External signer not found' error, got: {}",
                    msg
                );
            }
            other => panic!("Expected LoginError::Internal, got: {:?}", other),
        }

        whitenoise.pending_logins.remove(&pubkey);
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

        // Create an external account first.
        let first = Account::new_external(&whitenoise, pubkey).await.unwrap();
        whitenoise.persist_account(&first).await.unwrap();

        // Calling again should return the existing account without migration.
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

        // Create a local account.
        let (local_account, keys) = Account::new(&whitenoise, None).await.unwrap();
        let local_account = whitenoise.persist_account(&local_account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();
        assert_eq!(local_account.account_type, AccountType::Local);

        // setup_external_signer_account_without_relays should migrate it.
        let (account, _user) = whitenoise
            .setup_external_signer_account_without_relays(local_account.pubkey)
            .await
            .unwrap();

        assert_eq!(account.account_type, AccountType::External);
        // Private key should have been removed.
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&account.pubkey)
                .is_err(),
            "Private key should be removed after migration to external"
        );
    }

    #[tokio::test]
    async fn test_login_external_signer_start_no_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Don't publish anything — should return NeedsRelayLists.
        let result = whitenoise
            .login_external_signer_start(pubkey, keys.clone())
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::NeedsRelayLists);
        assert_eq!(result.account.pubkey, pubkey);
        assert_eq!(result.account.account_type, AccountType::External);
        assert!(whitenoise.pending_logins.contains_key(&pubkey));

        // Clean up.
        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    #[tokio::test]
    async fn test_login_external_signer_start_happy_path() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Pre-publish relay lists.
        publish_relay_lists_to_dev_relays(&keys).await;

        let result = whitenoise
            .login_external_signer_start(pubkey, keys.clone())
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(result.account.pubkey, pubkey);
        assert_eq!(result.account.account_type, AccountType::External);
    }

    // -----------------------------------------------------------------------
    // External signer — selective publish tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_all_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        setup_partial_pending_login_external_signer(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![],
                key_package: vec![],
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(result.account.account_type, AccountType::External);

        for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
            let relays = result
                .account
                .relays(relay_type, &whitenoise)
                .await
                .unwrap();
            assert!(
                !relays.is_empty(),
                "{:?} must be assigned when all lists were missing (external signer)",
                relay_type
            );
        }
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_preserves_found_nip65() {
        // External signer path: user has 10002 but not 10050/10051.
        // NIP-65 must be preserved; inbox and key_package get defaults.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_url = RelayUrl::parse("wss://custom-ext-nip65.example.com").unwrap();
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&nip65_url)
            .await
            .unwrap();

        setup_pending_login_external_signer_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![nip65_relay],
                inbox: vec![],
                key_package: vec![],
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);

        let stored_nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(
            stored_nip65[0].url, nip65_url,
            "NIP-65 must not be overwritten"
        );

        let inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert!(!inbox.is_empty(), "Inbox must receive defaults");

        let kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert!(!kp.is_empty(), "KeyPackage must receive defaults");
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_without_pending_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();
        let err = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap_err();
        assert!(
            matches!(err, LoginError::NoLoginInProgress),
            "Must return NoLoginInProgress when no stash exists"
        );
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_removes_pending_entry() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        setup_partial_pending_login_external_signer(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![],
                key_package: vec![],
            },
        )
        .await;

        whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();

        assert!(
            !whitenoise.pending_logins.contains_key(&pubkey),
            "pending_logins must be cleared after external signer publish"
        );
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_no_signer_registered() {
        // Stash present but signer not registered → Internal error.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        whitenoise.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![],
                key_package: vec![],
            },
        );

        let err = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap_err();
        assert!(
            matches!(err, LoginError::Internal(_)),
            "Must error when signer is missing from the registry"
        );

        whitenoise.pending_logins.remove(&pubkey);
    }

    // The following five tests mirror the local-key partial-combination matrix
    // (test_publish_default_relays_*) for the external signer path.  The code
    // paths diverge in how relay-list events are published (publish_relay_list
    // vs publish_relay_list_with_signer), so a bug in the signer path for a
    // specific combination could slip through without these.

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_inbox_only_present() {
        // User has 10050 but not 10002 or 10051.
        // Inbox must be preserved; nip65 and key_package must get defaults.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let inbox_url = RelayUrl::parse("wss://custom-ext-inbox.example.com").unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&inbox_url)
            .await
            .unwrap();

        setup_pending_login_external_signer_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![inbox_relay],
                key_package: vec![],
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(result.account.account_type, AccountType::External);

        let stored_inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert_eq!(
            stored_inbox[0].url, inbox_url,
            "Inbox must not be overwritten (external signer)"
        );

        let nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert!(!nip65.is_empty(), "NIP-65 must receive defaults");

        let kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert!(!kp.is_empty(), "KeyPackage must receive defaults");
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_key_package_only_present() {
        // User has 10051 but not 10002 or 10050.
        // KeyPackage must be preserved; nip65 and inbox must get defaults.
        //
        // The key package relay must be a reachable URL because complete_external_signer_login
        // publishes the MLS key package to the kp relay.  We use the local Docker relay
        // (ws://localhost:7777) so publishing succeeds; the point of this test is to verify
        // the "already present → not overwritten" logic, not to test relay connectivity.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let kp_url = RelayUrl::parse("ws://localhost:7777").unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&kp_url)
            .await
            .unwrap();

        setup_pending_login_external_signer_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![],
                key_package: vec![kp_relay],
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);

        let stored_kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert_eq!(
            stored_kp[0].url, kp_url,
            "KeyPackage must not be overwritten (external signer)"
        );

        let nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert!(!nip65.is_empty(), "NIP-65 must receive defaults");

        let inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert!(!inbox.is_empty(), "Inbox must receive defaults");
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_nip65_and_inbox_present_kp_missing() {
        // User has 10002 and 10050 but not 10051.
        // NIP-65 and inbox must be preserved; key_package must get defaults.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_url = RelayUrl::parse("wss://custom-ext-nip65b.example.com").unwrap();
        let inbox_url = RelayUrl::parse("wss://custom-ext-inboxb.example.com").unwrap();
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&nip65_url)
            .await
            .unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&inbox_url)
            .await
            .unwrap();

        setup_pending_login_external_signer_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![nip65_relay],
                inbox: vec![inbox_relay],
                key_package: vec![],
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);

        let stored_nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(stored_nip65[0].url, nip65_url, "NIP-65 must be preserved");

        let stored_inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert_eq!(stored_inbox[0].url, inbox_url, "Inbox must be preserved");

        let kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert!(!kp.is_empty(), "KeyPackage must receive defaults");
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_nip65_and_kp_present_inbox_missing() {
        // User has 10002 and 10051 but not 10050.
        // This is the original bug scenario for external signer accounts.
        // NIP-65 and key_package must be preserved; inbox must get defaults.
        //
        // The key package relay must be reachable (local Docker) so the key package publish
        // step in complete_external_signer_login succeeds.  The nip65 relay can be an
        // unreachable URL because relay-list publish for an already-present list is skipped.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_url = RelayUrl::parse("wss://custom-ext-nip65c.example.com").unwrap();
        // Use the local Docker relay for the key package relay so publishing succeeds.
        let kp_url = RelayUrl::parse("ws://localhost:7777").unwrap();
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&nip65_url)
            .await
            .unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&kp_url)
            .await
            .unwrap();

        setup_pending_login_external_signer_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![nip65_relay],
                inbox: vec![],
                key_package: vec![kp_relay],
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);

        let stored_nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(stored_nip65[0].url, nip65_url, "NIP-65 must be preserved");

        let stored_kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert_eq!(stored_kp[0].url, kp_url, "KeyPackage must be preserved");

        let inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert!(!inbox.is_empty(), "Inbox must receive defaults");
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_inbox_and_kp_present_nip65_missing() {
        // User has 10050 and 10051 but not 10002.
        // Inbox and key_package must be preserved; nip65 must get defaults.
        //
        // The key package relay must be reachable (local Docker) so the key package publish
        // step in complete_external_signer_login succeeds.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let inbox_url = RelayUrl::parse("wss://custom-ext-inboxd.example.com").unwrap();
        // Use the local Docker relay for the key package relay so publishing succeeds.
        let kp_url = RelayUrl::parse("ws://localhost:7777").unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&inbox_url)
            .await
            .unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&kp_url)
            .await
            .unwrap();

        setup_pending_login_external_signer_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![inbox_relay],
                key_package: vec![kp_relay],
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);

        let nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert!(!nip65.is_empty(), "NIP-65 must receive defaults");

        let stored_inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert_eq!(stored_inbox[0].url, inbox_url, "Inbox must be preserved");

        let stored_kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert_eq!(stored_kp[0].url, kp_url, "KeyPackage must be preserved");
    }

    // -----------------------------------------------------------------------
    // Regression test — the original bug
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_regression_nip65_only_user_is_blocked_from_login() {
        // This is the exact scenario that was broken:
        // A user has published a 10002 relay list but has never published
        // 10050 (InboxRelays) or 10051 (MlsKeyPackageRelays).
        //
        // The old code fell back to defaults silently and returned Complete.
        // The new code must return NeedsRelayLists so the UI can prompt the user.
        //
        // We simulate this by pre-populating pending_logins with a partial
        // DiscoveredRelayLists that only has nip65 populated, then verifying
        // the stash is NOT complete.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://relay.damus.io").unwrap())
            .await
            .unwrap();

        // This is what try_discover_relay_lists now returns for the bug user.
        let partial = DiscoveredRelayLists {
            nip65: vec![nip65_relay],
            inbox: vec![],       // 10050 absent
            key_package: vec![], // 10051 absent
        };

        assert!(
            !partial.is_complete(),
            "A user with only 10002 must NOT pass is_complete() — this would have been the bug"
        );

        // Simulate what login_start does with this result: stash and return NeedsRelayLists.
        whitenoise
            .create_base_account_with_private_key(&keys)
            .await
            .unwrap();
        whitenoise.pending_logins.insert(pubkey, partial);

        assert!(
            whitenoise.pending_logins.contains_key(&pubkey),
            "User with only 10002 must be held in pending state"
        );

        // Now drive publish_default_relays. It should fill only the missing ones.
        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

        // Inbox and key_package must now have defaults.
        let inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert!(
            !inbox.is_empty(),
            "Inbox must be filled after publish_default_relays"
        );
        let kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert!(
            !kp.is_empty(),
            "KeyPackage must be filled after publish_default_relays"
        );
    }
}
