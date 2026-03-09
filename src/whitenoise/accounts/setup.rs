use std::collections::HashSet;

use mdk_core::prelude::group_types::GroupState;
use nostr_sdk::prelude::*;
use tokio::sync::watch;

use crate::RelayType;
use crate::relay_control::groups::GroupSubscriptionSpec;
use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;
use crate::whitenoise::error::Result;
use crate::whitenoise::relays::Relay;
use crate::whitenoise::users::User;
use crate::whitenoise::{Whitenoise, WhitenoiseError};

use super::{Account, ExternalSignerRelaySetup};

impl Whitenoise {
    pub(super) async fn create_base_account_with_private_key(
        &self,
        keys: &Keys,
    ) -> Result<Account> {
        let (account, _keys) = Account::new(self, Some(keys.clone())).await?;

        self.secrets_store.store_private_key(keys).map_err(|e| {
            tracing::error!(target: "whitenoise::accounts", "Failed to store private key: {}", e);
            e
        })?;

        let account = self.persist_account(&account).await?;

        Ok(account)
    }

    pub(super) async fn activate_account(
        &self,
        account: &Account,
        user: &User,
        is_new_account: bool,
        inbox_relays: &[Relay],
        key_package_relays: &[Relay],
    ) -> Result<()> {
        // Create a fresh cancellation channel for this account's background tasks.
        // Any previous channel (from a prior login) is replaced, which also drops
        // any stale receivers.
        let (cancel_tx, _) = tokio::sync::watch::channel(false);
        self.background_task_cancellation
            .insert(account.pubkey, cancel_tx);

        if let Err(e) = self.refresh_global_subscription_for_user().await {
            tracing::warn!(
                target: "whitenoise::accounts",
                "Failed to refresh global subscription for new user {}: {}",
                user.pubkey,
                e
            );
        }
        tracing::debug!(target: "whitenoise::accounts", "Global subscription refreshed for account user");
        self.setup_subscriptions(account, inbox_relays).await?;
        tracing::debug!(target: "whitenoise::accounts", "Subscriptions setup");
        self.setup_key_package(account, is_new_account, key_package_relays)
            .await?;
        tracing::debug!(target: "whitenoise::accounts", "Key package setup");
        Ok(())
    }

    /// Activates an account without publishing anything (for external signer accounts).
    /// This sets up relay connections and subscriptions but skips key package publishing.
    pub(super) async fn activate_account_without_publishing(
        &self,
        account: &Account,
        user: &User,
        inbox_relays: &[Relay],
    ) -> Result<()> {
        let (cancel_tx, _) = tokio::sync::watch::channel(false);
        self.background_task_cancellation
            .insert(account.pubkey, cancel_tx);

        if let Err(e) = self.refresh_global_subscription_for_user().await {
            tracing::warn!(
                target: "whitenoise::accounts",
                "Failed to refresh global subscription for new user {}: {}",
                user.pubkey,
                e
            );
        }
        tracing::debug!(target: "whitenoise::accounts", "Global subscription refreshed");

        self.setup_subscriptions(account, inbox_relays).await?;
        tracing::debug!(target: "whitenoise::accounts", "Subscriptions setup");

        // Note: We skip key package setup for external signer accounts.
        // Key packages need to be published separately with the external signer.
        tracing::debug!(target: "whitenoise::accounts", "Skipping key package setup (external signer)");

        Ok(())
    }

    pub(super) async fn persist_account(&self, account: &Account) -> Result<Account> {
        let saved_account = account.save(&self.database).await.map_err(|e| {
            tracing::error!(target: "whitenoise::accounts", "Failed to save account: {}", e);
            // Try to clean up stored private key
            if let Err(cleanup_err) = self.secrets_store.remove_private_key_for_pubkey(&account.pubkey) {
                tracing::error!(target: "whitenoise::accounts", "Failed to cleanup private key after account save failure: {}", cleanup_err);
            }
            e
        })?;
        tracing::debug!(target: "whitenoise::accounts", "Account saved to database");
        Ok(saved_account)
    }

