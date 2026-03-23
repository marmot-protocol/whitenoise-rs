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
    #[perf_instrument("accounts")]
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
        // publish_to_urls is the target for relay-list events: use discovered
        // NIP-65 relays when available, otherwise defaults.
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
    #[perf_instrument("accounts")]
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
            self.sync_discovered_relay_lists(&account, &merged).await?;
            self.complete_external_signer_login(&account, merged.relays(RelayType::Inbox), signer)
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
    pub(crate) async fn complete_external_signer_login<S>(
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

        let user = account
            .user(&self.database)
            .await
            .map_err(LoginError::from)?;
        self.activate_account_without_publishing(account, &user, inbox_relays)
            .await
            .map_err(LoginError::from)?;

        self.publish_key_package_for_account_with_signer(account, signer)
            .await
            .map_err(LoginError::from)?;

        Ok(())
    }

    /// Create/update an external signer account record without setting up relays.
    /// Used by the multi-step external signer login flow.
    pub(crate) async fn setup_external_signer_account_without_relays(
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
    pub(crate) async fn setup_external_signer_account(
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

        let user = account.user(&self.database).await?;
        self.activate_account_without_publishing(&account, &user, &relay_setup.inbox_relays)
            .await?;

        Ok(account)
    }
}
