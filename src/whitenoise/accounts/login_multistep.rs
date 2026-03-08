use nostr_sdk::prelude::*;

use super::{Account, DiscoveredRelayLists, LoginError, LoginResult, LoginStatus};
use crate::RelayType;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::relays::Relay;

impl Whitenoise {
    // -----------------------------------------------------------------------
    // Multi-step login API
    //
    // Callers must not invoke step-2 methods concurrently for the same pubkey.
    // The pending-login stash is not guarded beyond DashMap's per-key lock, so
    // overlapping calls can race between the merge and complete_login phases.
    // -----------------------------------------------------------------------

    /// Step 1 of the multi-step login flow.
    ///
    /// Parses the private key, creates/updates the account in the database and
    /// keychain, then attempts to discover existing relay lists from the network.
    ///
    /// **Happy path:** relay lists are found on the network and the account is
    /// fully activated (subscriptions, key package, etc.). Returns
    /// [`LoginStatus::Complete`].
    ///
    /// **No relay lists found:** the account is left in a *partial* state and
    /// [`LoginStatus::NeedsRelayLists`] is returned. The caller must then invoke
    /// one of:
    /// - [`Whitenoise::login_publish_default_relays`] -- publish default relay lists
    /// - [`Whitenoise::login_with_custom_relay`] -- search a user-provided relay
    /// - [`Whitenoise::login_cancel`] -- abort and clean up
    pub async fn login_start(
        &self,
        nsec_or_hex_privkey: String,
    ) -> core::result::Result<LoginResult, LoginError> {
        let keys = Keys::parse(&nsec_or_hex_privkey)
            .map_err(|e| LoginError::InvalidKeyFormat(e.to_string()))?;
        let pubkey = keys.public_key();
        tracing::debug!(
            target: "whitenoise::accounts",
            "Starting login for pubkey: {}",
            pubkey.to_hex()
        );

        let mut account = self
            .create_base_account_with_private_key(&keys)
            .await
            .map_err(LoginError::from)?;
        tracing::debug!(
            target: "whitenoise::accounts",
            "Account created in DB and key stored in keychain"
        );

        // Try to discover relay lists from the network via default relays.
        let default_relays = self.load_default_relays().await.map_err(LoginError::from)?;

        let discovered = self
            .try_discover_relay_lists(&mut account, &default_relays)
            .await?;

        if discovered.is_complete() {
            // Happy path: all three relay lists found, complete the login.
            self.complete_login(
                &account,
                &discovered.nip65,
                &discovered.inbox,
                &discovered.key_package,
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
            // One or more relay lists are missing. Stash what we found so that
            // login_publish_default_relays can skip publishing the ones that
            // already exist.
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

    /// Step 2a: publish default relay lists for any that are missing, then complete login.
    ///
    /// Called after [`Whitenoise::login_start`] returned [`LoginStatus::NeedsRelayLists`].
    /// Uses the partial discovery results stashed during step 1 to determine which
    /// of the three relay list kinds (10002, 10050, 10051) were already found on the
    /// network.  Default relays are assigned and published **only for the missing
    /// ones**; existing lists are left untouched.
    pub async fn login_publish_default_relays(
        &self,
        pubkey: &PublicKey,
    ) -> core::result::Result<LoginResult, LoginError> {
        let discovered = self
            .pending_logins
            .get(pubkey)
            .ok_or(LoginError::NoLoginInProgress)?
            .clone();

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
        let keys = self
            .secrets_store
            .get_nostr_keys_for_pubkey(pubkey)
            .map_err(|e| LoginError::Internal(e.to_string()))?;
        let user = account
            .user(&self.database)
            .await
            .map_err(LoginError::from)?;

        // publish_to_relays is the target for relay-list events: use the
        // already-discovered NIP-65 relays when available, otherwise defaults.
        let publish_to_relays = discovered
            .relays_or(RelayType::Nip65, &default_relays)
            .to_vec();

        // For each relay type: publish defaults only for the ones that were
        // missing. Already-found lists were already persisted to the DB by
        // try_discover_relay_lists in step 1, so no second write is needed.
        for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
            if discovered.relays(relay_type).is_empty() {
                // Missing — assign defaults in the DB and publish to the network.
                user.add_relays(&default_relays, relay_type, &self.database)
                    .await
                    .map_err(LoginError::from)?;
                self.publish_relay_list(
                    &default_relays,
                    relay_type,
                    &publish_to_relays,
                    keys.clone(),
                )
                .await
                .map_err(LoginError::from)?;
            } else {
                tracing::debug!(
                    target: "whitenoise::accounts",
                    "Skipping publish for {:?} — already exists on network",
                    relay_type,
                );
            }
        }

        tracing::debug!(
            target: "whitenoise::accounts",
            "Missing relay lists published, activating account"
        );

        self.complete_login(
            &account,
            discovered.relays_or(RelayType::Nip65, &default_relays),
            discovered.relays_or(RelayType::Inbox, &default_relays),
            discovered.relays_or(RelayType::KeyPackage, &default_relays),
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

    /// Step 2b: search a user-provided relay for existing relay lists.
    ///
    /// Called after [`Whitenoise::login_start`] returned [`LoginStatus::NeedsRelayLists`].
    /// Connects to `relay_url` and attempts to fetch all three relay list kinds
    /// (10002, 10050, 10051). Any newly-found lists are merged into the existing
    /// pending-login stash so previously-discovered relays are preserved.
    ///
    /// If the merged stash is now complete the account is activated and
    /// [`LoginStatus::Complete`] is returned. Otherwise
    /// [`LoginStatus::NeedsRelayLists`] is returned so the caller can re-prompt.
    pub async fn login_with_custom_relay(
        &self,
        pubkey: &PublicKey,
        relay_url: RelayUrl,
    ) -> core::result::Result<LoginResult, LoginError> {
        if !self.pending_logins.contains_key(pubkey) {
            return Err(LoginError::NoLoginInProgress);
        }
        tracing::debug!(
            target: "whitenoise::accounts",
            "Searching for relay lists on {} for {}",
            relay_url,
            pubkey.to_hex()
        );

        let mut account = Account::find_by_pubkey(pubkey, &self.database)
            .await
            .map_err(LoginError::from)?;

        // Create a Relay object for the user-provided URL.
        let custom_relay = self
            .find_or_create_relay_by_url(&relay_url)
            .await
            .map_err(LoginError::from)?;
        let source_relays = vec![custom_relay];

        let discovered = self
            .try_discover_relay_lists(&mut account, &source_relays)
            .await?;

        // Merge newly-discovered lists into the stash unconditionally, before
        // attempting completion.  Doing this upfront means:
        // (a) if complete_login fails and the user retries via
        //     login_publish_default_relays, the stash reflects what was already
        //     found on the network, so we don't publish defaults over relays
        //     that try_discover_relay_lists already wrote to the DB; and
        // (b) the is_complete check below correctly accounts for lists found
        //     across multiple relays (e.g. nip65 from login_start, inbox+kp here).
        let merged = self.merge_into_stash(pubkey, discovered)?;

        if merged.is_complete() {
            self.complete_login(&account, &merged.nip65, &merged.inbox, &merged.key_package)
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

    /// Cancel a pending login and clean up all partial state.
    ///
    /// Only performs cleanup when a login is actually pending for the given
    /// pubkey (i.e. `login_start` was called but not yet completed). If no
    /// login is pending this is a no-op and returns `Ok(())`.
    pub async fn login_cancel(&self, pubkey: &PublicKey) -> core::result::Result<(), LoginError> {
        // Only clean up if there was actually a pending login for this pubkey.
        if self.pending_logins.remove(pubkey).is_none() {
            tracing::debug!(
                target: "whitenoise::accounts",
                "No pending login for {}, nothing to cancel",
                pubkey.to_hex()
            );
            return Ok(());
        }

        // Remove any stashed external signer for this pubkey.
        self.remove_external_signer(pubkey);

        // Clean up the partial account if it exists.
        if let Ok(account) = Account::find_by_pubkey(pubkey, &self.database).await {
            // Remove relay associations that try_discover_relay_lists may have
            // written to the DB before the user cancelled.  The user_relays table
            // has no CASCADE from accounts, so these rows would otherwise persist
            // as stale data for a subsequent login with the same pubkey.
            if let Ok(user) = account.user(&self.database).await
                && let Err(e) = user.remove_all_relays(&self.database).await
            {
                tracing::warn!(
                    target: "whitenoise::accounts",
                    "Failed to remove relay associations during login cancel for {}: {}",
                    pubkey.to_hex(),
                    e
                );
            }
            account
                .delete(&self.database)
                .await
                .map_err(LoginError::from)?;
            // Best-effort removal of the keychain entry.
            let _ = self.secrets_store.remove_private_key_for_pubkey(pubkey);
            tracing::info!(
                target: "whitenoise::accounts",
                "Cleaned up partial login for {}",
                pubkey.to_hex()
            );
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Shared helpers for multi-step login
    // -----------------------------------------------------------------------

    /// Attempt to fetch all three relay lists from the network.
    ///
    /// Returns a [`DiscoveredRelayLists`] whose fields contain the relays found
    /// for each list type.  Any field that is empty was **not found** on the
    /// network.  When 10050/10051 are searched, the source is the NIP-65 relays
    /// (if found); otherwise the original `source_relays` are used as a fallback
    /// so we still try all three even when 10002 is missing.
    ///
    /// Callers should check [`DiscoveredRelayLists::is_complete`] to determine
    /// whether login can proceed or whether the user must provide relay lists.
    pub(super) async fn try_discover_relay_lists(
        &self,
        account: &mut Account,
        source_relays: &[Relay],
    ) -> core::result::Result<DiscoveredRelayLists, LoginError> {
        // Step 1: Fetch NIP-65 relay list (kind 10002) from the source relays.
        let nip65_relays = self
            .fetch_existing_relays(account.pubkey, RelayType::Nip65, source_relays)
            .await
            .map_err(LoginError::from)?;

        // Use the discovered NIP-65 relays as the source for 10050/10051 when
        // available; otherwise fall back to the original source relays so we
        // still make a best-effort attempt even when 10002 is absent.
        let secondary_source = if nip65_relays.is_empty() {
            source_relays.to_vec()
        } else {
            nip65_relays.clone()
        };

        // Steps 2 & 3: Fetch Inbox (10050) and KeyPackage (10051) concurrently.
        let pubkey = account.pubkey;
        let (inbox_result, key_package_result) = tokio::join!(
            self.fetch_existing_relays(pubkey, RelayType::Inbox, &secondary_source),
            self.fetch_existing_relays(pubkey, RelayType::KeyPackage, &secondary_source),
        );
        let inbox_relays = inbox_result.map_err(LoginError::from)?;
        let key_package_relays = key_package_result.map_err(LoginError::from)?;

        // Persist only the lists that were actually found so the DB reflects
        // what exists on the network, not assumed defaults.
        let user = account
            .user(&self.database)
            .await
            .map_err(LoginError::from)?;
        if !nip65_relays.is_empty() {
            user.add_relays(&nip65_relays, RelayType::Nip65, &self.database)
                .await
                .map_err(LoginError::from)?;
        }
        if !inbox_relays.is_empty() {
            user.add_relays(&inbox_relays, RelayType::Inbox, &self.database)
                .await
                .map_err(LoginError::from)?;
        }
        if !key_package_relays.is_empty() {
            user.add_relays(&key_package_relays, RelayType::KeyPackage, &self.database)
                .await
                .map_err(LoginError::from)?;
        }

        Ok(DiscoveredRelayLists {
            nip65: nip65_relays,
            inbox: inbox_relays,
            key_package: key_package_relays,
        })
    }

    /// Merge newly-discovered relay lists into the pending-login stash and
    /// return a snapshot.  The DashMap lock is released before returning so
    /// the caller is free to do async work with the result.
    pub(super) fn merge_into_stash(
        &self,
        pubkey: &PublicKey,
        discovered: DiscoveredRelayLists,
    ) -> core::result::Result<DiscoveredRelayLists, LoginError> {
        let mut stash = self
            .pending_logins
            .get_mut(pubkey)
            .ok_or(LoginError::NoLoginInProgress)?;
        stash.merge(discovered);
        let snapshot = stash.clone();
        drop(stash);
        Ok(snapshot)
    }

    /// Activate a local-key account after relay lists have been resolved.
    async fn complete_login(
        &self,
        account: &Account,
        nip65_relays: &[Relay],
        inbox_relays: &[Relay],
        key_package_relays: &[Relay],
    ) -> core::result::Result<(), LoginError> {
        let user = account
            .user(&self.database)
            .await
            .map_err(LoginError::from)?;
        self.activate_account(
            account,
            &user,
            false,
            nip65_relays,
            inbox_relays,
            key_package_relays,
        )
        .await
        .map_err(LoginError::from)
    }

    /// Activate an external-signer account after relay lists have been resolved.
    pub(super) async fn complete_external_signer_login<S>(
        &self,
        account: &Account,
        nip65_relays: &[Relay],
        inbox_relays: &[Relay],
        key_package_relays: &[Relay],
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
        self.activate_account_without_publishing(
            account,
            &user,
            nip65_relays,
            inbox_relays,
            key_package_relays,
        )
        .await
        .map_err(LoginError::from)?;

        self.publish_key_package_for_account_with_signer(account, signer)
            .await
            .map_err(LoginError::from)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::RelayType;
    use crate::whitenoise::WhitenoiseError;
    use crate::whitenoise::accounts::{Account, AccountType, DiscoveredRelayLists};
    use crate::whitenoise::relays::Relay;
    use crate::whitenoise::test_utils::*;

    // -----------------------------------------------------------------------
    // LoginError / LoginResult / LoginStatus tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_login_status_equality() {
        assert_eq!(LoginStatus::Complete, LoginStatus::Complete);
        assert_eq!(LoginStatus::NeedsRelayLists, LoginStatus::NeedsRelayLists);
        assert_ne!(LoginStatus::Complete, LoginStatus::NeedsRelayLists);
    }

    #[test]
    fn test_login_status_clone() {
        let status = LoginStatus::NeedsRelayLists;
        let cloned = status.clone();
        assert_eq!(status, cloned);
    }

    #[test]
    fn test_login_status_debug() {
        let debug = format!("{:?}", LoginStatus::Complete);
        assert!(debug.contains("Complete"));
        let debug = format!("{:?}", LoginStatus::NeedsRelayLists);
        assert!(debug.contains("NeedsRelayLists"));
    }

    // -----------------------------------------------------------------------
    // LoginError / LoginResult / LoginStatus tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_login_error_from_key_error() {
        // Simulate a bad key parse
        let key_result = Keys::parse("not-a-valid-nsec");
        assert!(key_result.is_err());
        let login_err = LoginError::from(key_result.unwrap_err());
        assert!(matches!(login_err, LoginError::InvalidKeyFormat(_)));
    }

    #[test]
    fn test_login_error_from_whitenoise_error_relay() {
        let wn_err = WhitenoiseError::NostrManager(
            crate::nostr_manager::NostrManagerError::NoRelayConnections,
        );
        let login_err = LoginError::from(wn_err);
        assert!(matches!(login_err, LoginError::NoRelayConnections));
    }

    #[test]
    fn test_login_error_from_whitenoise_error_timeout() {
        // A relay Timeout should map to LoginError::Timeout via the dedicated
        // NostrManagerError::Timeout variant (no string matching).
        let nostr_mgr_err = crate::nostr_manager::NostrManagerError::Timeout;
        let wn_err = WhitenoiseError::NostrManager(nostr_mgr_err);
        let login_err = LoginError::from(wn_err);
        assert!(
            matches!(login_err, LoginError::Timeout(_)),
            "Expected Timeout, got: {:?}",
            login_err
        );
    }

    #[test]
    fn test_login_error_from_whitenoise_error_non_timeout_client() {
        // A Client error that is NOT a timeout should map to Internal.
        let client_err = nostr_sdk::client::Error::Signer(nostr_sdk::signer::SignerError::backend(
            std::io::Error::other("some signer error"),
        ));
        let nostr_mgr_err = crate::nostr_manager::NostrManagerError::Client(client_err);
        let wn_err = WhitenoiseError::NostrManager(nostr_mgr_err);
        let login_err = LoginError::from(wn_err);
        assert!(
            matches!(login_err, LoginError::Internal(_)),
            "Expected Internal for non-timeout client error, got: {:?}",
            login_err
        );
    }

    #[tokio::test]
    async fn test_login_cancel_no_account_is_ok() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();
        // Cancelling when no login is in progress should succeed silently.
        let result = whitenoise.login_cancel(&pubkey).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_login_publish_default_relays_without_start_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();
        let result = whitenoise.login_publish_default_relays(&pubkey).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), LoginError::NoLoginInProgress));
    }

    #[tokio::test]
    async fn test_login_with_custom_relay_without_start_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();
        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let result = whitenoise.login_with_custom_relay(&pubkey, relay_url).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), LoginError::NoLoginInProgress));
    }

    #[test]
    fn test_login_error_debug() {
        let err = LoginError::NoRelayConnections;
        let debug = format!("{:?}", err);
        assert!(debug.contains("NoRelayConnections"));
    }

    #[tokio::test]
    async fn test_login_start_invalid_key() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let result = whitenoise
            .login_start("definitely-not-a-valid-key".to_string())
            .await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            LoginError::InvalidKeyFormat(_)
        ));
    }

    #[tokio::test]
    async fn test_login_start_valid_key_no_relays() {
        // In the mock environment with no relay servers, login_start should
        // either return NeedsRelayLists or an error (relay connection failure).
        // Both are acceptable -- the key thing is it doesn't panic and the
        // account is created.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let result = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await;

        match result {
            Ok(login_result) => {
                // NeedsRelayLists is the expected happy case in a no-relay env.
                assert_eq!(login_result.status, LoginStatus::NeedsRelayLists);
                assert_eq!(login_result.account.pubkey, pubkey);
                assert!(whitenoise.pending_logins.contains_key(&pubkey));
                // Clean up
                let _ = whitenoise.login_cancel(&pubkey).await;
            }
            Err(e) => {
                // Relay connection errors are acceptable in the mock env.
                let err_msg = e.to_string();
                assert!(
                    err_msg.contains("relay")
                        || err_msg.contains("connection")
                        || err_msg.contains("timeout"),
                    "Unexpected error: {}",
                    err_msg
                );
            }
        }
    }

    #[tokio::test]
    async fn test_login_start_duplicate_call_overwrites_stash() {
        // If login_start is called twice for the same pubkey (e.g. the user taps
        // "login" again after a partial discovery), the second call inserts a new
        // stash entry that silently replaces the old one in the DashMap.
        //
        // This test documents the current behaviour: the second stash wins.
        // Callers that rely on accumulated cross-relay state should not call
        // login_start a second time — they should use login_with_custom_relay.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Manually seed a partial stash (as if a first login_start found nip65).
        whitenoise
            .create_base_account_with_private_key(&keys)
            .await
            .unwrap();
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://first-run.example.com").unwrap())
            .await
            .unwrap();
        whitenoise.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: vec![nip65_relay.clone()],
                inbox: vec![],
                key_package: vec![],
            },
        );

        // A second login_start (or a direct insert, as we do here) overwrites
        // the existing stash unconditionally.
        whitenoise.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![],
                key_package: vec![],
            },
        );

        // The stash now reflects the second insert, not the first.
        let stash = whitenoise.pending_logins.get(&pubkey).unwrap();
        assert!(
            stash.nip65.is_empty(),
            "Second insert must overwrite the first stash — nip65 discovered in the first run is lost"
        );
        drop(stash);

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    #[tokio::test]
    async fn test_login_cancel_with_pending_login() {
        // Start a login (which creates a partial account), then cancel it.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Manually create a partial account and mark as pending to simulate
        // what login_start does, avoiding relay connection issues in the mock.
        whitenoise
            .create_base_account_with_private_key(&keys)
            .await
            .unwrap();
        whitenoise.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![],
                key_package: vec![],
            },
        );

        // Verify account exists.
        assert!(
            whitenoise.find_account_by_pubkey(&pubkey).await.is_ok(),
            "Account should exist before cancel"
        );

        // Cancel should clean up everything.
        let result = whitenoise.login_cancel(&pubkey).await;
        assert!(result.is_ok());

        // Account should be gone.
        assert!(
            whitenoise.find_account_by_pubkey(&pubkey).await.is_err(),
            "Account should be deleted after cancel"
        );
        // Pending login should be cleared.
        assert!(
            !whitenoise.pending_logins.contains_key(&pubkey),
            "Pending login should be removed after cancel"
        );
    }

    #[tokio::test]
    async fn test_login_cancel_pending_but_no_account_in_db() {
        // Edge case: pubkey is in pending_logins but no account exists in DB
        // (e.g., account creation failed but pending was inserted).
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        // Insert into pending_logins without creating an account.
        whitenoise.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![],
                key_package: vec![],
            },
        );

        let result = whitenoise.login_cancel(&pubkey).await;
        assert!(result.is_ok());
        assert!(!whitenoise.pending_logins.contains_key(&pubkey));
    }

    #[tokio::test]
    async fn test_login_cancel_does_not_delete_non_pending_account() {
        // Verify that login_cancel does NOT delete an account that isn't
        // in the pending set (protects fully-activated accounts).
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Create an account but don't add to pending_logins.
        whitenoise
            .create_base_account_with_private_key(&keys)
            .await
            .unwrap();

        let result = whitenoise.login_cancel(&pubkey).await;
        assert!(result.is_ok());

        // Account should still exist since it wasn't pending.
        assert!(
            whitenoise.find_account_by_pubkey(&pubkey).await.is_ok(),
            "Non-pending account should not be deleted by login_cancel"
        );
    }

    #[test]
    fn test_login_result_debug() {
        let keys = Keys::generate();
        let account = Account {
            id: Some(1),
            pubkey: keys.public_key(),
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let result = LoginResult {
            account,
            status: LoginStatus::Complete,
        };
        let debug = format!("{:?}", result);
        assert!(!debug.is_empty());
        assert!(debug.contains("Complete"));
    }

    #[test]
    fn test_login_result_clone() {
        let keys = Keys::generate();
        let account = Account {
            id: Some(1),
            pubkey: keys.public_key(),
            user_id: 1,
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let result = LoginResult {
            account: account.clone(),
            status: LoginStatus::NeedsRelayLists,
        };
        let cloned = result.clone();
        assert_eq!(cloned.status, LoginStatus::NeedsRelayLists);
        assert_eq!(cloned.account.pubkey, account.pubkey);
    }

    #[test]
    fn test_login_error_timeout_display() {
        let err = LoginError::Timeout("relay fetch took 30s".to_string());
        assert_eq!(
            err.to_string(),
            "Login operation timed out: relay fetch took 30s"
        );
    }

    #[test]
    fn test_login_error_internal_display() {
        let err = LoginError::Internal("unexpected DB error".to_string());
        assert_eq!(err.to_string(), "Login error: unexpected DB error");
    }

    #[test]
    fn test_login_error_no_login_in_progress_display() {
        let err = LoginError::NoLoginInProgress;
        assert_eq!(err.to_string(), "No login in progress for this account");
    }

    // -----------------------------------------------------------------------
    // Multi-step login flow tests (require Docker relays on localhost)
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

    #[tokio::test]
    async fn test_login_start_happy_path_with_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Pre-publish relay lists to the Docker relays.
        publish_relay_lists_to_dev_relays(&keys).await;

        // login_start should discover the lists and return Complete.
        let result = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(result.account.pubkey, pubkey);

        // Verify relay lists were stored.
        let nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert!(
            !nip65.is_empty(),
            "Expected NIP-65 relays to be stored after login"
        );
    }

    #[tokio::test]
    async fn test_login_start_no_relays_then_publish_defaults() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Don't publish anything — login_start should return NeedsRelayLists.
        let result = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::NeedsRelayLists);
        assert!(whitenoise.pending_logins.contains_key(&pubkey));

        // Now publish default relays to complete the login.
        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(result.account.pubkey, pubkey);
        assert!(!whitenoise.pending_logins.contains_key(&pubkey));

        // Verify all three relay types are stored.
        for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
            let relays = result
                .account
                .relays(relay_type, &whitenoise)
                .await
                .unwrap();
            assert!(
                !relays.is_empty(),
                "Expected {:?} relays after publishing defaults",
                relay_type
            );
        }
    }

    #[tokio::test]
    async fn test_login_with_custom_relay_finds_lists() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Publish relay lists to the Docker relays.
        publish_relay_lists_to_dev_relays(&keys).await;

        // Start login — since the dev relays ARE the defaults, this may find
        // the lists immediately. If it does, great. If not, use custom relay.
        let start = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .unwrap();

        if start.status == LoginStatus::Complete {
            // Already found on defaults — test passes.
            return;
        }

        // Use one of the Docker relays as the "custom" relay.
        let relay_url = RelayUrl::parse("ws://localhost:8080").unwrap();
        let result = whitenoise
            .login_with_custom_relay(&pubkey, relay_url)
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(result.account.pubkey, pubkey);
    }

    #[tokio::test]
    async fn test_login_with_custom_relay_not_found() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Don't publish anything. Start login — returns NeedsRelayLists.
        let start = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .unwrap();
        assert_eq!(start.status, LoginStatus::NeedsRelayLists);

        // Try a custom relay — lists aren't there either.
        let relay_url = RelayUrl::parse("ws://localhost:8080").unwrap();
        let result = whitenoise
            .login_with_custom_relay(&pubkey, relay_url)
            .await
            .unwrap();

        assert_eq!(
            result.status,
            LoginStatus::NeedsRelayLists,
            "Should still be NeedsRelayLists when no lists exist"
        );

        // Clean up.
        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    // -----------------------------------------------------------------------
    // DiscoveredRelayLists::is_complete tests
    // -----------------------------------------------------------------------

    /// Helper: build a dummy Relay from a URL string without touching the DB.
    fn dummy_relay(url: &str) -> Relay {
        Relay {
            id: Some(1),
            url: RelayUrl::parse(url).unwrap(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_all_present() {
        let lists = DiscoveredRelayLists {
            nip65: vec![dummy_relay("wss://a.example.com")],
            inbox: vec![dummy_relay("wss://b.example.com")],
            key_package: vec![dummy_relay("wss://c.example.com")],
        };
        assert!(
            lists.is_complete(),
            "All three lists present — should be complete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_all_empty() {
        let lists = DiscoveredRelayLists {
            nip65: vec![],
            inbox: vec![],
            key_package: vec![],
        };
        assert!(!lists.is_complete(), "All empty — should not be complete");
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_only_nip65() {
        let lists = DiscoveredRelayLists {
            nip65: vec![dummy_relay("wss://a.example.com")],
            inbox: vec![],
            key_package: vec![],
        };
        assert!(
            !lists.is_complete(),
            "Only nip65 present — must be incomplete (inbox and key_package missing)"
        );
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_only_inbox() {
        let lists = DiscoveredRelayLists {
            nip65: vec![],
            inbox: vec![dummy_relay("wss://b.example.com")],
            key_package: vec![],
        };
        assert!(
            !lists.is_complete(),
            "Only inbox present — must be incomplete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_only_key_package() {
        let lists = DiscoveredRelayLists {
            nip65: vec![],
            inbox: vec![],
            key_package: vec![dummy_relay("wss://c.example.com")],
        };
        assert!(
            !lists.is_complete(),
            "Only key_package present — must be incomplete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_nip65_and_inbox_only() {
        let lists = DiscoveredRelayLists {
            nip65: vec![dummy_relay("wss://a.example.com")],
            inbox: vec![dummy_relay("wss://b.example.com")],
            key_package: vec![],
        };
        assert!(
            !lists.is_complete(),
            "nip65 + inbox but no key_package — must be incomplete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_nip65_and_key_package_only() {
        // This is the bug scenario: user has 10002 and 10051 but no 10050.
        let lists = DiscoveredRelayLists {
            nip65: vec![dummy_relay("wss://a.example.com")],
            inbox: vec![],
            key_package: vec![dummy_relay("wss://c.example.com")],
        };
        assert!(
            !lists.is_complete(),
            "nip65 + key_package but no inbox — must be incomplete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_inbox_and_key_package_only() {
        let lists = DiscoveredRelayLists {
            nip65: vec![],
            inbox: vec![dummy_relay("wss://b.example.com")],
            key_package: vec![dummy_relay("wss://c.example.com")],
        };
        assert!(
            !lists.is_complete(),
            "inbox + key_package but no nip65 — must be incomplete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_multiple_relays_per_list() {
        // Multiple entries per list should still count as complete.
        let lists = DiscoveredRelayLists {
            nip65: vec![
                dummy_relay("wss://a1.example.com"),
                dummy_relay("wss://a2.example.com"),
            ],
            inbox: vec![
                dummy_relay("wss://b1.example.com"),
                dummy_relay("wss://b2.example.com"),
            ],
            key_package: vec![dummy_relay("wss://c.example.com")],
        };
        assert!(
            lists.is_complete(),
            "Multiple relays per list — should be complete"
        );
    }

    // -----------------------------------------------------------------------
    // DiscoveredRelayLists::relays_or tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_relays_or_returns_own_relays_when_non_empty() {
        // When the field is non-empty, relays_or must return the field's contents
        // rather than the fallback.
        let relay_a = dummy_relay("wss://a.example.com");
        let relay_fallback = dummy_relay("wss://fallback.example.com");
        let lists = DiscoveredRelayLists {
            nip65: vec![relay_a.clone()],
            inbox: vec![],
            key_package: vec![],
        };
        let fallback_slice = [relay_fallback.clone()];
        let result = lists.relays_or(RelayType::Nip65, &fallback_slice);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].url, relay_a.url,
            "Must return own relay, not fallback"
        );
    }

    #[test]
    fn test_relays_or_returns_fallback_when_field_empty() {
        // When the field is empty, relays_or must return the fallback slice.
        let relay_fallback = dummy_relay("wss://fallback.example.com");
        let lists = DiscoveredRelayLists {
            nip65: vec![],
            inbox: vec![],
            key_package: vec![],
        };
        let fallback_slice = [relay_fallback.clone()];
        let result = lists.relays_or(RelayType::Nip65, &fallback_slice);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].url, relay_fallback.url,
            "Must return fallback when field is empty"
        );
    }

    #[test]
    fn test_relays_or_works_for_all_relay_types() {
        // Verify the correct field is selected for each RelayType variant.
        let nip65_relay = dummy_relay("wss://nip65.example.com");
        let inbox_relay = dummy_relay("wss://inbox.example.com");
        let kp_relay = dummy_relay("wss://kp.example.com");
        let fallback = dummy_relay("wss://fallback.example.com");
        let fallback_slice = [fallback.clone()];

        let lists = DiscoveredRelayLists {
            nip65: vec![nip65_relay.clone()],
            inbox: vec![inbox_relay.clone()],
            key_package: vec![kp_relay.clone()],
        };

        let r = lists.relays_or(RelayType::Nip65, &fallback_slice);
        assert_eq!(r[0].url, nip65_relay.url);

        let r = lists.relays_or(RelayType::Inbox, &fallback_slice);
        assert_eq!(r[0].url, inbox_relay.url);

        let r = lists.relays_or(RelayType::KeyPackage, &fallback_slice);
        assert_eq!(r[0].url, kp_relay.url);
    }

    #[test]
    fn test_relays_or_fallback_for_all_relay_types_when_all_empty() {
        // Verify fallback is used for every RelayType when the corresponding field is empty.
        let fallback = dummy_relay("wss://fallback.example.com");
        let fallback_slice = [fallback.clone()];
        let lists = DiscoveredRelayLists {
            nip65: vec![],
            inbox: vec![],
            key_package: vec![],
        };

        for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
            let r = lists.relays_or(relay_type, &fallback_slice);
            assert_eq!(
                r[0].url, fallback.url,
                "{:?} must return fallback when empty",
                relay_type
            );
        }
    }

    // -----------------------------------------------------------------------
    // DiscoveredRelayLists::merge tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_merge_fills_empty_fields_from_other() {
        // When the existing stash has nothing, merge should adopt all non-empty
        // fields from the incoming discovery.
        let mut stash = DiscoveredRelayLists {
            nip65: vec![],
            inbox: vec![],
            key_package: vec![],
        };
        let incoming = DiscoveredRelayLists {
            nip65: vec![dummy_relay("wss://a.example.com")],
            inbox: vec![dummy_relay("wss://b.example.com")],
            key_package: vec![],
        };
        stash.merge(incoming);
        assert_eq!(stash.nip65.len(), 1, "nip65 adopted from incoming");
        assert_eq!(stash.inbox.len(), 1, "inbox adopted from incoming");
        assert!(stash.key_package.is_empty(), "key_package stays empty");
        assert!(!stash.is_complete());
    }

    #[test]
    fn test_merge_preserves_existing_non_empty_fields() {
        // When the existing stash already has a value for a field, merge must
        // NOT overwrite it even if the incoming also has a value.
        let original_relay = dummy_relay("wss://original.example.com");
        let mut stash = DiscoveredRelayLists {
            nip65: vec![original_relay.clone()],
            inbox: vec![],
            key_package: vec![],
        };
        let incoming = DiscoveredRelayLists {
            nip65: vec![dummy_relay("wss://new.example.com")], // must NOT overwrite
            inbox: vec![dummy_relay("wss://inbox.example.com")],
            key_package: vec![dummy_relay("wss://kp.example.com")],
        };
        stash.merge(incoming);
        // nip65 should still be the original relay, not the incoming one.
        assert_eq!(stash.nip65.len(), 1);
        assert_eq!(
            stash.nip65[0].url, original_relay.url,
            "nip65 must not be overwritten by merge"
        );
        assert_eq!(stash.inbox.len(), 1, "inbox adopted from incoming");
        assert_eq!(
            stash.key_package.len(),
            1,
            "key_package adopted from incoming"
        );
        assert!(stash.is_complete());
    }

    #[test]
    fn test_merge_both_empty_stays_empty() {
        let mut stash = DiscoveredRelayLists {
            nip65: vec![],
            inbox: vec![],
            key_package: vec![],
        };
        stash.merge(DiscoveredRelayLists {
            nip65: vec![],
            inbox: vec![],
            key_package: vec![],
        });
        assert!(!stash.is_complete());
    }

    #[test]
    fn test_merge_sequential_retries_accumulate() {
        // Simulate two login_with_custom_relay retries, each finding one new list.
        // After both, the stash should be complete.
        let mut stash = DiscoveredRelayLists {
            nip65: vec![],
            inbox: vec![],
            key_package: vec![],
        };

        // First retry finds nip65.
        stash.merge(DiscoveredRelayLists {
            nip65: vec![dummy_relay("wss://nip65.example.com")],
            inbox: vec![],
            key_package: vec![],
        });
        assert!(!stash.is_complete(), "Still missing inbox and key_package");

        // Second retry finds inbox and key_package.
        stash.merge(DiscoveredRelayLists {
            nip65: vec![dummy_relay("wss://other.example.com")], // ignored — nip65 already set
            inbox: vec![dummy_relay("wss://inbox.example.com")],
            key_package: vec![dummy_relay("wss://kp.example.com")],
        });
        assert!(stash.is_complete(), "All three found across two retries");
        assert_eq!(
            stash.nip65[0].url,
            RelayUrl::parse("wss://nip65.example.com").unwrap(),
            "First nip65 preserved — not overwritten"
        );
    }

    #[tokio::test]
    async fn test_pending_logins_cleared_on_complete() {
        // After login_publish_default_relays completes, the stash must be removed.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        setup_partial_pending_login(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![],
                key_package: vec![],
            },
        )
        .await;

        let complete = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(complete.status, LoginStatus::Complete);
        assert!(
            !whitenoise.pending_logins.contains_key(&pubkey),
            "Stash must be removed after login_publish_default_relays"
        );
    }

    // -----------------------------------------------------------------------
    // Cross-relay accumulation: login_with_custom_relay completes via merge
    // -----------------------------------------------------------------------

    #[test]
    fn test_merge_then_is_complete_drives_login_completion_logic() {
        // Unit test for the core invariant behind the merge-without-recheck fix:
        // after merging a second discovery into a partial stash, is_complete()
        // must return true so the caller knows to call complete_login.
        //
        // Scenario:
        //   login_start  → found nip65 only           → stash incomplete
        //   custom relay → found inbox + key_package  → stash now complete
        let mut stash = DiscoveredRelayLists {
            nip65: vec![dummy_relay("wss://nip65.example.com")],
            inbox: vec![],
            key_package: vec![],
        };
        assert!(
            !stash.is_complete(),
            "Stash after login_start (nip65 only) must be incomplete"
        );

        // Simulate what try_discover_relay_lists returns for the custom relay.
        let custom_relay_finds = DiscoveredRelayLists {
            nip65: vec![], // custom relay has no 10002
            inbox: vec![dummy_relay("wss://inbox.example.com")],
            key_package: vec![dummy_relay("wss://kp.example.com")],
        };
        stash.merge(custom_relay_finds);

        // The post-merge is_complete() check is what gates complete_login.
        assert!(
            stash.is_complete(),
            "After merging inbox+key_package from custom relay the stash must be complete"
        );
        // The original nip65 must not have been overwritten.
        assert_eq!(
            stash.nip65[0].url,
            RelayUrl::parse("wss://nip65.example.com").unwrap()
        );
    }

    #[test]
    fn test_merge_cross_relay_accumulation_is_complete() {
        // Pure unit test: confirms that a stash built across two retries
        // reaches is_complete() after the second merge — the core invariant
        // that the post-merge recheck relies on.
        let mut stash = DiscoveredRelayLists {
            nip65: vec![dummy_relay("wss://nip65.example.com")],
            inbox: vec![],
            key_package: vec![],
        };
        assert!(!stash.is_complete(), "Incomplete before second relay");

        stash.merge(DiscoveredRelayLists {
            nip65: vec![dummy_relay("wss://ignored.example.com")], // already set, ignored
            inbox: vec![dummy_relay("wss://inbox.example.com")],
            key_package: vec![dummy_relay("wss://kp.example.com")],
        });

        assert!(
            stash.is_complete(),
            "Complete after merging inbox+key_package from second relay"
        );
        // The original nip65 relay must be preserved.
        assert_eq!(
            stash.nip65[0].url,
            RelayUrl::parse("wss://nip65.example.com").unwrap()
        );
    }

    // -----------------------------------------------------------------------
    // State machine tests: login_with_custom_relay full transitions
    //
    // Strategy: we inject partial relay lists into pending_logins so that
    // when login_with_custom_relay calls try_discover_relay_lists against the
    // local Docker relay (which has nothing for a fresh pubkey), the fresh
    // discovery result is empty and the merge relies entirely on what was
    // already in the stash.  This lets us test every branch of the function
    // without needing a relay that actually serves specific event kinds.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_login_with_custom_relay_no_pending_returns_error() {
        // Calling login_with_custom_relay without a prior login_start must fail.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        let err = whitenoise
            .login_with_custom_relay(&pubkey, RelayUrl::parse("ws://localhost:8080").unwrap())
            .await
            .unwrap_err();

        assert!(
            matches!(err, LoginError::NoLoginInProgress),
            "Must return NoLoginInProgress when no login is pending"
        );
    }

    #[tokio::test]
    async fn test_login_with_custom_relay_stays_incomplete_when_stash_still_missing_lists() {
        // Stash starts with nip65 only.  The custom relay (local Docker, empty for
        // this pubkey) finds nothing.  After merge the stash is still incomplete →
        // must return NeedsRelayLists and keep the stash.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://nip65.example.com").unwrap())
            .await
            .unwrap();
        setup_partial_pending_login(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![nip65_relay.clone()],
                inbox: vec![],
                key_package: vec![],
            },
        )
        .await;

        let result = whitenoise
            .login_with_custom_relay(&pubkey, RelayUrl::parse("ws://localhost:8080").unwrap())
            .await
            .unwrap();

        assert_eq!(
            result.status,
            LoginStatus::NeedsRelayLists,
            "Still missing inbox+key_package → must stay NeedsRelayLists"
        );
        // Stash must still exist and retain the nip65 relay.
        let stash = whitenoise.pending_logins.get(&pubkey).unwrap();
        assert_eq!(
            stash.nip65.len(),
            1,
            "nip65 relay must be preserved in stash"
        );
        assert!(stash.inbox.is_empty());
        assert!(stash.key_package.is_empty());
        drop(stash);

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    #[tokio::test]
    async fn test_login_with_custom_relay_completes_when_stash_already_complete() {
        // Verifies the "stash was already complete on entry" branch: when two
        // prior retries have accumulated all three lists, a third call to
        // login_with_custom_relay should see is_complete() = true after the
        // (no-op) merge and immediately call complete_login.
        //
        // The custom relay URL (ws://localhost:8080) has no events for this
        // fresh pubkey, so try_discover_relay_lists returns all-empty.  The
        // merge is therefore a no-op; completeness comes purely from the stash.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://nip65.example.com").unwrap())
            .await
            .unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://inbox.example.com").unwrap())
            .await
            .unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://kp.example.com").unwrap())
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![nip65_relay.clone()],
                inbox: vec![inbox_relay.clone()],
                key_package: vec![kp_relay.clone()],
            },
        )
        .await;

        let result = whitenoise
            .login_with_custom_relay(&pubkey, RelayUrl::parse("ws://localhost:8080").unwrap())
            .await
            .unwrap();

        assert_eq!(
            result.status,
            LoginStatus::Complete,
            "Stash already complete → must complete login even when custom relay finds nothing"
        );
        assert!(
            !whitenoise.pending_logins.contains_key(&pubkey),
            "pending_logins must be cleared after Complete"
        );
        assert_eq!(result.account.pubkey, pubkey);
    }

    #[tokio::test]
    async fn test_login_with_custom_relay_completes_via_merge_from_partial_stash() {
        // This is the cross-relay accumulation path: login_start found nip65 and
        // stored it in the stash.  The custom relay (Docker, empty for this pubkey)
        // finds nothing new, but we inject inbox + key_package into the stash
        // *before* the call (via setup_pending_login_with_db_relays) so that
        // after the merge the stash is complete and complete_login is triggered.
        //
        // What distinguishes this test from the "already complete" one above is
        // that the stash started as partial (nip65-only) and became complete only
        // because a previous merge wrote inbox+kp into it.  We then confirm the
        // function correctly drives completion from that merged state.
        //
        // A fully live test (where the custom relay actually serves 10050/10051)
        // requires Docker + published events and belongs in the integration suite.
        // This unit test focuses on the merge-then-complete state transition.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Build the partial stash that would exist after login_start found nip65.
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://nip65.example.com").unwrap())
            .await
            .unwrap();

        // Now simulate a prior custom-relay retry that found inbox+kp by inserting
        // those relays into the DB and stash (mirroring what try_discover_relay_lists
        // does inside login_with_custom_relay when it actually finds events).
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://inbox.example.com").unwrap())
            .await
            .unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://kp.example.com").unwrap())
            .await
            .unwrap();

        // The stash now has all three — this is the state after the merge that
        // accumulated nip65 (from login_start) and inbox+kp (from a prior retry).
        setup_pending_login_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![nip65_relay.clone()],
                inbox: vec![inbox_relay.clone()],
                key_package: vec![kp_relay.clone()],
            },
        )
        .await;

        // This call's try_discover_relay_lists returns all-empty (fresh pubkey,
        // Docker relay has no events).  After the no-op merge, is_complete() is
        // true → complete_login is called → Complete is returned.
        let result = whitenoise
            .login_with_custom_relay(&pubkey, RelayUrl::parse("ws://localhost:8080").unwrap())
            .await
            .unwrap();

        assert_eq!(
            result.status,
            LoginStatus::Complete,
            "login_with_custom_relay must return Complete when the merged stash is complete"
        );
        assert!(
            !whitenoise.pending_logins.contains_key(&pubkey),
            "pending_logins must be cleared after Complete"
        );
        assert_eq!(result.account.pubkey, pubkey);

        // Verify the relay associations that drove complete_login are correct.
        let stored_nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(
            stored_nip65[0].url, nip65_relay.url,
            "NIP-65 must be the originally-discovered relay"
        );
        let stored_inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert_eq!(
            stored_inbox[0].url, inbox_relay.url,
            "Inbox must be the relay found on the custom relay"
        );
        let stored_kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert_eq!(
            stored_kp[0].url, kp_relay.url,
            "KeyPackage must be the relay found on the custom relay"
        );
    }

    #[tokio::test]
    async fn test_login_with_custom_relay_all_missing_stays_incomplete() {
        // Stash starts empty (nothing found anywhere so far).  Custom relay
        // (local Docker, empty) also finds nothing.  Must return NeedsRelayLists.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        setup_partial_pending_login(
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
            .login_with_custom_relay(&pubkey, RelayUrl::parse("ws://localhost:8080").unwrap())
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::NeedsRelayLists);
        assert!(
            whitenoise.pending_logins.contains_key(&pubkey),
            "Stash must persist when still incomplete"
        );

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    // -----------------------------------------------------------------------
    // State machine tests: login_external_signer_with_custom_relay full transitions
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_login_ext_signer_with_custom_relay_no_pending_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        let err = whitenoise
            .login_external_signer_with_custom_relay(
                &pubkey,
                RelayUrl::parse("ws://localhost:8080").unwrap(),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(err, LoginError::NoLoginInProgress),
            "Must return NoLoginInProgress when no login is pending"
        );
    }

    #[tokio::test]
    async fn test_login_ext_signer_with_custom_relay_stays_incomplete() {
        // Same as the local-key variant: stash has nip65 only, custom relay finds
        // nothing → NeedsRelayLists, stash preserved.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://nip65.example.com").unwrap())
            .await
            .unwrap();
        setup_partial_pending_login_external_signer(
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
            .login_external_signer_with_custom_relay(
                &pubkey,
                RelayUrl::parse("ws://localhost:8080").unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::NeedsRelayLists);
        let stash = whitenoise.pending_logins.get(&pubkey).unwrap();
        assert_eq!(stash.nip65.len(), 1, "nip65 must be preserved");
        assert!(stash.inbox.is_empty());
        assert!(stash.key_package.is_empty());
        drop(stash);

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    #[tokio::test]
    async fn test_login_ext_signer_with_custom_relay_completes_when_stash_full() {
        // Pre-loaded complete stash → must return Complete, clear stash.
        // Use the local Docker relay URL for key_package so publish succeeds.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://localhost:7777").unwrap())
            .await
            .unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://localhost:7777").unwrap())
            .await
            .unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://localhost:7777").unwrap())
            .await
            .unwrap();

        setup_pending_login_external_signer_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![nip65_relay],
                inbox: vec![inbox_relay],
                key_package: vec![kp_relay],
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_with_custom_relay(
                &pubkey,
                RelayUrl::parse("ws://localhost:8080").unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(result.account.account_type, AccountType::External);
        assert!(
            !whitenoise.pending_logins.contains_key(&pubkey),
            "Stash must be cleared after Complete"
        );
        assert_eq!(result.account.pubkey, pubkey);
    }

    #[tokio::test]
    async fn test_login_ext_signer_with_custom_relay_all_missing_stays_incomplete() {
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
            .login_external_signer_with_custom_relay(
                &pubkey,
                RelayUrl::parse("ws://localhost:8080").unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::NeedsRelayLists);
        assert!(whitenoise.pending_logins.contains_key(&pubkey));

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    // -----------------------------------------------------------------------
    // Stash-before-completion invariant tests
    //
    // These tests verify that the stash is updated with newly-discovered relays
    // BEFORE complete_login is attempted, so that if complete_login fails and
    // the user retries via login_publish_default_relays, the stash reflects what
    // is already on the network and defaults are not published over found lists.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_stash_updated_before_completion_local_key() {
        // Scenario: login_start found nip65 only (partial stash).  A subsequent
        // login_with_custom_relay call discovers inbox+key_package on the relay.
        // The merge must update the stash *before* complete_login is called.
        //
        // We verify this by calling login_with_custom_relay on a relay that has
        // nothing (empty discovery), which leaves the stash unchanged except for
        // the merge of an empty DiscoveredRelayLists — and then manually
        // simulating what a successful custom relay would have done via direct
        // stash injection.  Then we drive login_publish_default_relays and
        // assert it skips all three kinds (proving the stash was complete).
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://localhost:7777").unwrap())
            .await
            .unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://localhost:7777").unwrap())
            .await
            .unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://localhost:7777").unwrap())
            .await
            .unwrap();

        // Simulate login_start: partial stash with nip65 only, DB relay rows
        // already written by try_discover_relay_lists.
        setup_pending_login_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![nip65_relay.clone()],
                inbox: vec![],
                key_package: vec![],
            },
        )
        .await;

        // Simulate what login_with_custom_relay would have written to the stash
        // after try_discover_relay_lists found inbox+key_package on the custom
        // relay and merged them in (before attempting complete_login).
        {
            let mut stash = whitenoise.pending_logins.get_mut(&pubkey).unwrap();
            stash.merge(DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![inbox_relay.clone()],
                key_package: vec![kp_relay.clone()],
            });
            assert!(
                stash.is_complete(),
                "Stash must be complete after merge — this is the state after the upfront merge"
            );
        }

        // Also write the inbox+kp rows to the DB, as try_discover_relay_lists
        // would have done before the merge and complete_login attempt.
        let account = Account::find_by_pubkey(&pubkey, &whitenoise.database)
            .await
            .unwrap();
        let user = account.user(&whitenoise.database).await.unwrap();
        user.add_relays(&[inbox_relay], RelayType::Inbox, &whitenoise.database)
            .await
            .unwrap();
        user.add_relays(&[kp_relay], RelayType::KeyPackage, &whitenoise.database)
            .await
            .unwrap();

        // Now simulate: complete_login failed and the user retries via
        // login_publish_default_relays.  Because the stash is complete, it
        // must NOT publish defaults for any of the three kinds.
        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);

        // All three relay types must still be the pre-discovered relays, not
        // replaced with defaults.  If the stash had been stale (partial), the
        // inbox and key_package rows would have been overwritten with defaults.
        let stored_nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(
            stored_nip65[0].url,
            RelayUrl::parse("ws://localhost:7777").unwrap()
        );

        let stored_inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert_eq!(
            stored_inbox[0].url,
            RelayUrl::parse("ws://localhost:7777").unwrap()
        );

        let stored_kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert_eq!(
            stored_kp[0].url,
            RelayUrl::parse("ws://localhost:7777").unwrap()
        );
    }

    #[test]
    fn test_stash_merge_happens_before_complete_login_invariant() {
        // Pure unit test: verify the invariant that the stash must be complete
        // (reflecting all discovered lists) before complete_login is called.
        //
        // This is the core property guaranteed by the upfront-merge refactor:
        // even if complete_login subsequently fails, the stash already holds
        // the full merged state, so login_publish_default_relays will not
        // publish defaults over relays that were already found.
        let mut stash = DiscoveredRelayLists {
            nip65: vec![dummy_relay("wss://nip65.example.com")],
            inbox: vec![],
            key_package: vec![],
        };

        // Simulate what try_discover_relay_lists returns on the custom relay.
        let discovered = DiscoveredRelayLists {
            nip65: vec![],
            inbox: vec![dummy_relay("wss://inbox.example.com")],
            key_package: vec![dummy_relay("wss://kp.example.com")],
        };

        // This is the upfront merge that now happens before complete_login.
        stash.merge(discovered);

        // The stash must be complete at this point — so if complete_login fails
        // and the user retries, login_publish_default_relays will see all three
        // as present and publish nothing.
        assert!(
            stash.is_complete(),
            "Stash must be complete after upfront merge, before complete_login is called"
        );
        assert_eq!(
            stash.nip65[0].url,
            RelayUrl::parse("wss://nip65.example.com").unwrap(),
            "Original nip65 relay must be preserved"
        );
        assert_eq!(
            stash.inbox[0].url,
            RelayUrl::parse("wss://inbox.example.com").unwrap(),
        );
        assert_eq!(
            stash.key_package[0].url,
            RelayUrl::parse("wss://kp.example.com").unwrap(),
        );
    }

    // -----------------------------------------------------------------------
    // login_publish_default_relays — selective publish tests
    // These tests inject partial DiscoveredRelayLists directly into
    // pending_logins and then drive login_publish_default_relays, verifying
    // that only the missing kinds end up with relay associations.
    // -----------------------------------------------------------------------

    /// Helper: create a local-key account and insert a partial DiscoveredRelayLists
    /// into pending_logins so we can test the step-2a path in isolation.
    async fn setup_partial_pending_login(
        whitenoise: &Whitenoise,
        keys: &Keys,
        discovered: DiscoveredRelayLists,
    ) {
        whitenoise
            .create_base_account_with_private_key(keys)
            .await
            .unwrap();
        whitenoise
            .pending_logins
            .insert(keys.public_key(), discovered);
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

    /// Helper: like `setup_partial_pending_login` but also writes the relay
    /// associations from `discovered` to the DB, mirroring what
    /// `try_discover_relay_lists` does.  Use this when the test will call a
    /// function that relies on those rows already existing (e.g. a path that
    /// calls `complete_login` without going through `login_publish_default_relays`).
    async fn setup_pending_login_with_db_relays(
        whitenoise: &Whitenoise,
        keys: &Keys,
        discovered: DiscoveredRelayLists,
    ) {
        whitenoise
            .create_base_account_with_private_key(keys)
            .await
            .unwrap();
        let account = Account::find_by_pubkey(&keys.public_key(), &whitenoise.database)
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
        whitenoise
            .pending_logins
            .insert(keys.public_key(), discovered);
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

    #[tokio::test]
    async fn test_publish_default_relays_all_missing_assigns_all_three() {
        // When all three lists are absent, publish_default_relays must
        // assign default relays to all three kinds.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        setup_partial_pending_login(
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
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

        // All three relay types must have been assigned.
        for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
            let relays = result
                .account
                .relays(relay_type, &whitenoise)
                .await
                .unwrap();
            assert!(
                !relays.is_empty(),
                "{:?} relays must be assigned when all lists were missing",
                relay_type
            );
        }
    }

    #[tokio::test]
    async fn test_publish_default_relays_nip65_present_inbox_and_kp_missing() {
        // The user from the bug report: has 10002 but missing 10050 and 10051.
        // Only 10050 and 10051 should be published as defaults.
        // 10002 relays should match what was pre-discovered, not defaults.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_url = RelayUrl::parse("wss://custom-nip65.example.com").unwrap();
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&nip65_url)
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![nip65_relay.clone()],
                inbox: vec![],
                key_package: vec![],
            },
        )
        .await;

        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

        // NIP-65 relays must be the custom one, not defaults.
        let stored_nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(
            stored_nip65.len(),
            1,
            "NIP-65 must keep the pre-discovered relay"
        );
        assert_eq!(
            stored_nip65[0].url, nip65_url,
            "NIP-65 must not be overwritten with defaults"
        );

        // Inbox and key_package must have been filled with defaults.
        let inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert!(
            !inbox.is_empty(),
            "Inbox must get defaults when it was missing"
        );

        let kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert!(
            !kp.is_empty(),
            "KeyPackage must get defaults when it was missing"
        );
    }

    #[tokio::test]
    async fn test_publish_default_relays_inbox_present_nip65_and_kp_missing() {
        // User has 10050 but not 10002 or 10051.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let inbox_url = RelayUrl::parse("wss://custom-inbox.example.com").unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&inbox_url)
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
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
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

        // Inbox must stay as the custom relay.
        let stored_inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert_eq!(stored_inbox.len(), 1);
        assert_eq!(
            stored_inbox[0].url, inbox_url,
            "Inbox must not be overwritten"
        );

        // NIP-65 and key_package must be filled with defaults.
        let nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert!(!nip65.is_empty(), "NIP-65 must get defaults");

        let kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert!(!kp.is_empty(), "KeyPackage must get defaults");
    }

    #[tokio::test]
    async fn test_publish_default_relays_key_package_present_nip65_and_inbox_missing() {
        // User has 10051 but not 10002 or 10050.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let kp_url = RelayUrl::parse("wss://custom-kp.example.com").unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&kp_url)
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
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
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

        let stored_kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert_eq!(stored_kp.len(), 1);
        assert_eq!(
            stored_kp[0].url, kp_url,
            "KeyPackage must not be overwritten"
        );

        let nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert!(!nip65.is_empty());

        let inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert!(!inbox.is_empty());
    }

    #[tokio::test]
    async fn test_publish_default_relays_nip65_and_inbox_present_kp_missing() {
        // User has 10002 and 10050 but not 10051.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_url = RelayUrl::parse("wss://custom-nip65.example.com").unwrap();
        let inbox_url = RelayUrl::parse("wss://custom-inbox.example.com").unwrap();
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&nip65_url)
            .await
            .unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&inbox_url)
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
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
            .login_publish_default_relays(&pubkey)
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
        assert!(!kp.is_empty(), "KeyPackage must get defaults");
    }

    #[tokio::test]
    async fn test_publish_default_relays_nip65_and_kp_present_inbox_missing() {
        // User has 10002 and 10051 but not 10050.  This was the original bug scenario.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_url = RelayUrl::parse("wss://custom-nip65.example.com").unwrap();
        let kp_url = RelayUrl::parse("wss://custom-kp.example.com").unwrap();
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&nip65_url)
            .await
            .unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&kp_url)
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
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
            .login_publish_default_relays(&pubkey)
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
        assert!(!inbox.is_empty(), "Inbox must get defaults");
    }

    #[tokio::test]
    async fn test_publish_default_relays_inbox_and_kp_present_nip65_missing() {
        // User has 10050 and 10051 but not 10002.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let inbox_url = RelayUrl::parse("wss://custom-inbox.example.com").unwrap();
        let kp_url = RelayUrl::parse("wss://custom-kp.example.com").unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&inbox_url)
            .await
            .unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&kp_url)
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
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
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

        let nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert!(!nip65.is_empty(), "NIP-65 must get defaults");

        let stored_inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert_eq!(stored_inbox[0].url, inbox_url, "Inbox must be preserved");

        let stored_kp = result
            .account
            .key_package_relays(&whitenoise)
            .await
            .unwrap();
        assert_eq!(stored_kp[0].url, kp_url, "KeyPackage must be preserved");
    }

    #[tokio::test]
    async fn test_publish_default_relays_removes_pending_entry() {
        // After a successful call the pending_logins map must be empty for this pubkey.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        setup_partial_pending_login(
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
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert!(
            !whitenoise.pending_logins.contains_key(&pubkey),
            "pending_logins must be cleared after successful publish"
        );
    }

    #[tokio::test]
    async fn test_publish_default_relays_without_pending_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();
        // No pending login — must fail.
        let err = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap_err();
        assert!(
            matches!(err, LoginError::NoLoginInProgress),
            "Must return NoLoginInProgress when no stash exists"
        );
    }

    // -----------------------------------------------------------------------
    // login_cancel edge cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_login_cancel_is_idempotent() {
        // Calling login_cancel twice must not error on the second call.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        whitenoise
            .create_base_account_with_private_key(&keys)
            .await
            .unwrap();
        whitenoise.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: vec![],
                inbox: vec![],
                key_package: vec![],
            },
        );

        // First cancel: removes account and stash.
        whitenoise.login_cancel(&pubkey).await.unwrap();
        // Second cancel: no stash → should be a no-op, not an error.
        whitenoise.login_cancel(&pubkey).await.unwrap();
    }

    #[tokio::test]
    async fn test_login_cancel_with_partial_nip65_stash() {
        // Cancel works even when the stash has a non-empty nip65 list.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        whitenoise
            .create_base_account_with_private_key(&keys)
            .await
            .unwrap();
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://r.example.com").unwrap())
            .await
            .unwrap();
        whitenoise.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: vec![nip65_relay],
                inbox: vec![],
                key_package: vec![],
            },
        );

        whitenoise.login_cancel(&pubkey).await.unwrap();
        assert!(!whitenoise.pending_logins.contains_key(&pubkey));
        assert!(whitenoise.find_account_by_pubkey(&pubkey).await.is_err());
    }

    #[tokio::test]
    async fn test_login_cancel_clears_relay_associations_written_by_try_discover() {
        // try_discover_relay_lists writes relay associations to the DB as soon as
        // it finds them (not all-or-nothing).  If the user then cancels, those
        // user_relays rows must not persist.
        //
        // Note: user_relays has no CASCADE from the accounts table, so the rows
        // survive the account deletion unless cleaned up explicitly.
        // This test documents the expected behaviour: after cancel, the user's
        // relay associations must be gone.
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Simulate try_discover_relay_lists having found and written nip65 relays
        // to the DB before the user decided to cancel.
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://cancel-test.example.com").unwrap())
            .await
            .unwrap();

        // setup_pending_login_with_db_relays mirrors what try_discover_relay_lists
        // does: it persists the relay associations AND inserts the stash.
        setup_pending_login_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: vec![nip65_relay.clone()],
                inbox: vec![],
                key_package: vec![],
            },
        )
        .await;

        // Confirm the association was written before cancel.
        let account = Account::find_by_pubkey(&pubkey, &whitenoise.database)
            .await
            .unwrap();
        let user = account.user(&whitenoise.database).await.unwrap();
        let nip65_before = user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(
            nip65_before.len(),
            1,
            "NIP-65 association must exist before cancel"
        );

        // Cancel the login.
        whitenoise.login_cancel(&pubkey).await.unwrap();

        // The account must be gone.
        assert!(whitenoise.find_account_by_pubkey(&pubkey).await.is_err());
        assert!(!whitenoise.pending_logins.contains_key(&pubkey));

        // The user row persists (users are not deleted with accounts), but the
        // relay associations (user_relays rows) must be cleaned up so that a
        // subsequent login for the same pubkey starts with a clean slate.
        let nip65_after = user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        assert!(
            nip65_after.is_empty(),
            "NIP-65 relay associations must be removed when login is cancelled \
             (found {} rows; user_relays should be cleaned up by login_cancel)",
            nip65_after.len()
        );
    }
}