    async fn setup_key_package(
        &self,
        account: &Account,
        is_new_account: bool,
        key_package_relays: &[Relay],
    ) -> Result<()> {
        let mut needs_publish = true;

        if !is_new_account {
            tracing::debug!(target: "whitenoise::accounts", "Found {} key package relays", key_package_relays.len());
            let relays_urls = Relay::urls(key_package_relays);

            if let Some(event) = self
                .relay_control
                .fetch_user_key_package(account.pubkey, &relays_urls)
                .await?
            {
                if self.is_own_key_package(&account.pubkey, &event.id).await {
                    tracing::debug!(
                        target: "whitenoise::accounts",
                        "Found existing key package with live key material, skipping publish"
                    );
                    needs_publish = false;
                } else {
                    tracing::debug!(
                        target: "whitenoise::accounts",
                        "Key package on relay was not published by this client or key material is gone, publishing new one"
                    );
                }
            }
        }

        if needs_publish {
            match self
                .create_and_publish_key_package(account, key_package_relays)
                .await
            {
                Ok(()) => {
                    tracing::debug!(target: "whitenoise::accounts", "Published key package");
                }
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::accounts",
                        "Initial key package publish failed, scheduler will retry: {}",
                        e
                    );
                }
            }
        }

        Ok(())
    }

    /// Checks whether a key package event was published by this Whitenoise instance
    /// and still has live local key material.
    async fn is_own_key_package(&self, pubkey: &PublicKey, event_id: &EventId) -> bool {
        match PublishedKeyPackage::find_by_event_id(pubkey, &event_id.to_hex(), &self.database)
            .await
        {
            Ok(Some(pkg)) => !pkg.key_material_deleted,
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::accounts",
                    "Failed to look up published key package for event {}: {}",
                    event_id,
                    e
                );
                false
            }
        }
    }

    pub(super) async fn load_default_relays(&self) -> Result<Vec<Relay>> {
        let mut default_relays = Vec::new();
        for Relay { url, .. } in Relay::defaults() {
            let relay = self.find_or_create_relay_by_url(&url).await?;
            default_relays.push(relay);
        }
        Ok(default_relays)
    }

    /// Sets up the relays for a new account.
    ///
    /// # Arguments
    ///
    /// * `account` - The account to setup the relays for
    /// * `user` - The user to setup the relays for
    ///
    /// # Returns
    /// Returns the default relays for the account.
    pub(super) async fn setup_relays_for_new_account(
        &self,
        account: &mut Account,
        user: &User,
    ) -> Result<Vec<Relay>> {
        let default_relays = self.load_default_relays().await?;

        user.add_relays(&default_relays, RelayType::Nip65, &self.database)
            .await?;
        user.add_relays(&default_relays, RelayType::Inbox, &self.database)
            .await?;
        user.add_relays(&default_relays, RelayType::KeyPackage, &self.database)
            .await?;

        self.background_publish_account_relay_list(
            account,
            RelayType::Nip65,
            Some(&default_relays),
        )
        .await?;
        self.background_publish_account_relay_list(
            account,
            RelayType::Inbox,
            Some(&default_relays),
        )
        .await?;
        self.background_publish_account_relay_list(
            account,
            RelayType::KeyPackage,
            Some(&default_relays),
        )
        .await?;

        Ok(default_relays)
    }

    pub(super) async fn setup_relays_for_existing_account(
        &self,
        account: &mut Account,
    ) -> Result<(Vec<Relay>, Vec<Relay>, Vec<Relay>)> {
        let default_relays = self.load_default_relays().await?;
        let signer = self.get_signer_for_account(account)?;

        // NIP-65 must be fetched first because Inbox and KeyPackage use the
        // discovered NIP-65 relays as their query source.
        let (nip65_relays, should_publish_nip65) = self
            .setup_existing_account_relay_type(
                account,
                RelayType::Nip65,
                &default_relays,
                &default_relays,
            )
            .await?;

        // Inbox and KeyPackage fetches are independent — run them concurrently.
        let (inbox_result, key_package_result) = tokio::join!(
            self.setup_existing_account_relay_type(
                account,
                RelayType::Inbox,
                &nip65_relays,
                &default_relays,
            ),
            self.setup_existing_account_relay_type(
                account,
                RelayType::KeyPackage,
                &nip65_relays,
                &default_relays,
            ),
        );
        let (inbox_relays, should_publish_inbox) = inbox_result?;
        let (key_package_relays, should_publish_key_package) = key_package_result?;

        // Publish any relay lists that need publishing concurrently.
        let nip65_publish = async {
            if should_publish_nip65 {
                self.publish_relay_list(
                    &nip65_relays,
                    RelayType::Nip65,
                    &nip65_relays,
                    signer.clone(),
                )
                .await
            } else {
                Ok(())
            }
        };
        let inbox_publish = async {
            if should_publish_inbox {
                self.publish_relay_list(
                    &inbox_relays,
                    RelayType::Inbox,
                    &nip65_relays,
                    signer.clone(),
                )
                .await
            } else {
                Ok(())
            }
        };
        let key_package_publish = async {
            if should_publish_key_package {
                self.publish_relay_list(
                    &key_package_relays,
                    RelayType::KeyPackage,
                    &nip65_relays,
                    signer.clone(),
                )
                .await
            } else {
                Ok(())
            }
        };
        let (r1, r2, r3) = tokio::join!(nip65_publish, inbox_publish, key_package_publish);
        r1?;
        r2?;
        r3?;

        Ok((nip65_relays, inbox_relays, key_package_relays))
    }

    /// Sets up relays for an external signer account.
    ///
    /// This is similar to `setup_relays_for_existing_account` but does NOT publish
    /// relay lists (the caller with the signer should handle that).
    /// It only fetches existing relays or uses defaults and saves them locally.
    ///
    /// Returns the relays for each type and booleans indicating which relay lists
    /// should be published (true when defaults were used).
    pub(super) async fn setup_relays_for_external_signer_account(
        &self,
        account: &mut Account,
    ) -> Result<ExternalSignerRelaySetup> {
        let default_relays = self.load_default_relays().await?;

        // NIP-65 must be fetched first because Inbox and KeyPackage use the
        // discovered NIP-65 relays as their query source.
        let (nip65_relays, should_publish_nip65) = self
            .setup_external_account_relay_type(
                account,
                RelayType::Nip65,
                &default_relays,
                &default_relays,
            )
            .await?;

        // Inbox and KeyPackage fetches are independent — run them concurrently.
        let (inbox_result, key_package_result) = tokio::join!(
            self.setup_external_account_relay_type(
                account,
                RelayType::Inbox,
                &nip65_relays,
                &default_relays,
            ),
            self.setup_external_account_relay_type(
                account,
                RelayType::KeyPackage,
                &nip65_relays,
                &default_relays,
            ),
        );
        let (inbox_relays, should_publish_inbox) = inbox_result?;
        let (key_package_relays, should_publish_key_package) = key_package_result?;

        Ok(ExternalSignerRelaySetup {
            nip65_relays,
            inbox_relays,
            key_package_relays,
            should_publish_nip65,
            should_publish_inbox,
            should_publish_key_package,
        })
    }

    /// Sets up a specific relay type for an external signer account.
    /// Fetches from network or uses defaults, but never publishes.
    ///
    /// Returns the relays and a boolean indicating whether they should be published
    /// (true if defaults were used because no existing relay list was found on the network).
    async fn setup_external_account_relay_type(
        &self,
        account: &Account,
        relay_type: RelayType,
        source_relays: &[Relay],
        default_relays: &[Relay],
    ) -> Result<(Vec<Relay>, bool)> {
        // Try to fetch existing relay lists first
        let fetched_relays = self
            .fetch_existing_relays(account.pubkey, relay_type, source_relays)
            .await?;

        match fetched_relays {
            None => {
                // No existing relay list found on network — use defaults, mark for publishing.
                self.sync_account_relays(account, default_relays, relay_type)
                    .await?;
                Ok((default_relays.to_vec(), true))
            }
            Some(relays) => {
                // Found an existing relay list — use it, no publishing needed.
                self.sync_account_relays(account, &relays, relay_type)
                    .await?;
                Ok((relays, false))
            }
        }
    }

    async fn setup_existing_account_relay_type(
        &self,
        account: &Account,
        relay_type: RelayType,
        source_relays: &[Relay],
        default_relays: &[Relay],
    ) -> Result<(Vec<Relay>, bool)> {
        // Existing accounts: try to fetch existing relay lists first
        let fetched_relays = self
            .fetch_existing_relays(account.pubkey, relay_type, source_relays)
            .await?;

        match fetched_relays {
            None => {
                // No existing relay list found on network — use defaults and publish.
                self.add_relays_to_account(account, default_relays, relay_type)
                    .await?;
                Ok((default_relays.to_vec(), true))
            }
            Some(relays) => {
                // Found an existing relay list — use it, no publishing needed.
                self.sync_account_relays(account, &relays, relay_type)
                    .await?;
                Ok((relays, false))
            }
        }
    }

    pub(super) async fn fetch_existing_relays(
        &self,
        pubkey: PublicKey,
        relay_type: RelayType,
        source_relays: &[Relay],
    ) -> Result<Option<Vec<Relay>>> {
        let source_relay_urls = Relay::urls(source_relays);
        if source_relay_urls.is_empty() {
            return Ok(None);
        }

        // Relay-control ephemeral sessions add/connect target relays internally,
        // so callers no longer need a separate pre-connection step here.
        let relay_event = self
            .relay_control
            .fetch_user_relays(pubkey, relay_type, &source_relay_urls)
            .await?;

        match relay_event {
            None => Ok(None),
            Some(event) => {
                let relay_urls = crate::nostr_manager::utils::relay_urls_from_event(&event);
                let mut relays = Vec::with_capacity(relay_urls.len());
                for url in relay_urls {
                    let relay = self.find_or_create_relay_by_url(&url).await?;
                    relays.push(relay);
                }
                Ok(Some(relays))
            }
        }
    }

    pub(super) async fn add_relays_to_account(
        &self,
        account: &Account,
        relays: &[Relay],
        relay_type: RelayType,
    ) -> Result<()> {
        if relays.is_empty() {
            return Ok(());
        }

        self.sync_account_relays(account, relays, relay_type)
            .await?;

        self.background_publish_account_relay_list(account, relay_type, Some(relays))
            .await?;

        tracing::debug!(
            target: "whitenoise::accounts",
            "Stored {} relays of type {:?} for account",
            relays.len(),
            relay_type
        );

        Ok(())
    }

    pub(super) async fn sync_account_relays(
        &self,
        account: &Account,
        relays: &[Relay],
        relay_type: RelayType,
    ) -> Result<()> {
        let user = account.user(&self.database).await?;
        let relay_urls = Relay::urls(relays).into_iter().collect::<HashSet<_>>();
        user.sync_relay_urls(self, relay_type, &relay_urls, None)
            .await?;
        Ok(())
    }

    pub(super) async fn publish_relay_list<S>(
        &self,
        relays: &[Relay],
        relay_type: RelayType,
        target_relays: &[Relay],
        signer: S,
    ) -> Result<()>
    where
        S: NostrSigner + 'static,
    {
        let relays_urls = Relay::urls(relays);
        let target_relays_urls = Relay::urls(target_relays);
        self.relay_control
            .publish_relay_list_with_signer(
                &relays_urls,
                relay_type,
                &target_relays_urls,
                std::sync::Arc::new(signer),
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn background_publish_account_metadata(
        &self,
        account: &Account,
    ) -> Result<()> {
        let account_clone = account.clone();
        let ephemeral = self.relay_control.ephemeral();
        let signer = self.get_signer_for_account(account)?;
        let user = account.user(&self.database).await?;
        let relays = account.nip65_relays(self).await?;

        tokio::spawn(async move {
            tracing::debug!(target: "whitenoise::accounts", "Background task: Publishing metadata for account: {:?}", account_clone.pubkey);

            let relays_urls = Relay::urls(&relays);

            ephemeral
                .publish_metadata_with_signer(&user.metadata, &relays_urls, signer)
                .await?;

            tracing::debug!(target: "whitenoise::accounts", "Successfully published metadata for account: {:?}", account_clone.pubkey);
            Ok::<(), WhitenoiseError>(())
        });
        Ok(())
    }

    /// Publishes the relay list for the account to the Nostr network.
    ///
    /// # Arguments
    ///
    /// * `account` - The account to publish the relay list for
    /// * `relay_type` - The type of relay list to publish
    /// * `relays` - The relays to publish the relay list to, if None, the relays will be fetched from the account
    pub(crate) async fn background_publish_account_relay_list(
        &self,
        account: &Account,
        relay_type: RelayType,
        relays: Option<&[Relay]>,
    ) -> Result<()> {
        let account_clone = account.clone();
        let ephemeral = self.relay_control.ephemeral();
        let relays = if let Some(relays) = relays {
            relays.to_vec()
        } else {
            account.relays(relay_type, self).await?
        };
        let signer = self.get_signer_for_account(account)?;
        let target_relays = if relay_type == RelayType::Nip65 {
            relays.clone()
        } else {
            account.nip65_relays(self).await?
        };

        tokio::spawn(async move {
            tracing::debug!(target: "whitenoise::accounts", "Background task: Publishing relay list for account: {:?}", account_clone.pubkey);

            let relays_urls = Relay::urls(&relays);
            let target_relays_urls = Relay::urls(&target_relays);

            ephemeral
                .publish_relay_list_with_signer(
                    &relays_urls,
                    relay_type,
                    &target_relays_urls,
                    signer,
                )
                .await?;

            tracing::debug!(target: "whitenoise::accounts", "Successfully published relay list for account: {:?}", account_clone.pubkey);
            Ok::<(), WhitenoiseError>(())
        });
        Ok(())
    }

    pub(crate) async fn background_publish_account_follow_list(
        &self,
        account: &Account,
    ) -> Result<()> {
        let account_clone = account.clone();
        let ephemeral = self.relay_control.ephemeral();
        let relays = account.nip65_relays(self).await?;
        let signer = self.get_signer_for_account(account)?;
        let follows = account.follows(&self.database).await?;
        let follows_pubkeys = follows.iter().map(|f| f.pubkey).collect::<Vec<_>>();

        tokio::spawn(async move {
            tracing::debug!(target: "whitenoise::accounts", "Background task: Publishing follow list for account: {:?}", account_clone.pubkey);

            let relays_urls = Relay::urls(&relays);
            ephemeral
                .publish_follow_list_with_signer(&follows_pubkeys, &relays_urls, signer)
                .await?;

            tracing::debug!(target: "whitenoise::accounts", "Successfully published follow list for account: {:?}", account_clone.pubkey);
            Ok::<(), WhitenoiseError>(())
        });
        Ok(())
    }

    /// Extract per-group relay specs for subscription setup.
    pub(crate) async fn extract_group_subscription_specs(
        &self,
        account: &Account,
    ) -> Result<Vec<GroupSubscriptionSpec>> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let groups = mdk.get_groups()?;
        let mut group_specs = Vec::with_capacity(groups.len());

        for group in &groups {
            if group.state != GroupState::Active {
                continue;
            }
            let relays = mdk.get_relays(&group.mls_group_id)?;
            group_specs.push(GroupSubscriptionSpec {
                group_id: hex::encode(group.nostr_group_id),
                relays: relays.into_iter().collect(),
            });
        }

        Ok(group_specs)
    }

    async fn refresh_account_group_subscriptions_with_cancel(
        &self,
        account: &Account,
        cancel_rx: Option<&watch::Receiver<bool>>,
    ) -> Result<()> {
        if Self::is_background_task_cancelled(cancel_rx) {
            tracing::debug!(
                target: "whitenoise::accounts::background_refresh_account_group_subscriptions",
                account_pubkey = %account.pubkey,
                "Skipping group subscription refresh because the account was cancelled",
            );
            return Ok(());
        }

        let group_specs = self.extract_group_subscription_specs(account).await?;

        for relay_url in group_specs.iter().flat_map(|group| group.relays.iter()) {
            Relay::find_or_create_by_url(relay_url, &self.database).await?;
        }

        if Self::is_background_task_cancelled(cancel_rx) {
            tracing::debug!(
                target: "whitenoise::accounts::background_refresh_account_group_subscriptions",
                account_pubkey = %account.pubkey,
                "Skipping group subscription sync because the account was cancelled",
            );
            return Ok(());
        }

        self.relay_control
            .sync_account_group_subscriptions(
                account.pubkey,
                &group_specs,
                account.since_timestamp(10),
            )
            .await
            .map_err(WhitenoiseError::from)
    }

    fn is_background_task_cancelled(cancel_rx: Option<&watch::Receiver<bool>>) -> bool {
        cancel_rx.map(|rx| *rx.borrow()).unwrap_or(true)
    }

    pub(crate) fn background_refresh_account_group_subscriptions(&self, account: &Account) {
        let account_clone = account.clone();
        let account_pubkey = account.pubkey;
        let cancel_rx = self
            .background_task_cancellation
            .get(&account.pubkey)
            .map(|entry| entry.value().subscribe());
        if cancel_rx.is_none() {
            tracing::debug!(
                target: "whitenoise::accounts::background_refresh_account_group_subscriptions",
                account_pubkey = %account.pubkey,
                "Skipping background group subscription refresh because the account has no cancellation channel",
            );
            return;
        }
        let refresh_task = tokio::spawn(async move {
            let whitenoise = match Whitenoise::get_instance() {
                Ok(wn) => wn,
                Err(error) => {
                    tracing::error!(
                        target: "whitenoise::accounts::background_refresh_account_group_subscriptions",
                        "Failed to get Whitenoise instance for background group subscription refresh: {}",
                        error
                    );
                    return;
                }
            };

            if let Err(error) = whitenoise
                .refresh_account_group_subscriptions_with_cancel(&account_clone, cancel_rx.as_ref())
                .await
            {
                tracing::warn!(
                    target: "whitenoise::accounts::background_refresh_account_group_subscriptions",
                    account_pubkey = %account_clone.pubkey,
                    "Background group subscription refresh failed: {}",
                    error
                );
            }
        });
        tokio::spawn(async move {
            if let Err(error) = refresh_task.await {
                tracing::error!(
                    target: "whitenoise::accounts::background_refresh_account_group_subscriptions",
                    account_pubkey = %account_pubkey,
                    "Background group subscription refresh task panicked: {}",
                    error
                );
            }
        });
    }

    pub(crate) async fn setup_subscriptions(
        &self,
        account: &Account,
        inbox_relays: &[Relay],
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::accounts",
            "Setting up subscriptions for account: {:?}",
            account
        );

        let inbox_relays: Vec<RelayUrl> = Relay::urls(inbox_relays);

        let group_specs = self.extract_group_subscription_specs(account).await?;

        // Ensure group relays are in the database
        for relay_url in group_specs.iter().flat_map(|group| group.relays.iter()) {
            Relay::find_or_create_by_url(relay_url, &self.database).await?;
        }

        // Compute per-account since with a 10s lookback buffer when available
        let since = account.since_timestamp(10);
        match since {
            Some(ts) => tracing::debug!(
                target: "whitenoise::accounts",
                "Computed per-account since={}s (10s buffer) for {}",
                ts.as_secs(),
                account.pubkey.to_hex()
            ),
            None => tracing::debug!(
                target: "whitenoise::accounts",
                "Computed per-account since=None (unsynced) for {}",
                account.pubkey.to_hex()
            ),
        }

        let signer = self.get_signer_for_account(account)?;
        self.relay_control
            .activate_account_subscriptions(
                account.pubkey,
                &inbox_relays,
                &group_specs,
                since,
                signer,
            )
            .await?;

        tracing::debug!(
            target: "whitenoise::accounts",
            "Subscriptions setup"
        );
        Ok(())
    }

    /// Refresh account subscriptions.
    ///
    /// This method updates subscriptions when account state changes (group membership, relay
    /// preferences). It first unsubscribes all existing account subscriptions to clean up stale
    /// relay state, then resubscribes with a replay anchor derived from the account's sync state.
    ///
    /// Gift-wrap backdating (nostr-sdk timestamps gift-wraps up to 48 hours in the past) is
    /// handled inside `setup_giftwrap_subscription` via `adjust_since_for_giftwrap`, which
    /// subtracts an additional `GIFTWRAP_LOOKBACK_BUFFER` (7 days) from the `since` value.
    /// No extra buffer is needed here for that purpose.
    ///
    /// The replay anchor falls back to `None` (no since filter) when the account has never
    /// synced, so the relay sends everything it has.
    ///
    /// # Arguments
    ///
    /// * `account` - The account to refresh subscriptions for
    pub(crate) async fn refresh_account_subscriptions(&self, account: &Account) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::accounts",
            "Refreshing account subscriptions for account: {:?}",
            account.pubkey
        );

        // Gather all inputs before touching existing subscriptions so that a
        // fallible data-fetch cannot leave the account with no active subs.
        let inbox_relays: Vec<RelayUrl> = Relay::urls(&account.effective_inbox_relays(self).await?);

        let group_specs = self.extract_group_subscription_specs(account).await?;

        // 10s buffer to avoid missing events at the boundary of the last sync window.
        // Gift-wrap backdating is handled separately inside setup_giftwrap_subscription.
        let since = account.since_timestamp(10);

        let signer = self.get_signer_for_account(account)?;

        // All inputs ready — now safe to tear down and replace.
        self.relay_control
            .deactivate_account_subscriptions(&account.pubkey)
            .await;

        self.relay_control
            .activate_account_subscriptions(
                account.pubkey,
                &inbox_relays,
                &group_specs,
                since,
                signer,
            )
            .await
            .map_err(WhitenoiseError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;
    use crate::whitenoise::test_utils::*;
    use mdk_core::prelude::*;

    #[tokio::test]
    async fn test_extract_group_subscription_specs_no_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let group_specs = whitenoise
            .extract_group_subscription_specs(&account)
            .await
            .unwrap();

        assert!(
            group_specs.is_empty(),
            "Should have no group specs when account has no groups"
        );
    }

    #[tokio::test]
    async fn test_extract_group_subscription_specs_with_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create creator and member accounts
        let creator_account = whitenoise.create_identity().await.unwrap();
        let member_account = whitenoise.create_identity().await.unwrap();

        // Allow time for key packages to be published
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        let relay1 = RelayUrl::parse("ws://localhost:8080").unwrap();
        let relay2 = RelayUrl::parse("ws://localhost:7777").unwrap();

        // Create a group with specific relays
        let config = NostrGroupConfigData::new(
            "Test Group".to_string(),
            "Test Description".to_string(),
            None,
            None,
            None,
            vec![relay1.clone(), relay2.clone()],
            vec![creator_account.pubkey],
        );

        let group = whitenoise
            .create_group(&creator_account, vec![member_account.pubkey], config, None)
            .await
            .unwrap();

        // Extract per-group relay specs
        let group_specs = whitenoise
            .extract_group_subscription_specs(&creator_account)
            .await
            .unwrap();

        assert_eq!(group_specs.len(), 1, "Should have one group spec");
        let spec = &group_specs[0];

        // Verify relays were extracted for the group
        assert!(
            spec.relays.contains(&relay1),
            "Should contain relay1 from group config"
        );
        assert!(
            spec.relays.contains(&relay2),
            "Should contain relay2 from group config"
        );

        // Verify group ID was extracted
        assert_eq!(
            spec.group_id,
            hex::encode(group.nostr_group_id),
            "Group ID should match the created group"
        );
    }

    #[tokio::test]
    async fn test_sync_account_relays_replaces_stale_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let user = account.user(&whitenoise.database).await.unwrap();

        let stale_relay = Relay::find_or_create_by_url(
            &RelayUrl::parse("wss://stale-relay.example.com").unwrap(),
            &whitenoise.database,
        )
        .await
        .unwrap();

        let replacement_a = Relay::find_or_create_by_url(
            &RelayUrl::parse("wss://replacement-a.example.com").unwrap(),
            &whitenoise.database,
        )
        .await
        .unwrap();
        let replacement_b = Relay::find_or_create_by_url(
            &RelayUrl::parse("wss://replacement-b.example.com").unwrap(),
            &whitenoise.database,
        )
        .await
        .unwrap();

        user.add_relays(
            std::slice::from_ref(&stale_relay),
            RelayType::Nip65,
            &whitenoise.database,
        )
        .await
        .unwrap();

        let initial_urls = account
            .nip65_relays(&whitenoise)
            .await
            .unwrap()
            .into_iter()
            .map(|relay| relay.url)
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            initial_urls.contains(&stale_relay.url),
            "Test setup should create an initial stale relay entry"
        );

        whitenoise
            .sync_account_relays(
                &account,
                &[replacement_a.clone(), replacement_b.clone()],
                RelayType::Nip65,
            )
            .await
            .unwrap();

        let stored_urls = account
            .nip65_relays(&whitenoise)
            .await
            .unwrap()
            .into_iter()
            .map(|relay| relay.url)
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            stored_urls,
            [replacement_a.url, replacement_b.url]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
        );
        assert!(
            !stored_urls.contains(&stale_relay.url),
            "Stale relay should be removed during sync"
        );
    }

    #[tokio::test]
    async fn test_is_own_key_package_returns_false_for_unknown_event() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, _keys) = create_test_account(&whitenoise).await;

        let event_id = EventId::all_zeros();
        assert!(
            !whitenoise
                .is_own_key_package(&account.pubkey, &event_id)
                .await
        );
    }

    #[tokio::test]
    async fn test_is_own_key_package_returns_true_for_tracked_kp() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, _keys) = create_test_account(&whitenoise).await;
        account.save(&whitenoise.database).await.unwrap();

        // Use a valid 64-char hex string as event_id
        let event_hex = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        PublishedKeyPackage::create(&account.pubkey, &[1, 2, 3], event_hex, &whitenoise.database)
            .await
            .unwrap();

        let event_id = EventId::parse(event_hex).unwrap();
        assert!(
            whitenoise
                .is_own_key_package(&account.pubkey, &event_id)
                .await
        );
    }

    #[tokio::test]
    async fn test_is_own_key_package_returns_false_when_key_material_deleted() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, _keys) = create_test_account(&whitenoise).await;
        account.save(&whitenoise.database).await.unwrap();

        let event_hex = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        PublishedKeyPackage::create(&account.pubkey, &[1, 2, 3], event_hex, &whitenoise.database)
            .await
            .unwrap();

        // Mark key material as deleted
        let pkg =
            PublishedKeyPackage::find_by_event_id(&account.pubkey, event_hex, &whitenoise.database)
                .await
                .unwrap()
                .unwrap();
        PublishedKeyPackage::mark_key_material_deleted(pkg.id, &whitenoise.database)
            .await
            .unwrap();

        let event_id = EventId::parse(event_hex).unwrap();
        assert!(
            !whitenoise
                .is_own_key_package(&account.pubkey, &event_id)
                .await
        );
    }

    #[tokio::test]
    async fn test_is_own_key_package_returns_false_on_db_error() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let pubkey = Keys::generate().public_key();

        // Close the pool to simulate a database error
        whitenoise.database.pool.close().await;

        let event_id = EventId::all_zeros();
        assert!(!whitenoise.is_own_key_package(&pubkey, &event_id).await);
    }
}
