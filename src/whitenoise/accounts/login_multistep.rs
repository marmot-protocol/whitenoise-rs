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
            && !self.account_manager.pending_logins.contains_key(&pubkey)
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
            self.account_manager
                .pending_logins
                .insert(pubkey, discovered);
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
            .account_manager
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

        self.account_manager.pending_logins.remove(pubkey);
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
        if !self.account_manager.pending_logins.contains_key(pubkey) {
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
            self.account_manager.pending_logins.remove(pubkey);
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
        if self.account_manager.pending_logins.remove(pubkey).is_none() {
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
    pub(super) async fn try_discover_relay_lists(
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
    pub(super) fn merge_into_stash(
        &self,
        pubkey: &PublicKey,
        discovered: DiscoveredRelayLists,
    ) -> core::result::Result<DiscoveredRelayLists, LoginError> {
        let mut stash = self
            .account_manager
            .pending_logins
            .get_mut(pubkey)
            .ok_or(LoginError::NoLoginInProgress)?;
        stash.merge(discovered);
        let snapshot = stash.clone();
        drop(stash);
        Ok(snapshot)
    }

    pub(super) async fn sync_discovered_relay_lists(
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
    pub(super) async fn complete_login(
        &self,
        account: &Account,
        inbox_relays: &[Relay],
        key_package_relays: &[Relay],
    ) -> core::result::Result<(), LoginError> {
        self.activate_account(account, false, inbox_relays, key_package_relays)
            .await
            .map_err(LoginError::from)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use nostr_sdk::prelude::*;

    use crate::RelayType;
    use crate::whitenoise::Whitenoise;
    use crate::whitenoise::WhitenoiseError;
    use crate::whitenoise::accounts::{
        Account, AccountType, DiscoveredRelayLists, LoginError, LoginResult, LoginStatus,
    };
    use crate::whitenoise::relays::Relay;
    use crate::whitenoise::test_utils::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Build a dummy Relay from a URL string without touching the DB.
    fn dummy_relay(url: &str) -> Relay {
        Relay {
            id: Some(1),
            url: RelayUrl::parse(url).unwrap(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// Create a nostr Client connected to the dev Docker relays and publish all
    /// three relay list events (10002, 10050, 10051) for the given keys.
    async fn publish_relay_lists_to_dev_relays(keys: &Keys) {
        let dev_relays = &["ws://localhost:8080", "ws://localhost:7777"];
        let client = Client::default();
        for relay in dev_relays {
            client.add_relay(*relay).await.unwrap();
        }
        client.connect().await;
        client.set_signer(keys.clone()).await;

        let relay_urls: Vec<String> = dev_relays.iter().map(|s| s.to_string()).collect();

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

        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        client.disconnect().await;
    }

    /// Create a local-key account and insert a partial DiscoveredRelayLists into
    /// pending_logins so we can test the step-2a path in isolation.
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
            .account_manager
            .pending_logins
            .insert(keys.public_key(), discovered);
    }

    /// Create an external-signer account, register its signer, and insert a
    /// partial DiscoveredRelayLists into pending_logins.
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
        whitenoise
            .account_manager
            .pending_logins
            .insert(pubkey, discovered);
    }

    /// Like `setup_partial_pending_login` but also writes relay associations to
    /// the DB, mirroring what `try_discover_relay_lists` does.
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
            .account_manager
            .pending_logins
            .insert(keys.public_key(), discovered);
    }

    /// Like `setup_partial_pending_login_external_signer` but also writes relay
    /// associations to the DB.
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
        whitenoise
            .account_manager
            .pending_logins
            .insert(pubkey, discovered);
    }

    // -----------------------------------------------------------------------
    // DiscoveredRelayLists::is_complete tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_discovered_relay_lists_is_complete_all_present() {
        let lists = DiscoveredRelayLists {
            nip65: Some(vec![dummy_relay("wss://a.example.com")]),
            inbox: Some(vec![dummy_relay("wss://b.example.com")]),
            key_package: Some(vec![dummy_relay("wss://c.example.com")]),
        };
        assert!(
            lists.is_complete(),
            "All three lists present — should be complete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_all_empty() {
        let lists = DiscoveredRelayLists {
            nip65: None,
            inbox: None,
            key_package: None,
        };
        assert!(!lists.is_complete(), "All empty — should not be complete");
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_only_nip65() {
        let lists = DiscoveredRelayLists {
            nip65: Some(vec![dummy_relay("wss://a.example.com")]),
            inbox: None,
            key_package: None,
        };
        assert!(
            !lists.is_complete(),
            "Only nip65 present — must be incomplete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_only_inbox() {
        let lists = DiscoveredRelayLists {
            nip65: None,
            inbox: Some(vec![dummy_relay("wss://b.example.com")]),
            key_package: None,
        };
        assert!(
            !lists.is_complete(),
            "Only inbox present — must be incomplete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_only_key_package() {
        let lists = DiscoveredRelayLists {
            nip65: None,
            inbox: None,
            key_package: Some(vec![dummy_relay("wss://c.example.com")]),
        };
        assert!(
            !lists.is_complete(),
            "Only key_package present — must be incomplete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_nip65_and_inbox_only() {
        let lists = DiscoveredRelayLists {
            nip65: Some(vec![dummy_relay("wss://a.example.com")]),
            inbox: Some(vec![dummy_relay("wss://b.example.com")]),
            key_package: None,
        };
        assert!(
            !lists.is_complete(),
            "nip65 + inbox but no key_package — must be incomplete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_nip65_and_key_package_only() {
        let lists = DiscoveredRelayLists {
            nip65: Some(vec![dummy_relay("wss://a.example.com")]),
            inbox: None,
            key_package: Some(vec![dummy_relay("wss://c.example.com")]),
        };
        assert!(
            !lists.is_complete(),
            "nip65 + key_package but no inbox — must be incomplete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_is_complete_inbox_and_key_package_only() {
        let lists = DiscoveredRelayLists {
            nip65: None,
            inbox: Some(vec![dummy_relay("wss://b.example.com")]),
            key_package: Some(vec![dummy_relay("wss://c.example.com")]),
        };
        assert!(
            !lists.is_complete(),
            "inbox + key_package but no nip65 — must be incomplete"
        );
    }

    #[test]
    fn test_discovered_relay_lists_multiple_relays_per_list() {
        let lists = DiscoveredRelayLists {
            nip65: Some(vec![
                dummy_relay("wss://a1.example.com"),
                dummy_relay("wss://a2.example.com"),
            ]),
            inbox: Some(vec![
                dummy_relay("wss://b1.example.com"),
                dummy_relay("wss://b2.example.com"),
            ]),
            key_package: Some(vec![dummy_relay("wss://c.example.com")]),
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
        let relay_a = dummy_relay("wss://a.example.com");
        let relay_fallback = dummy_relay("wss://fallback.example.com");
        let lists = DiscoveredRelayLists {
            nip65: Some(vec![relay_a.clone()]),
            inbox: None,
            key_package: None,
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
        let relay_fallback = dummy_relay("wss://fallback.example.com");
        let lists = DiscoveredRelayLists {
            nip65: None,
            inbox: None,
            key_package: None,
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
        let nip65_relay = dummy_relay("ws://127.0.0.1:19010");
        let inbox_relay = dummy_relay("ws://127.0.0.1:19011");
        let kp_relay = dummy_relay("ws://127.0.0.1:19012");
        let fallback = dummy_relay("wss://fallback.example.com");
        let fallback_slice = [fallback.clone()];

        let lists = DiscoveredRelayLists {
            nip65: Some(vec![nip65_relay.clone()]),
            inbox: Some(vec![inbox_relay.clone()]),
            key_package: Some(vec![kp_relay.clone()]),
        };

        assert_eq!(
            lists.relays_or(RelayType::Nip65, &fallback_slice)[0].url,
            nip65_relay.url
        );
        assert_eq!(
            lists.relays_or(RelayType::Inbox, &fallback_slice)[0].url,
            inbox_relay.url
        );
        assert_eq!(
            lists.relays_or(RelayType::KeyPackage, &fallback_slice)[0].url,
            kp_relay.url
        );
    }

    #[test]
    fn test_relays_or_fallback_for_all_relay_types_when_all_empty() {
        let fallback = dummy_relay("wss://fallback.example.com");
        let fallback_slice = [fallback.clone()];
        let lists = DiscoveredRelayLists {
            nip65: None,
            inbox: None,
            key_package: None,
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
        let mut stash = DiscoveredRelayLists {
            nip65: None,
            inbox: None,
            key_package: None,
        };
        let incoming = DiscoveredRelayLists {
            nip65: Some(vec![dummy_relay("wss://a.example.com")]),
            inbox: Some(vec![dummy_relay("wss://b.example.com")]),
            key_package: None,
        };
        stash.merge(incoming);
        assert_eq!(
            stash.nip65.as_ref().map_or(0, |v| v.len()),
            1,
            "nip65 adopted from incoming"
        );
        assert_eq!(
            stash.inbox.as_ref().map_or(0, |v| v.len()),
            1,
            "inbox adopted from incoming"
        );
        assert!(stash.key_package.is_none(), "key_package stays empty");
        assert!(!stash.is_complete());
    }

    #[test]
    fn test_merge_preserves_existing_non_empty_fields() {
        let original_relay = dummy_relay("wss://original.example.com");
        let mut stash = DiscoveredRelayLists {
            nip65: Some(vec![original_relay.clone()]),
            inbox: None,
            key_package: None,
        };
        let incoming = DiscoveredRelayLists {
            nip65: Some(vec![dummy_relay("wss://new.example.com")]), // must NOT overwrite
            inbox: Some(vec![dummy_relay("ws://127.0.0.1:19011")]),
            key_package: Some(vec![dummy_relay("ws://127.0.0.1:19012")]),
        };
        stash.merge(incoming);
        assert_eq!(stash.nip65.as_ref().map_or(0, |v| v.len()), 1);
        assert_eq!(
            stash.nip65.as_ref().unwrap()[0].url,
            original_relay.url,
            "nip65 must not be overwritten by merge"
        );
        assert_eq!(
            stash.inbox.as_ref().map_or(0, |v| v.len()),
            1,
            "inbox adopted from incoming"
        );
        assert_eq!(
            stash.key_package.as_ref().map_or(0, |v| v.len()),
            1,
            "key_package adopted from incoming"
        );
        assert!(stash.is_complete());
    }

    #[test]
    fn test_merge_both_empty_stays_empty() {
        let mut stash = DiscoveredRelayLists {
            nip65: None,
            inbox: None,
            key_package: None,
        };
        stash.merge(DiscoveredRelayLists {
            nip65: None,
            inbox: None,
            key_package: None,
        });
        assert!(!stash.is_complete());
    }

    #[test]
    fn test_merge_sequential_retries_accumulate() {
        let mut stash = DiscoveredRelayLists {
            nip65: None,
            inbox: None,
            key_package: None,
        };

        stash.merge(DiscoveredRelayLists {
            nip65: Some(vec![dummy_relay("ws://127.0.0.1:19010")]),
            inbox: None,
            key_package: None,
        });
        assert!(!stash.is_complete(), "Still missing inbox and key_package");

        stash.merge(DiscoveredRelayLists {
            nip65: Some(vec![dummy_relay("wss://other.example.com")]), // ignored — nip65 already set
            inbox: Some(vec![dummy_relay("ws://127.0.0.1:19011")]),
            key_package: Some(vec![dummy_relay("ws://127.0.0.1:19012")]),
        });
        assert!(stash.is_complete(), "All three found across two retries");
        assert_eq!(
            stash.nip65.as_ref().unwrap()[0].url,
            RelayUrl::parse("ws://127.0.0.1:19010").unwrap(),
            "First nip65 preserved — not overwritten"
        );
    }

    #[test]
    fn test_merge_then_is_complete_drives_login_completion_logic() {
        let mut stash = DiscoveredRelayLists {
            nip65: Some(vec![dummy_relay("ws://127.0.0.1:19010")]),
            inbox: None,
            key_package: None,
        };
        assert!(
            !stash.is_complete(),
            "Stash after login_start (nip65 only) must be incomplete"
        );

        stash.merge(DiscoveredRelayLists {
            nip65: None,
            inbox: Some(vec![dummy_relay("ws://127.0.0.1:19011")]),
            key_package: Some(vec![dummy_relay("ws://127.0.0.1:19012")]),
        });

        assert!(
            stash.is_complete(),
            "After merging inbox+key_package the stash must be complete"
        );
        assert_eq!(
            stash.nip65.as_ref().unwrap()[0].url,
            RelayUrl::parse("ws://127.0.0.1:19010").unwrap()
        );
    }

    #[test]
    fn test_merge_cross_relay_accumulation_is_complete() {
        let mut stash = DiscoveredRelayLists {
            nip65: Some(vec![dummy_relay("ws://127.0.0.1:19010")]),
            inbox: None,
            key_package: None,
        };
        assert!(!stash.is_complete(), "Incomplete before second relay");

        stash.merge(DiscoveredRelayLists {
            nip65: Some(vec![dummy_relay("wss://ignored.example.com")]), // already set, ignored
            inbox: Some(vec![dummy_relay("ws://127.0.0.1:19011")]),
            key_package: Some(vec![dummy_relay("ws://127.0.0.1:19012")]),
        });

        assert!(
            stash.is_complete(),
            "Complete after merging inbox+key_package from second relay"
        );
        assert_eq!(
            stash.nip65.as_ref().unwrap()[0].url,
            RelayUrl::parse("ws://127.0.0.1:19010").unwrap()
        );
    }

    #[test]
    fn test_stash_merge_happens_before_complete_login_invariant() {
        let mut stash = DiscoveredRelayLists {
            nip65: Some(vec![dummy_relay("ws://127.0.0.1:19010")]),
            inbox: None,
            key_package: None,
        };

        stash.merge(DiscoveredRelayLists {
            nip65: None,
            inbox: Some(vec![dummy_relay("ws://127.0.0.1:19011")]),
            key_package: Some(vec![dummy_relay("ws://127.0.0.1:19012")]),
        });

        assert!(
            stash.is_complete(),
            "Stash must be complete after upfront merge, before complete_login is called"
        );
        assert_eq!(
            stash.nip65.as_ref().unwrap()[0].url,
            RelayUrl::parse("ws://127.0.0.1:19010").unwrap()
        );
        assert_eq!(
            stash.inbox.as_ref().unwrap()[0].url,
            RelayUrl::parse("ws://127.0.0.1:19011").unwrap()
        );
        assert_eq!(
            stash.key_package.as_ref().unwrap()[0].url,
            RelayUrl::parse("ws://127.0.0.1:19012").unwrap()
        );
    }

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
        let cloned = LoginStatus::NeedsRelayLists.clone();
        assert_eq!(LoginStatus::NeedsRelayLists, cloned);
    }

    #[test]
    fn test_login_status_debug() {
        assert!(format!("{:?}", LoginStatus::Complete).contains("Complete"));
        assert!(format!("{:?}", LoginStatus::NeedsRelayLists).contains("NeedsRelayLists"));
    }

    #[test]
    fn test_login_error_from_key_error() {
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

    #[test]
    fn test_login_error_debug() {
        let debug = format!("{:?}", LoginError::NoRelayConnections);
        assert!(debug.contains("NoRelayConnections"));
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

    #[test]
    fn test_login_result_debug() {
        let account = Account {
            id: Some(1),
            pubkey: Keys::generate().public_key(),
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
        let account = Account {
            id: Some(1),
            pubkey: Keys::generate().public_key(),
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

    // -----------------------------------------------------------------------
    // login_start tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_login_start_already_logged_in_returns_existing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = create_test_keys();
        let original = whitenoise
            .create_test_identity_with_keys(&keys)
            .await
            .unwrap();
        let original_synced_at = Account::find_by_pubkey(&original.pubkey, &whitenoise.database)
            .await
            .unwrap()
            .last_synced_at;
        assert!(
            original_synced_at.is_some(),
            "create_identity must set last_synced_at"
        );

        let result = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .unwrap();

        assert_eq!(
            result.status,
            LoginStatus::Complete,
            "Already-logged-in account should return Complete"
        );
        assert_eq!(
            result.account.pubkey, original.pubkey,
            "Should return the same account"
        );
        assert_eq!(
            result.account.id, original.id,
            "Should return the same account row"
        );

        let after = Account::find_by_pubkey(&original.pubkey, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(
            after.last_synced_at, original_synced_at,
            "login_start must not reset last_synced_at"
        );

        assert!(
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&original.pubkey),
            "Should not create a pending login for an already-active account"
        );
    }

    #[tokio::test]
    async fn test_login_start_db_row_without_active_session_does_not_short_circuit() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = create_test_keys();
        let pubkey = keys.public_key();

        whitenoise
            .create_base_account_with_private_key(&keys)
            .await
            .unwrap();

        assert!(
            Account::find_by_pubkey(&pubkey, &whitenoise.database)
                .await
                .is_ok()
        );
        assert!(
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey)
        );
        assert!(
            !whitenoise
                .background_task_cancellation
                .contains_key(&pubkey)
        );

        let result = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .unwrap();

        assert_eq!(
            result.status,
            LoginStatus::NeedsRelayLists,
            "DB row without active session must not short-circuit to Complete"
        );
        assert!(
            whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey),
            "Relay discovery path must stash a pending login"
        );

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    #[tokio::test]
    async fn test_login_start_partial_login_does_not_short_circuit() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let result = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::NeedsRelayLists);
        assert!(
            whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey)
        );

        let result2 = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .unwrap();
        assert_eq!(
            result2.status,
            LoginStatus::NeedsRelayLists,
            "Partial login must retry discovery, not return Complete"
        );

        let _ = whitenoise.login_cancel(&pubkey).await;
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
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let result = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await;

        match result {
            Ok(login_result) => {
                assert_eq!(login_result.status, LoginStatus::NeedsRelayLists);
                assert_eq!(login_result.account.pubkey, pubkey);
                assert!(
                    whitenoise
                        .account_manager
                        .pending_logins
                        .contains_key(&pubkey)
                );
                let _ = whitenoise.login_cancel(&pubkey).await;
            }
            Err(e) => {
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
        assert!(
            whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey)
        );

        // Now publish default relays to complete the login.
        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(result.account.pubkey, pubkey);
        assert!(
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey)
        );

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
    async fn test_login_start_happy_path_with_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        publish_relay_lists_to_dev_relays(&keys).await;

        let result = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(result.account.pubkey, pubkey);

        let nip65 = result.account.nip65_relays(&whitenoise).await.unwrap();
        assert!(
            !nip65.is_empty(),
            "Expected NIP-65 relays to be stored after login"
        );
    }

    #[tokio::test]
    async fn test_login_start_duplicate_call_overwrites_stash() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        whitenoise
            .create_base_account_with_private_key(&keys)
            .await
            .unwrap();
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://first-run.example.com").unwrap())
            .await
            .unwrap();
        whitenoise.account_manager.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: Some(vec![nip65_relay.clone()]),
                inbox: None,
                key_package: None,
            },
        );

        whitenoise.account_manager.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
            },
        );

        let stash = whitenoise
            .account_manager
            .pending_logins
            .get(&pubkey)
            .unwrap();
        assert!(
            stash.nip65.is_none(),
            "Second insert must overwrite the first stash"
        );
        drop(stash);

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    // -----------------------------------------------------------------------
    // login_cancel tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_login_cancel_no_account_is_ok() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();
        let result = whitenoise.login_cancel(&pubkey).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_login_cancel_with_pending_login() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        whitenoise
            .create_base_account_with_private_key(&keys)
            .await
            .unwrap();
        whitenoise.account_manager.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
            },
        );

        assert!(
            whitenoise.find_account_by_pubkey(&pubkey).await.is_ok(),
            "Account should exist before cancel"
        );

        let result = whitenoise.login_cancel(&pubkey).await;
        assert!(result.is_ok());

        assert!(
            whitenoise.find_account_by_pubkey(&pubkey).await.is_err(),
            "Account should be deleted after cancel"
        );
        assert!(
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey),
            "Pending login should be removed after cancel"
        );
    }

    #[tokio::test]
    async fn test_login_cancel_pending_but_no_account_in_db() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        whitenoise.account_manager.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
            },
        );

        let result = whitenoise.login_cancel(&pubkey).await;
        assert!(result.is_ok());
        assert!(
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey)
        );
    }

    #[tokio::test]
    async fn test_login_cancel_does_not_delete_non_pending_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        whitenoise
            .create_base_account_with_private_key(&keys)
            .await
            .unwrap();

        let result = whitenoise.login_cancel(&pubkey).await;
        assert!(result.is_ok());

        assert!(
            whitenoise.find_account_by_pubkey(&pubkey).await.is_ok(),
            "Non-pending account should not be deleted by login_cancel"
        );
    }

    #[tokio::test]
    async fn test_login_cancel_is_idempotent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        whitenoise
            .create_base_account_with_private_key(&keys)
            .await
            .unwrap();
        whitenoise.account_manager.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
            },
        );

        whitenoise.login_cancel(&pubkey).await.unwrap();
        whitenoise.login_cancel(&pubkey).await.unwrap();
    }

    #[tokio::test]
    async fn test_login_cancel_with_partial_nip65_stash() {
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
        whitenoise.account_manager.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: Some(vec![nip65_relay]),
                inbox: None,
                key_package: None,
            },
        );

        whitenoise.login_cancel(&pubkey).await.unwrap();
        assert!(
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey)
        );
        assert!(whitenoise.find_account_by_pubkey(&pubkey).await.is_err());
    }

    #[tokio::test]
    async fn test_login_cancel_clears_relay_associations_written_by_try_discover() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://cancel-test.example.com").unwrap())
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: Some(vec![nip65_relay.clone()]),
                inbox: None,
                key_package: None,
            },
        )
        .await;

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

        whitenoise.login_cancel(&pubkey).await.unwrap();

        assert!(whitenoise.find_account_by_pubkey(&pubkey).await.is_err());
        assert!(
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey)
        );

        let nip65_after = user
            .relays(RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        assert!(
            nip65_after.is_empty(),
            "NIP-65 relay associations must be removed when login is cancelled (found {} rows)",
            nip65_after.len()
        );
    }

    // -----------------------------------------------------------------------
    // login_publish_default_relays tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_login_publish_default_relays_without_start_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();
        let result = whitenoise.login_publish_default_relays(&pubkey).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), LoginError::NoLoginInProgress));
    }

    #[tokio::test]
    async fn test_publish_default_relays_all_missing_assigns_all_three() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        setup_partial_pending_login(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
            },
        )
        .await;

        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

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
    async fn test_publish_default_relays_removes_pending_entry() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        setup_partial_pending_login(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
            },
        )
        .await;

        whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert!(
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey),
            "pending_logins must be cleared after successful publish"
        );
    }

    #[tokio::test]
    async fn test_publish_default_relays_without_pending_returns_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();
        let err = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap_err();
        assert!(
            matches!(err, LoginError::NoLoginInProgress),
            "Must return NoLoginInProgress when no stash exists"
        );
    }

    #[tokio::test]
    async fn test_pending_logins_cleared_on_complete() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        setup_partial_pending_login(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
            },
        )
        .await;

        let complete = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(complete.status, LoginStatus::Complete);
        assert!(
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey),
            "Stash must be removed after login_publish_default_relays"
        );
    }

    #[tokio::test]
    async fn test_publish_default_relays_nip65_present_inbox_and_kp_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_url = RelayUrl::parse("ws://localhost:8080").unwrap();
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&nip65_url)
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: Some(vec![nip65_relay.clone()]),
                inbox: None,
                key_package: None,
            },
        )
        .await;

        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

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

        assert!(
            !result
                .account
                .inbox_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "Inbox must get defaults"
        );
        assert!(
            !result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "KeyPackage must get defaults"
        );
    }

    #[tokio::test]
    async fn test_publish_default_relays_inbox_present_nip65_and_kp_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let inbox_url = RelayUrl::parse("ws://localhost:8080").unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&inbox_url)
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: None,
                inbox: Some(vec![inbox_relay]),
                key_package: None,
            },
        )
        .await;

        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

        let stored_inbox = result.account.inbox_relays(&whitenoise).await.unwrap();
        assert_eq!(stored_inbox.len(), 1);
        assert_eq!(
            stored_inbox[0].url, inbox_url,
            "Inbox must not be overwritten"
        );
        assert!(
            !result
                .account
                .nip65_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "NIP-65 must get defaults"
        );
        assert!(
            !result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "KeyPackage must get defaults"
        );
    }

    #[tokio::test]
    async fn test_publish_default_relays_key_package_present_nip65_and_inbox_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let kp_url = RelayUrl::parse("ws://localhost:7777").unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&kp_url)
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: Some(vec![kp_relay]),
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
        assert!(
            !result
                .account
                .nip65_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !result
                .account
                .inbox_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_publish_default_relays_nip65_and_inbox_present_kp_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_url = RelayUrl::parse("ws://localhost:8080").unwrap();
        let inbox_url = RelayUrl::parse("ws://localhost:8080").unwrap();
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
                nip65: Some(vec![nip65_relay]),
                inbox: Some(vec![inbox_relay]),
                key_package: None,
            },
        )
        .await;

        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(
            result.account.nip65_relays(&whitenoise).await.unwrap()[0].url,
            nip65_url,
            "NIP-65 must be preserved"
        );
        assert_eq!(
            result.account.inbox_relays(&whitenoise).await.unwrap()[0].url,
            inbox_url,
            "Inbox must be preserved"
        );
        assert!(
            !result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "KeyPackage must get defaults"
        );
    }

    #[tokio::test]
    async fn test_publish_default_relays_nip65_and_kp_present_inbox_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_url = RelayUrl::parse("ws://localhost:8080").unwrap();
        let kp_url = RelayUrl::parse("ws://localhost:7777").unwrap();
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
                nip65: Some(vec![nip65_relay]),
                inbox: None,
                key_package: Some(vec![kp_relay]),
            },
        )
        .await;

        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(
            result.account.nip65_relays(&whitenoise).await.unwrap()[0].url,
            nip65_url,
            "NIP-65 must be preserved"
        );
        assert_eq!(
            result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()[0]
                .url,
            kp_url,
            "KeyPackage must be preserved"
        );
        assert!(
            !result
                .account
                .inbox_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "Inbox must get defaults"
        );
    }

    #[tokio::test]
    async fn test_publish_default_relays_inbox_and_kp_present_nip65_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let inbox_url = RelayUrl::parse("ws://localhost:8080").unwrap();
        let kp_url = RelayUrl::parse("ws://localhost:7777").unwrap();
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
                nip65: None,
                inbox: Some(vec![inbox_relay]),
                key_package: Some(vec![kp_relay]),
            },
        )
        .await;

        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);
        assert!(
            !result
                .account
                .nip65_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "NIP-65 must get defaults"
        );
        assert_eq!(
            result.account.inbox_relays(&whitenoise).await.unwrap()[0].url,
            inbox_url,
            "Inbox must be preserved"
        );
        assert_eq!(
            result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()[0]
                .url,
            kp_url,
            "KeyPackage must be preserved"
        );
    }

    // -----------------------------------------------------------------------
    // login_with_custom_relay tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_login_with_custom_relay_without_start_fails() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();
        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let result = whitenoise.login_with_custom_relay(&pubkey, relay_url).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), LoginError::NoLoginInProgress));
    }

    #[tokio::test]
    async fn test_login_with_custom_relay_no_pending_returns_error() {
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
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://127.0.0.1:19010").unwrap())
            .await
            .unwrap();
        setup_partial_pending_login(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: Some(vec![nip65_relay.clone()]),
                inbox: None,
                key_package: None,
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
        let stash = whitenoise
            .account_manager
            .pending_logins
            .get(&pubkey)
            .unwrap();
        assert_eq!(
            stash.nip65.as_ref().map_or(0, |v| v.len()),
            1,
            "nip65 relay must be preserved in stash"
        );
        assert!(stash.inbox.is_none());
        assert!(stash.key_package.is_none());
        drop(stash);

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    #[tokio::test]
    async fn test_login_with_custom_relay_completes_when_stash_already_complete() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://localhost:8080").unwrap())
            .await
            .unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://localhost:8080").unwrap())
            .await
            .unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://localhost:7777").unwrap())
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: Some(vec![nip65_relay.clone()]),
                inbox: Some(vec![inbox_relay.clone()]),
                key_package: Some(vec![kp_relay.clone()]),
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
            "Stash already complete → must complete login"
        );
        assert!(
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey),
            "pending_logins must be cleared after Complete"
        );
        assert_eq!(result.account.pubkey, pubkey);
    }

    #[tokio::test]
    async fn test_login_with_custom_relay_all_missing_stays_incomplete() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        setup_partial_pending_login(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
            },
        )
        .await;

        let result = whitenoise
            .login_with_custom_relay(&pubkey, RelayUrl::parse("ws://localhost:8080").unwrap())
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::NeedsRelayLists);
        assert!(
            whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey),
            "Stash must persist when still incomplete"
        );

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    #[tokio::test]
    async fn test_login_with_custom_relay_finds_lists() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        publish_relay_lists_to_dev_relays(&keys).await;

        let start = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .unwrap();

        if start.status == LoginStatus::Complete {
            return;
        }

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

        let start = whitenoise
            .login_start(keys.secret_key().to_secret_hex())
            .await
            .unwrap();
        assert_eq!(start.status, LoginStatus::NeedsRelayLists);

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

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    #[tokio::test]
    async fn test_login_with_custom_relay_completes_via_merge_from_partial_stash() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://localhost:8080").unwrap())
            .await
            .unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://localhost:8080").unwrap())
            .await
            .unwrap();
        let kp_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://localhost:7777").unwrap())
            .await
            .unwrap();

        setup_pending_login_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: Some(vec![nip65_relay.clone()]),
                inbox: Some(vec![inbox_relay.clone()]),
                key_package: Some(vec![kp_relay.clone()]),
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
            "login_with_custom_relay must return Complete when merged stash is complete"
        );
        assert!(
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey),
            "pending_logins must be cleared after Complete"
        );
        assert_eq!(result.account.pubkey, pubkey);

        assert_eq!(
            result.account.nip65_relays(&whitenoise).await.unwrap()[0].url,
            nip65_relay.url
        );
        assert_eq!(
            result.account.inbox_relays(&whitenoise).await.unwrap()[0].url,
            inbox_relay.url
        );
        assert_eq!(
            result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()[0]
                .url,
            kp_relay.url
        );
    }

    #[tokio::test]
    async fn test_stash_updated_before_completion_local_key() {
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

        setup_pending_login_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: Some(vec![nip65_relay.clone()]),
                inbox: None,
                key_package: None,
            },
        )
        .await;

        {
            let mut stash = whitenoise
                .account_manager
                .pending_logins
                .get_mut(&pubkey)
                .unwrap();
            stash.merge(DiscoveredRelayLists {
                nip65: None,
                inbox: Some(vec![inbox_relay.clone()]),
                key_package: Some(vec![kp_relay.clone()]),
            });
            assert!(stash.is_complete(), "Stash must be complete after merge");
        }

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

        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

        assert_eq!(
            result.account.nip65_relays(&whitenoise).await.unwrap()[0].url,
            RelayUrl::parse("ws://localhost:7777").unwrap()
        );
        assert_eq!(
            result.account.inbox_relays(&whitenoise).await.unwrap()[0].url,
            RelayUrl::parse("ws://localhost:7777").unwrap()
        );
        assert_eq!(
            result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()[0]
                .url,
            RelayUrl::parse("ws://localhost:7777").unwrap()
        );
    }

    // -----------------------------------------------------------------------
    // login_external_signer_start tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_login_external_signer_start_no_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let result = whitenoise
            .login_external_signer_start(pubkey, keys.clone())
            .await
            .unwrap();

        assert_eq!(result.status, LoginStatus::NeedsRelayLists);
        assert_eq!(result.account.pubkey, pubkey);
        assert_eq!(result.account.account_type, AccountType::External);
        assert!(
            whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey)
        );

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    #[tokio::test]
    async fn test_login_external_signer_start_happy_path() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

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
    // login_external_signer_publish_default_relays tests
    // -----------------------------------------------------------------------

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
    async fn test_login_external_signer_publish_defaults_no_signer() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        whitenoise.account_manager.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
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
                    "Expected 'External signer not found', got: {}",
                    msg
                );
            }
            other => panic!("Expected LoginError::Internal, got: {:?}", other),
        }

        whitenoise.account_manager.pending_logins.remove(&pubkey);
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_all_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        setup_partial_pending_login_external_signer(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
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
    async fn test_external_signer_publish_default_relays_removes_pending_entry() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        setup_partial_pending_login_external_signer(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
            },
        )
        .await;

        whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert!(
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey),
            "pending_logins must be cleared after external signer publish"
        );
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
    async fn test_external_signer_publish_default_relays_no_signer_registered() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        whitenoise.account_manager.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
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

        whitenoise.account_manager.pending_logins.remove(&pubkey);
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_preserves_found_nip65() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_url = RelayUrl::parse("ws://localhost:8080").unwrap();
        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&nip65_url)
            .await
            .unwrap();

        setup_pending_login_external_signer_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: Some(vec![nip65_relay]),
                inbox: None,
                key_package: None,
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

        assert_eq!(
            result.account.nip65_relays(&whitenoise).await.unwrap()[0].url,
            nip65_url,
            "NIP-65 must not be overwritten"
        );
        assert!(
            !result
                .account
                .inbox_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "Inbox must receive defaults"
        );
        assert!(
            !result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "KeyPackage must receive defaults"
        );
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_inbox_only_present() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let inbox_url = RelayUrl::parse("ws://localhost:8080").unwrap();
        let inbox_relay = whitenoise
            .find_or_create_relay_by_url(&inbox_url)
            .await
            .unwrap();

        setup_pending_login_external_signer_with_db_relays(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: None,
                inbox: Some(vec![inbox_relay]),
                key_package: None,
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(result.account.account_type, AccountType::External);

        assert_eq!(
            result.account.inbox_relays(&whitenoise).await.unwrap()[0].url,
            inbox_url,
            "Inbox must not be overwritten"
        );
        assert!(
            !result
                .account
                .nip65_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "NIP-65 must receive defaults"
        );
        assert!(
            !result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "KeyPackage must receive defaults"
        );
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_key_package_only_present() {
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
                nip65: None,
                inbox: None,
                key_package: Some(vec![kp_relay]),
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

        assert_eq!(
            result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()[0]
                .url,
            kp_url,
            "KeyPackage must not be overwritten"
        );
        assert!(
            !result
                .account
                .nip65_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "NIP-65 must receive defaults"
        );
        assert!(
            !result
                .account
                .inbox_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "Inbox must receive defaults"
        );
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_nip65_and_inbox_present_kp_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_url = RelayUrl::parse("ws://localhost:8080").unwrap();
        let inbox_url = RelayUrl::parse("ws://localhost:8080").unwrap();
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
                nip65: Some(vec![nip65_relay]),
                inbox: Some(vec![inbox_relay]),
                key_package: None,
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(
            result.account.nip65_relays(&whitenoise).await.unwrap()[0].url,
            nip65_url,
            "NIP-65 must be preserved"
        );
        assert_eq!(
            result.account.inbox_relays(&whitenoise).await.unwrap()[0].url,
            inbox_url,
            "Inbox must be preserved"
        );
        assert!(
            !result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "KeyPackage must receive defaults"
        );
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_nip65_and_kp_present_inbox_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_url = RelayUrl::parse("ws://localhost:8080").unwrap();
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
                nip65: Some(vec![nip65_relay]),
                inbox: None,
                key_package: Some(vec![kp_relay]),
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);
        assert_eq!(
            result.account.nip65_relays(&whitenoise).await.unwrap()[0].url,
            nip65_url,
            "NIP-65 must be preserved"
        );
        assert_eq!(
            result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()[0]
                .url,
            kp_url,
            "KeyPackage must be preserved"
        );
        assert!(
            !result
                .account
                .inbox_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "Inbox must receive defaults"
        );
    }

    #[tokio::test]
    async fn test_external_signer_publish_default_relays_inbox_and_kp_present_nip65_missing() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let inbox_url = RelayUrl::parse("ws://localhost:8080").unwrap();
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
                nip65: None,
                inbox: Some(vec![inbox_relay]),
                key_package: Some(vec![kp_relay]),
            },
        )
        .await;

        let result = whitenoise
            .login_external_signer_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);
        assert!(
            !result
                .account
                .nip65_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "NIP-65 must receive defaults"
        );
        assert_eq!(
            result.account.inbox_relays(&whitenoise).await.unwrap()[0].url,
            inbox_url,
            "Inbox must be preserved"
        );
        assert_eq!(
            result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()[0]
                .url,
            kp_url,
            "KeyPackage must be preserved"
        );
    }

    // -----------------------------------------------------------------------
    // login_external_signer_with_custom_relay tests
    // -----------------------------------------------------------------------

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
    async fn test_login_external_signer_custom_relay_no_signer() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        whitenoise.account_manager.pending_logins.insert(
            pubkey,
            DiscoveredRelayLists {
                nip65: None,
                inbox: None,
                key_package: None,
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
                    "Expected 'External signer not found', got: {}",
                    msg
                );
            }
            other => panic!("Expected LoginError::Internal, got: {:?}", other),
        }

        whitenoise.account_manager.pending_logins.remove(&pubkey);
    }

    #[tokio::test]
    async fn test_login_ext_signer_with_custom_relay_stays_incomplete() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("ws://127.0.0.1:19010").unwrap())
            .await
            .unwrap();
        setup_partial_pending_login_external_signer(
            &whitenoise,
            &keys,
            DiscoveredRelayLists {
                nip65: Some(vec![nip65_relay]),
                inbox: None,
                key_package: None,
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
        let stash = whitenoise
            .account_manager
            .pending_logins
            .get(&pubkey)
            .unwrap();
        assert_eq!(
            stash.nip65.as_ref().map_or(0, |v| v.len()),
            1,
            "nip65 must be preserved"
        );
        assert!(stash.inbox.is_none());
        assert!(stash.key_package.is_none());
        drop(stash);

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    #[tokio::test]
    async fn test_login_ext_signer_with_custom_relay_completes_when_stash_full() {
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
                nip65: Some(vec![nip65_relay]),
                inbox: Some(vec![inbox_relay]),
                key_package: Some(vec![kp_relay]),
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
            !whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey),
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
                nip65: None,
                inbox: None,
                key_package: None,
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
        assert!(
            whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey)
        );

        let _ = whitenoise.login_cancel(&pubkey).await;
    }

    // -----------------------------------------------------------------------
    // Regression test — the original bug
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_regression_nip65_only_user_is_blocked_from_login() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let nip65_relay = whitenoise
            .find_or_create_relay_by_url(&RelayUrl::parse("wss://relay.damus.io").unwrap())
            .await
            .unwrap();

        let partial = DiscoveredRelayLists {
            nip65: Some(vec![nip65_relay]),
            inbox: None,
            key_package: None,
        };

        assert!(
            !partial.is_complete(),
            "A user with only 10002 must NOT pass is_complete()"
        );

        whitenoise
            .create_base_account_with_private_key(&keys)
            .await
            .unwrap();
        whitenoise
            .account_manager
            .pending_logins
            .insert(pubkey, partial);

        assert!(
            whitenoise
                .account_manager
                .pending_logins
                .contains_key(&pubkey),
            "User with only 10002 must be held in pending state"
        );

        let result = whitenoise
            .login_publish_default_relays(&pubkey)
            .await
            .unwrap();
        assert_eq!(result.status, LoginStatus::Complete);

        assert!(
            !result
                .account
                .inbox_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "Inbox must be filled after publish_default_relays"
        );
        assert!(
            !result
                .account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty(),
            "KeyPackage must be filled after publish_default_relays"
        );
    }
}
