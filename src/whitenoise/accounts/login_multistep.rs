use nostr_sdk::prelude::*;

use super::{Account, DiscoveredRelayLists, LoginError, LoginResult, LoginStatus};
use crate::RelayType;
use crate::perf_instrument;
use crate::perf_span;
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
    #[perf_instrument("accounts")]
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

        // If this account is already fully logged in, return it as-is.
        // Re-running create_base_account_with_private_key would reset
        // last_synced_at to NULL (poisoning compute_global_since_timestamp
        // for all accounts), and re-running activate_account would create
        // duplicate subscriptions and needlessly kill in-progress background
        // tasks.
        //
        // Three conditions must hold to short-circuit:
        // 1. Account row exists in the database.
        // 2. No pending multi-step login is in progress (pending_logins).
        // 3. An active session exists (background_task_cancellation is set
        //    by activate_account). Without this check, a concurrent second
        //    call could see the DB row created by a first call that hasn't
        //    finished activation yet and return Complete prematurely.
        if let Ok(existing) = Account::find_by_pubkey(&pubkey, &self.database).await
            && !self.pending_logins.contains_key(&pubkey)
            && self.background_task_cancellation.contains_key(&pubkey)
        {
            tracing::debug!(
                target: "whitenoise::accounts",
                "Account {} is already logged in, returning existing account",
                pubkey.to_hex()
            );
            return Ok(LoginResult {
                account: existing,
                status: LoginStatus::Complete,
            });
        }

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
                discovered.relays(RelayType::Inbox),
                discovered.relays(RelayType::KeyPackage),
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

    /// Step 2a: publish default relay lists for any that are missing, then complete login.
    ///
    /// Called after [`Whitenoise::login_start`] returned [`LoginStatus::NeedsRelayLists`].
    /// Uses the partial discovery results stashed during step 1 to determine which
    /// of the three relay list kinds (10002, 10050, 10051) were already found on the
    /// network.  Default relays are assigned and published **only for the missing
    /// ones**; existing lists are left untouched.
    #[perf_instrument("accounts")]
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
            !discovered.found(RelayType::Nip65),
            !discovered.found(RelayType::Inbox),
            !discovered.found(RelayType::KeyPackage),
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

        // Persist missing relay lists to the local database, then publish
        // them to the network concurrently. DB writes are fast (sub-ms) and
        // use the same connection, so they run sequentially. The publishes
        // are independent Nostr events (kinds 10002, 10050, 10051) targeting
        // the same relay set, so they run concurrently — same pattern as
        // setup_relays_for_existing_account.
        let _publish_span = perf_span!("accounts::login_publish_defaults::publish_missing");
        for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
            if !discovered.found(relay_type) {
                user.add_relays(&default_relays, relay_type, &self.database)
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

        let nip65_publish = async {
            if !discovered.found(RelayType::Nip65) {
                self.publish_relay_list(
                    &default_relays,
                    RelayType::Nip65,
                    &publish_to_relays,
                    keys.clone(),
                )
                .await
            } else {
                Ok(())
            }
        };
        let inbox_publish = async {
            if !discovered.found(RelayType::Inbox) {
                self.publish_relay_list(
                    &default_relays,
                    RelayType::Inbox,
                    &publish_to_relays,
                    keys.clone(),
                )
                .await
            } else {
                Ok(())
            }
        };
        let key_package_publish = async {
            if !discovered.found(RelayType::KeyPackage) {
                self.publish_relay_list(
                    &default_relays,
                    RelayType::KeyPackage,
                    &publish_to_relays,
                    keys.clone(),
                )
                .await
            } else {
                Ok(())
            }
        };

        let (nip65_result, inbox_result, kp_result) =
            tokio::join!(nip65_publish, inbox_publish, key_package_publish);

        // NIP-65 publish failure is always fatal.
        nip65_result.map_err(LoginError::from)?;

        for (relay_type, result) in [
            (RelayType::Inbox, inbox_result),
            (RelayType::KeyPackage, kp_result),
        ] {
            if let Err(error) = result {
                tracing::warn!(
                    target: "whitenoise::accounts",
                    pubkey = %pubkey,
                    ?relay_type,
                    "Failed to publish default relay list to preserved NIP-65 relays; continuing login with local relay state: {error}"
                );
            }
        }

        drop(_publish_span);

        tracing::debug!(
            target: "whitenoise::accounts",
            "Missing relay lists published, activating account"
        );

        self.complete_login(
            &account,
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
    #[perf_instrument("accounts")]
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
            self.sync_discovered_relay_lists(&account, &merged).await?;
            self.complete_login(
                &account,
                merged.relays(RelayType::Inbox),
                merged.relays(RelayType::KeyPackage),
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
    /// Returns a [`DiscoveredRelayLists`] whose fields are `Some(relays)` when
    /// a list was found on the network (even if it contains zero relays) and
    /// `None` when the list was not published at all.  When 10050/10051 are
    /// searched, the source is the NIP-65 relays (if found); otherwise the
    /// original `source_relays` are used as a fallback so we still try all
    /// three even when 10002 is missing.
    ///
    /// Callers should check [`DiscoveredRelayLists::is_complete`] to determine
    /// whether login can proceed or whether the user must provide relay lists.
    #[perf_instrument("accounts")]
    pub(crate) async fn try_discover_relay_lists(
        &self,
        account: &mut Account,
        source_relays: &[Relay],
    ) -> core::result::Result<DiscoveredRelayLists, LoginError> {
        // Step 1: Fetch NIP-65 relay list (kind 10002) from the source relays.
        let _fetch_nip65 = perf_span!("accounts::discover_relay_lists::fetch_nip65");
        let nip65_relays = self
            .fetch_existing_relays(account.pubkey, RelayType::Nip65, source_relays)
            .await
            .map_err(LoginError::from)?;
        drop(_fetch_nip65);

        // Use the discovered NIP-65 relays as the source for 10050/10051 when
        // available; otherwise fall back to the original source relays so we
        // still make a best-effort attempt even when 10002 is absent.
        let secondary_source = match &nip65_relays {
            Some(relays) if !relays.is_empty() => relays.clone(),
            _ => source_relays.to_vec(),
        };

        // Steps 2 & 3: Fetch Inbox (10050) and KeyPackage (10051) concurrently.
        let _fetch_inbox_kp = perf_span!("accounts::discover_relay_lists::fetch_inbox_and_kp");
        let pubkey = account.pubkey;
        let (inbox_result, key_package_result) = tokio::join!(
            self.fetch_existing_relays(pubkey, RelayType::Inbox, &secondary_source),
            self.fetch_existing_relays(pubkey, RelayType::KeyPackage, &secondary_source),
        );
        let inbox_relays = inbox_result.map_err(LoginError::from)?;
        let key_package_relays = key_package_result.map_err(LoginError::from)?;
        drop(_fetch_inbox_kp);

        // Persist the exact discovered network state, including empty results,
        // so stale relay rows are removed when a relay list no longer exists.
        let _sync_span = perf_span!("accounts::discover_relay_lists::sync_to_db");
        self.sync_account_relays(
            account,
            nip65_relays.as_deref().unwrap_or(&[]),
            RelayType::Nip65,
        )
        .await
        .map_err(LoginError::from)?;
        self.sync_account_relays(
            account,
            inbox_relays.as_deref().unwrap_or(&[]),
            RelayType::Inbox,
        )
        .await
        .map_err(LoginError::from)?;
        self.sync_account_relays(
            account,
            key_package_relays.as_deref().unwrap_or(&[]),
            RelayType::KeyPackage,
        )
        .await
        .map_err(LoginError::from)?;
        drop(_sync_span);

        Ok(DiscoveredRelayLists {
            nip65: nip65_relays,
            inbox: inbox_relays,
            key_package: key_package_relays,
        })
    }

    /// Merge newly-discovered relay lists into the pending-login stash and
    /// return a snapshot.  The DashMap lock is released before returning so
    /// the caller is free to do async work with the result.
    pub(crate) fn merge_into_stash(
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

    pub(crate) async fn sync_discovered_relay_lists(
        &self,
        account: &Account,
        discovered: &DiscoveredRelayLists,
    ) -> core::result::Result<(), LoginError> {
        self.sync_account_relays(
            account,
            discovered.relays(RelayType::Nip65),
            RelayType::Nip65,
        )
        .await
        .map_err(LoginError::from)?;
        self.sync_account_relays(
            account,
            discovered.relays(RelayType::Inbox),
            RelayType::Inbox,
        )
        .await
        .map_err(LoginError::from)?;
        self.sync_account_relays(
            account,
            discovered.relays(RelayType::KeyPackage),
            RelayType::KeyPackage,
        )
        .await
        .map_err(LoginError::from)?;
        Ok(())
    }

    /// Activate a local-key account after relay lists have been resolved.
    #[perf_instrument("accounts")]
    pub(crate) async fn complete_login(
        &self,
        account: &Account,
        inbox_relays: &[Relay],
        key_package_relays: &[Relay],
    ) -> core::result::Result<(), LoginError> {
        let user = account
            .user(&self.database)
            .await
            .map_err(LoginError::from)?;
        self.activate_account(account, &user, false, inbox_relays, key_package_relays)
            .await
            .map_err(LoginError::from)
    }

    /// Publishes relay lists using an external signer based on the relay setup configuration.
    ///
    /// Publishes NIP-65, inbox, and key package relay lists only if they need to be
    /// published (i.e., using defaults rather than existing user-configured lists).
    pub(crate) async fn publish_relay_lists_with_signer(
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
}
