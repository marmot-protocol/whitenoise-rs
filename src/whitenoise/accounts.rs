use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_blossom::client::BlossomClient;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::RelayType;
use crate::nostr_manager::{NostrManager, NostrManagerError};
use crate::types::ImageType;
use crate::whitenoise::error::Result;
use crate::whitenoise::relays::Relay;
use crate::whitenoise::users::User;
use crate::whitenoise::{Whitenoise, WhitenoiseError};

/// The type of account authentication.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, Default)]
pub enum AccountType {
    /// Account with locally stored private key.
    #[default]
    Local,
    /// Account using external signer (e.g., Amber via NIP-55).
    /// The private key never touches this app.
    External,
}

impl fmt::Display for AccountType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountType::Local => write!(f, "local"),
            AccountType::External => write!(f, "external"),
        }
    }
}

impl FromStr for AccountType {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "local" => Ok(AccountType::Local),
            "external" => Ok(AccountType::External),
            _ => Err(format!("Unknown account type: {}", s)),
        }
    }
}

#[derive(Error, Debug)]
pub enum AccountError {
    #[error("Failed to parse public key: {0}")]
    PublicKeyError(#[from] nostr_sdk::key::Error),

    #[error("Failed to initialize Nostr manager: {0}")]
    NostrManagerError(#[from] NostrManagerError),

    #[error("Nostr MLS error: {0}")]
    NostrMlsError(#[from] mdk_core::Error),

    #[error("Nostr MLS SQLite storage error: {0}")]
    NostrMlsSqliteStorageError(#[from] mdk_sqlite_storage::error::Error),

    #[error("Nostr MLS not initialized")]
    NostrMlsNotInitialized,

    #[error("Whitenoise not initialized")]
    WhitenoiseNotInitialized,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: Option<i64>,
    pub pubkey: PublicKey,
    pub user_id: i64,
    /// The type of account (local key or external signer).
    pub account_type: AccountType,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Account {
    /// Returns true if this account uses an external signer.
    pub fn uses_external_signer(&self) -> bool {
        matches!(self.account_type, AccountType::External)
    }

    /// Returns true if this account has a locally stored private key.
    pub fn has_local_key(&self) -> bool {
        matches!(self.account_type, AccountType::Local)
    }
}

impl Account {
    pub(crate) async fn new(
        whitenoise: &Whitenoise,
        keys: Option<Keys>,
    ) -> Result<(Account, Keys)> {
        let keys = keys.unwrap_or_else(Keys::generate);

        let (user, _created) =
            User::find_or_create_by_pubkey(&keys.public_key(), &whitenoise.database).await?;

        let account = Account {
            id: None,
            user_id: user.id.unwrap(),
            pubkey: keys.public_key(),
            account_type: AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        Ok((account, keys))
    }

    /// Creates a new account for an external signer (pubkey only, no private key).
    pub(crate) async fn new_external(
        whitenoise: &Whitenoise,
        pubkey: PublicKey,
    ) -> Result<Account> {
        let (user, _created) =
            User::find_or_create_by_pubkey(&pubkey, &whitenoise.database).await?;

        let account = Account {
            id: None,
            user_id: user.id.unwrap(),
            pubkey,
            account_type: AccountType::External,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        Ok(account)
    }

    /// Convert last_synced_at to a Timestamp applying a lookback buffer.
    /// Clamps future timestamps to now to avoid empty subscriptions.
    /// Returns None if the account has never synced.
    pub(crate) fn since_timestamp(&self, buffer_secs: u64) -> Option<nostr_sdk::Timestamp> {
        let ts = self.last_synced_at?;
        // Clamp to now, then apply buffer
        let now_secs = Utc::now().timestamp().max(0) as u64;
        let last_secs = (ts.timestamp().max(0) as u64).min(now_secs);
        let secs = last_secs.saturating_sub(buffer_secs);
        Some(nostr_sdk::Timestamp::from(secs))
    }

    /// Retrieves the account's configured relays for a specific relay type.
    ///
    /// This method fetches the locally cached relays associated with this account
    /// for the specified relay type. Different relay types serve different purposes
    /// in the Nostr ecosystem and are published as separate relay list events.
    ///
    /// # Arguments
    ///
    /// * `relay_type` - The type of relays to retrieve:
    ///   - `RelayType::Nip65` - General purpose relays for reading/writing events (kind 10002)
    ///   - `RelayType::Inbox` - Specialized relays for receiving private messages (kind 10050)
    ///   - `RelayType::KeyPackage` - Relays that store MLS key packages (kind 10051)
    /// * `whitenoise` - The Whitenoise instance for database operations
    pub async fn relays(
        &self,
        relay_type: RelayType,
        whitenoise: &Whitenoise,
    ) -> Result<Vec<Relay>> {
        let user = self.user(&whitenoise.database).await?;
        let relays = user.relays(relay_type, &whitenoise.database).await?;
        Ok(relays)
    }

    /// Helper method to retrieve the NIP-65 relays for this account.
    pub(crate) async fn nip65_relays(&self, whitenoise: &Whitenoise) -> Result<Vec<Relay>> {
        let user = self.user(&whitenoise.database).await?;
        let relays = user.relays(RelayType::Nip65, &whitenoise.database).await?;
        Ok(relays)
    }

    /// Helper method to retrieve the inbox relays for this account.
    pub(crate) async fn inbox_relays(&self, whitenoise: &Whitenoise) -> Result<Vec<Relay>> {
        let user = self.user(&whitenoise.database).await?;
        let relays = user.relays(RelayType::Inbox, &whitenoise.database).await?;
        Ok(relays)
    }

    /// Helper method to retrieve the key package relays for this account.
    pub(crate) async fn key_package_relays(&self, whitenoise: &Whitenoise) -> Result<Vec<Relay>> {
        let user = self.user(&whitenoise.database).await?;
        let relays = user
            .relays(RelayType::KeyPackage, &whitenoise.database)
            .await?;
        Ok(relays)
    }

    /// Adds a relay to the account's relay list for the specified relay type.
    ///
    /// This method adds a relay to the account's local relay configuration and automatically
    /// publishes the updated relay list to the Nostr network. The relay will be associated
    /// with the specified type (NIP-65, Inbox, or Key Package relays) and become part of
    /// the account's relay configuration for that purpose.
    ///
    /// # Arguments
    ///
    /// * `relay` - The relay to add to the account's relay list
    /// * `relay_type` - The type of relay list to add this relay to:
    ///   - `RelayType::Nip65` - General purpose relays (kind 10002)
    ///   - `RelayType::Inbox` - Inbox relays for private messages (kind 10050)
    ///   - `RelayType::KeyPackage` - Key package relays for MLS (kind 10051)
    /// * `whitenoise` - The Whitenoise instance for database and network operations
    pub async fn add_relay(
        &self,
        relay: &Relay,
        relay_type: RelayType,
        whitenoise: &Whitenoise,
    ) -> Result<()> {
        let user = self.user(&whitenoise.database).await?;
        user.add_relay(relay, relay_type, &whitenoise.database)
            .await?;
        whitenoise
            .background_publish_account_relay_list(self, relay_type, None)
            .await?;
        tracing::debug!(target: "whitenoise::accounts::add_relay", "Added relay to account: {:?}", relay.url);

        Ok(())
    }

    /// Removes a relay from the account's relay list for the specified relay type.
    ///
    /// This method removes a relay from the account's local relay configuration and automatically
    /// publishes the updated relay list to the Nostr network. The relay will be disassociated
    /// from the specified type and the account will stop using it for that purpose.
    ///
    /// # Arguments
    ///
    /// * `relay` - The relay to remove from the account's relay list
    /// * `relay_type` - The type of relay list to remove this relay from:
    ///   - `RelayType::Nip65` - General purpose relays (kind 10002)
    ///   - `RelayType::Inbox` - Inbox relays for private messages (kind 10050)
    ///   - `RelayType::KeyPackage` - Key package relays for MLS (kind 10051)
    /// * `whitenoise` - The Whitenoise instance for database and network operations
    pub async fn remove_relay(
        &self,
        relay: &Relay,
        relay_type: RelayType,
        whitenoise: &Whitenoise,
    ) -> Result<()> {
        let user = self.user(&whitenoise.database).await?;
        user.remove_relay(relay, relay_type, &whitenoise.database)
            .await?;
        whitenoise
            .background_publish_account_relay_list(self, relay_type, None)
            .await?;
        tracing::debug!(target: "whitenoise::accounts::remove_relay", "Removed relay from account: {:?}", relay.url);
        Ok(())
    }

    /// Retrieves the cached metadata for this account.
    ///
    /// This method returns the account's stored metadata from the local database without
    /// performing any network requests. The metadata contains profile information such as
    /// display name, about text, picture URL, and other profile fields as defined by NIP-01.
    ///
    /// # Arguments
    ///
    /// * `whitenoise` - The Whitenoise instance used to access the database
    pub async fn metadata(&self, whitenoise: &Whitenoise) -> Result<Metadata> {
        let user = self.user(&whitenoise.database).await?;
        Ok(user.metadata.clone())
    }

    /// Updates the account's metadata with new values and publishes to the network.
    ///
    /// This method updates the account's metadata in the local database with the provided
    /// values and automatically publishes a metadata event (kind 0) to the account's relays.
    /// This allows other users and clients to see the updated profile information.
    ///
    /// # Arguments
    ///
    /// * `metadata` - The new metadata to set for this account
    /// * `whitenoise` - The Whitenoise instance for database and network operations
    pub async fn update_metadata(
        &self,
        metadata: &Metadata,
        whitenoise: &Whitenoise,
    ) -> Result<()> {
        tracing::debug!(target: "whitenoise::accounts::update_metadata", "Updating metadata for account: {:?}", self.pubkey);
        let mut user = self.user(&whitenoise.database).await?;
        user.metadata = metadata.clone();
        user.save(&whitenoise.database).await?;
        whitenoise.background_publish_account_metadata(self).await?;
        Ok(())
    }

    /// Uploads an image file to a Blossom server and returns the URL.
    ///
    /// # Arguments
    /// * `file_path` - Path to the image file to upload
    /// * `image_type` - Image type (JPEG, PNG, etc.)
    /// * `server` - Blossom server URL
    /// * `whitenoise` - Whitenoise instance for accessing account keys
    pub async fn upload_profile_picture(
        &self,
        file_path: &str,
        image_type: ImageType,
        server: Url,
        whitenoise: &Whitenoise,
    ) -> Result<String> {
        let client = BlossomClient::new(server);
        let keys = whitenoise
            .secrets_store
            .get_nostr_keys_for_pubkey(&self.pubkey)?;
        let data = tokio::fs::read(file_path).await?;

        let descriptor = client
            .upload_blob(
                data,
                Some(image_type.mime_type().to_string()),
                None,
                Some(&keys),
            )
            .await
            .map_err(|err| WhitenoiseError::Other(anyhow::anyhow!(err)))?;

        Ok(descriptor.url.to_string())
    }

    pub(crate) fn create_mdk(
        pubkey: PublicKey,
        data_dir: &Path,
    ) -> core::result::Result<MDK<MdkSqliteStorage>, AccountError> {
        let mls_storage_dir = data_dir.join("mls").join(pubkey.to_hex());
        let storage = MdkSqliteStorage::new(mls_storage_dir)?;
        Ok(MDK::new(storage))
    }
}

impl Whitenoise {
    /// Creates a new identity (account) for the user.
    ///
    /// This method generates a new keypair, sets up the account with default relay lists,
    /// and fully configures the account for use in Whitenoise.
    pub async fn create_identity(&self) -> Result<Account> {
        let keys = Keys::generate();
        tracing::debug!(target: "whitenoise::create_identity", "Generated new keypair: {}", keys.public_key().to_hex());

        let mut account = self.create_base_account_with_private_key(&keys).await?;
        tracing::debug!(target: "whitenoise::create_identity", "Keys stored in secret store and account saved to database");

        let user = account.user(&self.database).await?;

        let relays = self
            .setup_relays_for_new_account(&mut account, &user)
            .await?;
        tracing::debug!(target: "whitenoise::create_identity", "Relays setup");

        self.activate_account(&account, &user, true, &relays, &relays, &relays)
            .await?;
        tracing::debug!(target: "whitenoise::create_identity", "Account persisted and activated");

        tracing::debug!(target: "whitenoise::create_identity", "Successfully created new identity: {}", account.pubkey.to_hex());
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
        tracing::debug!(target: "whitenoise::login", "Logging in with pubkey: {}", pubkey.to_hex());

        let mut account = self.create_base_account_with_private_key(&keys).await?;
        tracing::debug!(target: "whitenoise::login", "Keys stored in secret store and account saved to database");

        // Always check for existing relay lists when logging in, even if the user is
        // newly created in our database, because the keypair might already exist in
        // the Nostr ecosystem with published relay lists from other apps
        let (nip65_relays, inbox_relays, key_package_relays) =
            self.setup_relays_for_existing_account(&mut account).await?;
        tracing::debug!(target: "whitenoise::login", "Relays setup");

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
        tracing::debug!(target: "whitenoise::login", "Account persisted and activated");

        tracing::debug!(target: "whitenoise::login", "Successfully logged in: {}", account.pubkey.to_hex());
        Ok(account)
    }

    /// Logs in using an external signer (e.g., Amber via NIP-55).
    ///
    /// This method creates an account for the given public key without storing any
    /// private key locally. All signing operations must be performed by the external
    /// signer and provided back to Whitenoise.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The user's public key obtained from the external signer.
    ///
    /// # Note
    ///
    /// When using an external signer account, operations that require signing
    /// (like publishing metadata, relay lists, or sending messages) must use
    /// the `*_with_signer` variants that accept a `NostrSigner` parameter.
    pub async fn login_with_external_signer(&self, pubkey: PublicKey) -> Result<Account> {
        tracing::debug!(target: "whitenoise::login_external", "Logging in with external signer, pubkey: {}", pubkey.to_hex());

        // Check if account already exists
        if let Ok(existing) = Account::find_by_pubkey(&pubkey, &self.database).await {
            tracing::debug!(target: "whitenoise::login_external", "Found existing account, re-establishing relays and subscriptions");

            let mut account_mut = existing.clone();

            // Always attempt to remove any locally stored key for external logins
            // This handles edge cases where a prior migration failed or stale keys remain
            if let Err(e) = self
                .secrets_store
                .remove_private_key_for_pubkey(&account_mut.pubkey)
            {
                tracing::debug!(
                    target: "whitenoise::login_external",
                    "No local key to remove during external-login: {}",
                    e
                );
            }

            // Handle migration from Local to External account type
            if account_mut.account_type != AccountType::External {
                tracing::info!(
                    target: "whitenoise::login_external",
                    "Migrating account from {:?} to External",
                    account_mut.account_type
                );

                // Update account type to External and persist
                account_mut.account_type = AccountType::External;
                account_mut = self.persist_account(&account_mut).await?;
                tracing::debug!(target: "whitenoise::login_external", "Account migrated to External type");
            }

            // Setup relays for existing external signer account
            let (nip65_relays, inbox_relays, key_package_relays) = self
                .setup_relays_for_external_signer_account(&mut account_mut)
                .await?;
            tracing::debug!(target: "whitenoise::login_external", "Relays setup for existing account (without publishing)");

            let user = account_mut.user(&self.database).await?;

            // Activate without publishing (external signer will handle publishing)
            self.activate_account_without_publishing(
                &account_mut,
                &user,
                &nip65_relays,
                &inbox_relays,
                &key_package_relays,
            )
            .await?;
            tracing::debug!(target: "whitenoise::login_external", "Existing account activated (without publishing)");

            return Ok(account_mut);
        }

        // Create new external signer account
        let account = Account::new_external(self, pubkey).await?;
        let account = self.persist_account(&account).await?;
        tracing::debug!(target: "whitenoise::login_external", "Created new external signer account");

        // Setup relays for external signer account (fetch from network or use defaults)
        // This does NOT publish - the external signer will handle publishing
        let mut account_mut = account.clone();
        let (nip65_relays, inbox_relays, key_package_relays) = self
            .setup_relays_for_external_signer_account(&mut account_mut)
            .await?;
        tracing::debug!(target: "whitenoise::login_external", "Relays setup (without publishing)");

        let user = account_mut.user(&self.database).await?;

        // Activate without publishing (external signer will handle publishing)
        self.activate_account_without_publishing(
            &account_mut,
            &user,
            &nip65_relays,
            &inbox_relays,
            &key_package_relays,
        )
        .await?;
        tracing::debug!(target: "whitenoise::login_external", "Account activated (without publishing)");

        tracing::debug!(target: "whitenoise::login_external", "Successfully logged in with external signer: {}", account_mut.pubkey.to_hex());
        Ok(account_mut)
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

        // Unsubscribe from account-specific subscriptions before logout
        if let Err(e) = self.nostr.unsubscribe_account_subscriptions(pubkey).await {
            tracing::warn!(
                target: "whitenoise::logout",
                "Failed to unsubscribe from account subscriptions for {}: {}",
                pubkey, e
            );
            // Don't fail logout if unsubscribe fails
        }

        // Delete the account from the database
        account.delete(&self.database).await?;

        // Remove the private key from the secret store
        // For local accounts this is required; for external accounts this is best-effort cleanup
        if account.has_local_key() {
            self.secrets_store.remove_private_key_for_pubkey(pubkey)?;
        } else if let Err(e) = self.secrets_store.remove_private_key_for_pubkey(pubkey) {
            tracing::debug!(
                target: "whitenoise::logout",
                "No local key to remove for external account {}: {}",
                pubkey,
                e
            );
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

    async fn create_base_account_with_private_key(&self, keys: &Keys) -> Result<Account> {
        let (account, _keys) = Account::new(self, Some(keys.clone())).await?;

        self.secrets_store.store_private_key(keys).map_err(|e| {
            tracing::error!(target: "whitenoise::setup_account", "Failed to store private key: {}", e);
            e
        })?;

        let account = self.persist_account(&account).await?;

        Ok(account)
    }

    async fn activate_account(
        &self,
        account: &Account,
        user: &User,
        is_new_account: bool,
        nip65_relays: &[Relay],
        inbox_relays: &[Relay],
        key_package_relays: &[Relay],
    ) -> Result<()> {
        let relay_urls: Vec<RelayUrl> = Relay::urls(
            nip65_relays
                .iter()
                .chain(inbox_relays)
                .chain(key_package_relays),
        );
        self.nostr.ensure_relays_connected(&relay_urls).await?;
        tracing::debug!(target: "whitenoise::persist_and_activate_account", "Relays connected");
        if let Err(e) = self.refresh_global_subscription_for_user(user).await {
            tracing::warn!(
                target: "whitenoise::persist_and_activate_account",
                "Failed to refresh global subscription for new user {}: {}",
                user.pubkey,
                e
            );
        }
        tracing::debug!(target: "whitenoise::persist_and_activate_account", "Global subscription refreshed for account user");
        self.setup_subscriptions(account, nip65_relays, inbox_relays)
            .await?;
        tracing::debug!(target: "whitenoise::persist_and_activate_account", "Subscriptions setup");
        self.setup_key_package(account, is_new_account, key_package_relays)
            .await?;
        tracing::debug!(target: "whitenoise::persist_and_activate_account", "Key package setup");
        Ok(())
    }

    /// Activates an account without publishing anything (for external signer accounts).
    /// This sets up relay connections and subscriptions but skips key package publishing.
    async fn activate_account_without_publishing(
        &self,
        account: &Account,
        user: &User,
        nip65_relays: &[Relay],
        inbox_relays: &[Relay],
        key_package_relays: &[Relay],
    ) -> Result<()> {
        let relay_urls: Vec<RelayUrl> = Relay::urls(
            nip65_relays
                .iter()
                .chain(inbox_relays)
                .chain(key_package_relays),
        );
        self.nostr.ensure_relays_connected(&relay_urls).await?;
        tracing::debug!(target: "whitenoise::activate_account_without_publishing", "Relays connected");

        if let Err(e) = self.refresh_global_subscription_for_user(user).await {
            tracing::warn!(
                target: "whitenoise::activate_account_without_publishing",
                "Failed to refresh global subscription for new user {}: {}",
                user.pubkey,
                e
            );
        }
        tracing::debug!(target: "whitenoise::activate_account_without_publishing", "Global subscription refreshed");

        self.setup_subscriptions(account, nip65_relays, inbox_relays)
            .await?;
        tracing::debug!(target: "whitenoise::activate_account_without_publishing", "Subscriptions setup");

        // Note: We skip key package setup for external signer accounts.
        // Key packages need to be published separately with the external signer.
        tracing::debug!(target: "whitenoise::activate_account_without_publishing", "Skipping key package setup (external signer)");

        Ok(())
    }

    async fn setup_metadata(&self, account: &Account, user: &mut User) -> Result<()> {
        let petname = petname::petname(2, " ")
            .unwrap_or_else(|| "Anonymous User".to_string())
            .split_whitespace()
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    None => String::new(),
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");

        let default_name = "Anonymous".to_string();
        let metadata = Metadata::new().name(&petname);

        self.nostr
            .update_metadata(account, user, &metadata, &self.database)
            .await?;
        tracing::debug!(target: "whitenoise::setup_metadata", "Created and published metadata with petname: {}", metadata.name.as_ref().unwrap_or(&default_name));
        Ok(())
    }
    async fn persist_account(&self, account: &Account) -> Result<Account> {
        let saved_account = account.save(&self.database).await.map_err(|e| {
            tracing::error!(target: "whitenoise::setup_account", "Failed to save account: {}", e);
            // Try to clean up stored private key
            if let Err(cleanup_err) = self.secrets_store.remove_private_key_for_pubkey(&account.pubkey) {
                tracing::error!(target: "whitenoise::setup_account", "Failed to cleanup private key after account save failure: {}", cleanup_err);
            }
            e
        })?;
        tracing::debug!(target: "whitenoise::setup_account", "Account saved to database");
        Ok(saved_account)
    }

    async fn setup_key_package(
        &self,
        account: &Account,
        is_new_account: bool,
        key_package_relays: &[Relay],
    ) -> Result<()> {
        let mut key_package_event = None;
        if !is_new_account {
            tracing::debug!(target: "whitenoise::setup_key_package", "Found {} key package relays", key_package_relays.len());
            let relays_urls = Relay::urls(key_package_relays);
            key_package_event = self
                .nostr
                .fetch_user_key_package(account.pubkey, &relays_urls)
                .await?;
        }
        if key_package_event.is_none() {
            self.publish_key_package_to_relays(account, key_package_relays)
                .await?;
            tracing::debug!(target: "whitenoise::setup_account", "Published key package");
        }
        Ok(())
    }

    async fn load_default_relays(&self) -> Result<Vec<Relay>> {
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
    async fn setup_relays_for_new_account(
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

    async fn setup_relays_for_existing_account(
        &self,
        account: &mut Account,
    ) -> Result<(Vec<Relay>, Vec<Relay>, Vec<Relay>)> {
        let default_relays = self.load_default_relays().await?;
        let keys = self
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)?;

        // Existing accounts: Try to fetch existing relay lists, use defaults as fallback
        let (nip65_relays, should_publish_nip65) = self
            .setup_existing_account_relay_type(
                account,
                RelayType::Nip65,
                &default_relays,
                &default_relays,
            )
            .await?;

        let (inbox_relays, should_publish_inbox) = self
            .setup_existing_account_relay_type(
                account,
                RelayType::Inbox,
                &nip65_relays,
                &default_relays,
            )
            .await?;

        let (key_package_relays, should_publish_key_package) = self
            .setup_existing_account_relay_type(
                account,
                RelayType::KeyPackage,
                &nip65_relays,
                &default_relays,
            )
            .await?;

        // Only publish relay lists that need publishing (when using defaults as fallback)
        if should_publish_nip65 {
            self.publish_relay_list(&nip65_relays, RelayType::Nip65, &nip65_relays, &keys)
                .await?;
        }
        if should_publish_inbox {
            self.publish_relay_list(&inbox_relays, RelayType::Inbox, &nip65_relays, &keys)
                .await?;
        }
        if should_publish_key_package {
            self.publish_relay_list(
                &key_package_relays,
                RelayType::KeyPackage,
                &nip65_relays,
                &keys,
            )
            .await?;
        }

        Ok((nip65_relays, inbox_relays, key_package_relays))
    }

    /// Sets up relays for an external signer account.
    ///
    /// This is similar to `setup_relays_for_existing_account` but does NOT publish
    /// relay lists (since external signers handle their own publishing).
    /// It only fetches existing relays or uses defaults and saves them locally.
    async fn setup_relays_for_external_signer_account(
        &self,
        account: &mut Account,
    ) -> Result<(Vec<Relay>, Vec<Relay>, Vec<Relay>)> {
        let default_relays = self.load_default_relays().await?;

        // Existing accounts: Try to fetch existing relay lists, use defaults as fallback
        // We don't publish here - external signer will handle that
        let nip65_relays = self
            .setup_external_account_relay_type(
                account,
                RelayType::Nip65,
                &default_relays,
                &default_relays,
            )
            .await?;

        let inbox_relays = self
            .setup_external_account_relay_type(
                account,
                RelayType::Inbox,
                &nip65_relays,
                &default_relays,
            )
            .await?;

        let key_package_relays = self
            .setup_external_account_relay_type(
                account,
                RelayType::KeyPackage,
                &nip65_relays,
                &default_relays,
            )
            .await?;

        Ok((nip65_relays, inbox_relays, key_package_relays))
    }

    /// Sets up a specific relay type for an external signer account.
    /// Fetches from network or uses defaults, but never publishes.
    async fn setup_external_account_relay_type(
        &self,
        account: &mut Account,
        relay_type: RelayType,
        source_relays: &[Relay],
        default_relays: &[Relay],
    ) -> Result<Vec<Relay>> {
        // Try to fetch existing relay lists first
        let fetched_relays = self
            .fetch_existing_relays(account.pubkey, relay_type, source_relays)
            .await?;

        let user = account.user(&self.database).await?;
        if fetched_relays.is_empty() {
            // No existing relay lists - use defaults (don't publish)
            user.add_relays(default_relays, relay_type, &self.database)
                .await?;
            Ok(default_relays.to_vec())
        } else {
            // Found existing relay lists - use them
            user.add_relays(&fetched_relays, relay_type, &self.database)
                .await?;
            Ok(fetched_relays)
        }
    }

    async fn setup_existing_account_relay_type(
        &self,
        account: &mut Account,
        relay_type: RelayType,
        source_relays: &[Relay],
        default_relays: &[Relay],
    ) -> Result<(Vec<Relay>, bool)> {
        // Existing accounts: try to fetch existing relay lists first
        let fetched_relays = self
            .fetch_existing_relays(account.pubkey, relay_type, source_relays)
            .await?;

        if fetched_relays.is_empty() {
            // No existing relay lists - use defaults and publish
            self.add_relays_to_account(account, default_relays, relay_type)
                .await?;
            Ok((default_relays.to_vec(), true))
        } else {
            // Found existing relay lists - use them, no publishing needed
            let user = account.user(&self.database).await?;
            user.add_relays(&fetched_relays, relay_type, &self.database)
                .await?;
            Ok((fetched_relays, false))
        }
    }

    async fn fetch_existing_relays(
        &self,
        pubkey: PublicKey,
        relay_type: RelayType,
        source_relays: &[Relay],
    ) -> Result<Vec<Relay>> {
        let source_relay_urls = Relay::urls(source_relays);
        let relay_event = self
            .nostr
            .fetch_user_relays(pubkey, relay_type, &source_relay_urls)
            .await?;

        let mut relays = Vec::new();
        if let Some(event) = relay_event {
            let relay_urls = NostrManager::relay_urls_from_event(&event);
            for url in relay_urls {
                let relay = self.find_or_create_relay_by_url(&url).await?;
                relays.push(relay);
            }
        }

        Ok(relays)
    }

    async fn add_relays_to_account(
        &self,
        account: &Account,
        relays: &[Relay],
        relay_type: RelayType,
    ) -> Result<()> {
        if relays.is_empty() {
            return Ok(());
        }

        let user = account.user(&self.database).await?;
        user.add_relays(relays, relay_type, &self.database).await?;

        self.background_publish_account_relay_list(account, relay_type, Some(relays))
            .await?;

        tracing::debug!(target: "whitenoise::add_relays_to_account", "Added {} relays of type {:?} to account", relays.len(), relay_type);

        Ok(())
    }

    async fn publish_relay_list(
        &self,
        relays: &[Relay],
        relay_type: RelayType,
        target_relays: &[Relay],
        keys: &Keys,
    ) -> Result<()> {
        let relays_urls = Relay::urls(relays);
        let target_relays_urls = Relay::urls(target_relays);
        self.nostr
            .publish_relay_list_with_signer(
                &relays_urls,
                relay_type,
                &target_relays_urls,
                keys.clone(),
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn background_publish_account_metadata(
        &self,
        account: &Account,
    ) -> Result<()> {
        let account_clone = account.clone();
        let nostr = self.nostr.clone();
        let signer = self
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)?;
        let user = account.user(&self.database).await?;
        let relays = account.nip65_relays(self).await?;

        tokio::spawn(async move {
            tracing::debug!(target: "whitenoise::accounts::background_publish_user_metadata", "Background task: Publishing metadata for account: {:?}", account_clone.pubkey);

            let relays_urls = Relay::urls(&relays);

            nostr
                .publish_metadata_with_signer(&user.metadata, &relays_urls, signer)
                .await?;

            tracing::debug!(target: "whitenoise::accounts::background_publish_user_metadata", "Successfully published metadata for account: {:?}", account_clone.pubkey);
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
        let nostr = self.nostr.clone();
        let relays = if let Some(relays) = relays {
            relays.to_vec()
        } else {
            account.relays(relay_type, self).await?
        };
        let keys = self
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)?;
        let target_relays = if relay_type == RelayType::Nip65 {
            relays.clone()
        } else {
            account.nip65_relays(self).await?
        };

        tokio::spawn(async move {
            tracing::debug!(target: "whitenoise::accounts::background_publish_account_relay_list", "Background task: Publishing relay list for account: {:?}", account_clone.pubkey);

            let relays_urls = Relay::urls(&relays);
            let target_relays_urls = Relay::urls(&target_relays);

            nostr
                .publish_relay_list_with_signer(&relays_urls, relay_type, &target_relays_urls, keys)
                .await?;

            tracing::debug!(target: "whitenoise::accounts::background_publish_account_relay_list", "Successfully published relay list for account: {:?}", account_clone.pubkey);
            Ok::<(), WhitenoiseError>(())
        });
        Ok(())
    }

    pub(crate) async fn background_publish_account_follow_list(
        &self,
        account: &Account,
    ) -> Result<()> {
        let account_clone = account.clone();
        let nostr = self.nostr.clone();
        let relays = account.nip65_relays(self).await?;
        let keys = self
            .secrets_store
            .get_nostr_keys_for_pubkey(&account.pubkey)?;
        let follows = account.follows(&self.database).await?;
        let follows_pubkeys = follows.iter().map(|f| f.pubkey).collect::<Vec<_>>();

        tokio::spawn(async move {
            tracing::debug!(target: "whitenoise::accounts::background_publish_account_follow_list", "Background task: Publishing follow list for account: {:?}", account_clone.pubkey);

            let relays_urls = Relay::urls(&relays);
            nostr
                .publish_follow_list_with_signer(&follows_pubkeys, &relays_urls, keys)
                .await?;

            tracing::debug!(target: "whitenoise::accounts::background_publish_account_follow_list", "Successfully published follow list for account: {:?}", account_clone.pubkey);
            Ok::<(), WhitenoiseError>(())
        });
        Ok(())
    }

    /// Extract group data including relay URLs and group IDs for subscription setup.
    pub(crate) async fn extract_groups_relays_and_ids(
        &self,
        account: &Account,
    ) -> Result<(Vec<RelayUrl>, Vec<String>)> {
        let mdk = Account::create_mdk(account.pubkey, &self.config.data_dir)?;
        let groups = mdk.get_groups()?;
        let mut group_relays_set = HashSet::new();
        let mut group_ids = vec![];

        for group in &groups {
            let relays = mdk.get_relays(&group.mls_group_id)?;
            group_relays_set.extend(relays);
            group_ids.push(hex::encode(group.nostr_group_id));
        }

        let group_relays_urls = group_relays_set.into_iter().collect::<Vec<_>>();

        Ok((group_relays_urls, group_ids))
    }

    pub(crate) async fn setup_subscriptions(
        &self,
        account: &Account,
        nip65_relays: &[Relay],
        inbox_relays: &[Relay],
    ) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::setup_subscriptions",
            "Setting up subscriptions for account: {:?}",
            account
        );

        let user_relays: Vec<RelayUrl> = Relay::urls(nip65_relays);

        let inbox_relays: Vec<RelayUrl> = Relay::urls(inbox_relays);

        let (group_relays_urls, nostr_group_ids) =
            self.extract_groups_relays_and_ids(account).await?;

        // Ensure group relays are in the database
        for relay_url in &group_relays_urls {
            Relay::find_or_create_by_url(relay_url, &self.database).await?;
        }

        // Compute per-account since with a 10s lookback buffer when available
        let since = account.since_timestamp(10);
        match since {
            Some(ts) => tracing::debug!(
                target: "whitenoise::setup_subscriptions",
                "Computed per-account since={}s (10s buffer) for {}",
                ts.as_u64(),
                account.pubkey.to_hex()
            ),
            None => tracing::debug!(
                target: "whitenoise::setup_subscriptions",
                "Computed per-account since=None (unsynced) for {}",
                account.pubkey.to_hex()
            ),
        }

        // For external signer accounts, we can't get keys from the secret store.
        // Set up subscriptions without a signer - the signer is only needed for
        // decryption which will be handled separately for external signers.
        if account.uses_external_signer() {
            self.nostr
                .setup_account_subscriptions(
                    account.pubkey,
                    &user_relays,
                    &inbox_relays,
                    &group_relays_urls,
                    &nostr_group_ids,
                    since,
                )
                .await?;
        } else {
            let keys = self
                .secrets_store
                .get_nostr_keys_for_pubkey(&account.pubkey)?;

            self.nostr
                .setup_account_subscriptions_with_signer(
                    account.pubkey,
                    &user_relays,
                    &inbox_relays,
                    &group_relays_urls,
                    &nostr_group_ids,
                    since,
                    keys,
                )
                .await?;
        }

        tracing::debug!(
            target: "whitenoise::setup_subscriptions",
            "Subscriptions setup"
        );
        Ok(())
    }

    /// Refresh account subscriptions.
    ///
    /// This method updates subscriptions when account state changes (group membership, relay preferences).
    /// Uses explicit cleanup to handle relay changes properly - NIP-01 auto-replacement only works
    /// within the same relay, so changing relays would leave orphaned subscriptions without cleanup.
    ///
    /// # Arguments
    ///
    /// * `account` - The account to refresh subscriptions for
    pub(crate) async fn refresh_account_subscriptions(&self, account: &Account) -> Result<()> {
        tracing::debug!(
            target: "whitenoise::refresh_account_subscriptions",
            "Refreshing account subscriptions for account: {:?}",
            account.pubkey
        );

        let user_relays: Vec<RelayUrl> = Relay::urls(&account.nip65_relays(self).await?);

        let inbox_relays: Vec<RelayUrl> = Relay::urls(&account.inbox_relays(self).await?);

        let (group_relays_urls, nostr_group_ids) =
            self.extract_groups_relays_and_ids(account).await?;

        // For external signer accounts, we can't get keys from the secret store.
        if account.uses_external_signer() {
            self.nostr
                .update_account_subscriptions(
                    account.pubkey,
                    &user_relays,
                    &inbox_relays,
                    &group_relays_urls,
                    &nostr_group_ids,
                )
                .await
                .map_err(WhitenoiseError::from)
        } else {
            let keys = self
                .secrets_store
                .get_nostr_keys_for_pubkey(&account.pubkey)?;

            self.nostr
                .update_account_subscriptions_with_signer(
                    account.pubkey,
                    &user_relays,
                    &inbox_relays,
                    &group_relays_urls,
                    &nostr_group_ids,
                    keys,
                )
                .await
                .map_err(WhitenoiseError::from)
        }
    }
}

#[cfg(test)]
pub mod test_utils {
    use mdk_core::MDK;
    use mdk_sqlite_storage::MdkSqliteStorage;
    use nostr_sdk::PublicKey;
    use std::path::PathBuf;
    use tempfile::TempDir;

    pub fn data_dir() -> PathBuf {
        TempDir::new().unwrap().path().to_path_buf()
    }

    pub fn create_mdk(pubkey: PublicKey) -> MDK<MdkSqliteStorage> {
        super::Account::create_mdk(pubkey, &data_dir()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::accounts::Account;
    use crate::whitenoise::test_utils::*;
    use chrono::{TimeDelta, Utc};

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
        assert_eq!(ts.as_u64(), expected_secs);
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
        assert_eq!(ts.as_u64(), 0);
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

        let actual = ts.as_u64();
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
    async fn test_extract_groups_relays_and_ids_no_groups() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let (relays, group_ids) = whitenoise
            .extract_groups_relays_and_ids(&account)
            .await
            .unwrap();

        assert!(
            relays.is_empty(),
            "Should have no relays when account has no groups"
        );
        assert!(
            group_ids.is_empty(),
            "Should have no group IDs when account has no groups"
        );
    }

    #[tokio::test]
    async fn test_extract_groups_relays_and_ids_with_groups() {
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

        // Extract groups relays and IDs
        let (relays, group_ids) = whitenoise
            .extract_groups_relays_and_ids(&creator_account)
            .await
            .unwrap();

        // Verify relays were extracted
        assert!(!relays.is_empty(), "Should have relays from the group");
        assert!(
            relays.contains(&relay1),
            "Should contain relay1 from group config"
        );
        assert!(
            relays.contains(&relay2),
            "Should contain relay2 from group config"
        );

        // Verify group ID was extracted
        assert_eq!(group_ids.len(), 1, "Should have one group ID");
        assert_eq!(
            group_ids[0],
            hex::encode(group.nostr_group_id),
            "Group ID should match the created group"
        );
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
}
