use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

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
#[cfg(target_os = "android")]
use crate::whitenoise::signers::AmberSigner;
#[cfg(feature = "insecure-local-signer")]
use crate::whitenoise::signers::LocalSigner;
use crate::whitenoise::signers::{EphemeralSigner, SignerError, SignerKind};
use crate::whitenoise::users::User;
use crate::whitenoise::{Whitenoise, WhitenoiseError};

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

    #[error("Signer error: {0}")]
    SignerError(#[from] SignerError),

    #[error("Signer kind not found for account")]
    SignerKindNotFound,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: Option<i64>,
    pub pubkey: PublicKey,
    pub user_id: i64,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        Ok((account, keys))
    }

    /// Creates a new Account from a public key only (no private key needed).
    ///
    /// This is used for external signers like Amber where we don't have access
    /// to the private key.
    #[cfg(target_os = "android")]
    pub(crate) async fn new_from_pubkey(
        whitenoise: &Whitenoise,
        pubkey: PublicKey,
    ) -> Result<Account> {
        let (user, _created) =
            User::find_or_create_by_pubkey(&pubkey, &whitenoise.database).await?;

        let account = Account {
            id: None,
            user_id: user.id.unwrap(),
            pubkey,
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

    /// Gets the signer kind for this account.
    ///
    /// The signer kind determines how signing operations are performed for this account.
    /// It's stored in the secrets store alongside the account.
    ///
    /// # Arguments
    ///
    /// * `whitenoise` - The Whitenoise instance to access the secrets store
    ///
    /// # Returns
    ///
    /// The `SignerKind` for this account, or an error if not found.
    pub fn signer_kind(&self, whitenoise: &Whitenoise) -> Result<SignerKind> {
        whitenoise
            .secrets_store
            .get_signer_kind(&self.pubkey)
            .map_err(|_| WhitenoiseError::Account(AccountError::SignerKindNotFound))
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
        let signer = whitenoise.get_signer_for_account(self)?;
        let data = tokio::fs::read(file_path).await?;

        let descriptor = client
            .upload_blob(
                data,
                Some(image_type.mime_type().to_string()),
                None,
                Some(&signer),
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
    // ========================================================================
    // Account Creation (Local Signer - Insecure)
    // ========================================================================

    /// Creates a new identity (account) for the user using local key storage.
    ///
    /// **WARNING**: This method stores private keys locally, which is inherently insecure.
    /// On Android, use `login_with_amber()` instead for secure key management.
    ///
    /// This method generates a new keypair, sets up the account with default relay lists,
    /// creates a metadata event with a generated petname, and fully configures the account
    /// for use in Whitenoise.
    ///
    /// Only available when the `insecure-local-signer` feature is enabled.
    #[cfg(feature = "insecure-local-signer")]
    pub async fn create_identity(&self) -> Result<Account> {
        let keys = Keys::generate();
        tracing::debug!(target: "whitenoise::create_identity", "Generated new keypair: {}", keys.public_key().to_hex());

        let mut account = self
            .create_base_account_with_private_key(&keys, SignerKind::LocalInsecure)
            .await?;
        tracing::debug!(target: "whitenoise::create_identity", "Keys stored in secret store and account saved to database");

        let mut user = account.user(&self.database).await?;

        let relays = self
            .setup_relays_for_new_account(&mut account, &user)
            .await?;
        tracing::debug!(target: "whitenoise::create_identity", "Relays setup");

        self.activate_account(&account, &user, true, &relays, &relays, &relays)
            .await?;
        tracing::debug!(target: "whitenoise::create_identity", "Account persisted and activated");

        self.setup_metadata(&account, &mut user).await?;
        tracing::debug!(target: "whitenoise::create_identity", "Metadata setup");

        tracing::debug!(target: "whitenoise::create_identity", "Successfully created new identity: {}", account.pubkey.to_hex());
        Ok(account)
    }

    /// Logs in an existing user using a private key (nsec or hex format).
    ///
    /// **WARNING**: This method stores private keys locally, which is inherently insecure.
    /// On Android, use `login_with_amber()` instead for secure key management.
    ///
    /// This method parses the private key, checks if the account exists locally,
    /// and sets up the account for use. If the account doesn't exist locally,
    /// it treats it as an existing account and fetches data from the network.
    ///
    /// Only available when the `insecure-local-signer` feature is enabled.
    ///
    /// # Arguments
    ///
    /// * `nsec_or_hex_privkey` - The user's private key as a nsec string or hex-encoded string.
    #[cfg(feature = "insecure-local-signer")]
    pub async fn login(&self, nsec_or_hex_privkey: String) -> Result<Account> {
        let keys = Keys::parse(&nsec_or_hex_privkey)?;
        let pubkey = keys.public_key();
        tracing::debug!(target: "whitenoise::login", "Logging in with pubkey: {}", pubkey.to_hex());

        let mut account = self
            .create_base_account_with_private_key(&keys, SignerKind::LocalInsecure)
            .await?;
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

    // ========================================================================
    // Account Creation (Amber Signer - Android Only)
    // ========================================================================

    /// Logs in using Amber signer on Android.
    ///
    /// This method creates an account that delegates all signing operations to Amber,
    /// a dedicated NIP-55 signer app. The private key never enters this process.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The public key obtained from Amber
    ///
    /// # Platform
    ///
    /// This method is only available on Android. Call `Whitenoise::init_android_context()`
    /// before using this method.
    #[cfg(target_os = "android")]
    pub async fn login_with_amber(&self, pubkey: PublicKey) -> Result<Account> {
        tracing::debug!(target: "whitenoise::login_with_amber", "Logging in with Amber for pubkey: {}", pubkey.to_hex());

        let signer_kind = SignerKind::amber();
        let mut account = self
            .create_base_account_from_pubkey(pubkey, signer_kind)
            .await?;
        tracing::debug!(target: "whitenoise::login_with_amber", "Account created with Amber signer");

        // Check for existing relay lists
        let (nip65_relays, inbox_relays, key_package_relays) =
            self.setup_relays_for_existing_account(&mut account).await?;
        tracing::debug!(target: "whitenoise::login_with_amber", "Relays setup");

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
        tracing::debug!(target: "whitenoise::login_with_amber", "Account activated");

        tracing::debug!(target: "whitenoise::login_with_amber", "Successfully logged in with Amber: {}", account.pubkey.to_hex());
        Ok(account)
    }

    /// Logs in using Amber signer with a custom package name.
    ///
    /// This method is similar to `login_with_amber()` but allows specifying a custom
    /// signer app package name for non-standard Amber installations.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The public key obtained from Amber
    /// * `package_name` - The package name of the signer app
    #[cfg(target_os = "android")]
    pub async fn login_with_amber_custom(
        &self,
        pubkey: PublicKey,
        package_name: String,
    ) -> Result<Account> {
        tracing::debug!(
            target: "whitenoise::login_with_amber_custom",
            "Logging in with custom Amber ({}) for pubkey: {}",
            package_name,
            pubkey.to_hex()
        );

        let signer_kind = SignerKind::amber_with_package(package_name);
        let mut account = self
            .create_base_account_from_pubkey(pubkey, signer_kind)
            .await?;

        let (nip65_relays, inbox_relays, key_package_relays) =
            self.setup_relays_for_existing_account(&mut account).await?;

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

        tracing::debug!(target: "whitenoise::login_with_amber_custom", "Successfully logged in with custom Amber: {}", account.pubkey.to_hex());
        Ok(account)
    }

    /// Logs out the user associated with the given account.
    ///
    /// This method performs the following steps:
    /// - Removes the account from the database.
    /// - Removes the private key from the secret store (if stored locally).
    /// - Removes the signer kind from the secret store.
    /// - Updates the active account if the logged-out account was active.
    /// - Removes the account from the in-memory accounts list.
    ///
    /// - NB: This method does not remove the MLS database for the account. If the user logs back in, the MLS database will be re-initialized and used again.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The public key of the account to log out.
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

        // Remove the private key from the secret store (only applies for local signers)
        // This is a no-op when insecure-local-signer feature is disabled
        self.secrets_store.remove_private_key_for_pubkey(pubkey)?;

        // Remove the signer kind from the secret store
        if let Err(e) = self.secrets_store.remove_signer_kind(pubkey) {
            tracing::warn!(
                target: "whitenoise::logout",
                "Failed to remove signer kind for {}: {}",
                pubkey, e
            );
            // Don't fail logout if signer kind removal fails
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

    // ========================================================================
    // Signer Retrieval
    // ========================================================================

    /// Gets the appropriate signer for an account based on its stored SignerKind.
    ///
    /// This method returns an `Arc<dyn NostrSigner>` trait object that can be used for
    /// signing operations. The Arc wrapper allows the signer to be cloned and shared
    /// across async tasks.
    ///
    /// The actual signer implementation depends on the account's configured signer kind:
    ///
    /// - `SignerKind::LocalInsecure` - Returns a `LocalSigner` using locally stored keys
    /// - `SignerKind::Amber` - Returns an `AmberSigner` that delegates to the Amber app (Android only)
    /// - `SignerKind::Ephemeral` - Returns an `EphemeralSigner` (not typically used for accounts)
    ///
    /// # Arguments
    ///
    /// * `account` - The account to get a signer for
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The signer kind is not found for the account
    /// - The required feature is not enabled for the signer kind
    /// - The signer cannot be created (e.g., keys not found for LocalSigner)
    pub fn get_signer_for_account(&self, account: &Account) -> Result<Arc<dyn NostrSigner>> {
        let signer_kind = account.signer_kind(self)?;

        match signer_kind {
            #[cfg(feature = "insecure-local-signer")]
            SignerKind::LocalInsecure => {
                let signer = LocalSigner::from_secrets_store(&account.pubkey, &self.secrets_store)
                    .map_err(|e| WhitenoiseError::Account(AccountError::SignerError(e)))?;
                Ok(Arc::new(signer))
            }

            #[cfg(target_os = "android")]
            SignerKind::Amber { package_name } => {
                let signer = if package_name == crate::whitenoise::signers::AMBER_PACKAGE_NAME {
                    AmberSigner::new(account.pubkey)
                } else {
                    AmberSigner::with_package(account.pubkey, package_name)
                };
                Ok(Arc::new(signer))
            }

            SignerKind::Ephemeral => {
                // Ephemeral signers can't be restored - this shouldn't happen for persisted accounts
                tracing::error!(
                    target: "whitenoise::get_signer_for_account",
                    "Attempting to get signer for ephemeral account {}. Ephemeral signers cannot be restored.",
                    account.pubkey
                );
                Err(WhitenoiseError::Account(AccountError::SignerError(
                    SignerError::FeatureNotEnabled(
                        "Ephemeral signers cannot be restored for persisted accounts".to_string(),
                    ),
                )))
            }

            // Handle cases where feature/platform doesn't match the stored signer kind
            #[allow(unreachable_patterns)]
            _ => {
                tracing::error!(
                    target: "whitenoise::get_signer_for_account",
                    "Signer kind {:?} not supported on this platform/build",
                    signer_kind
                );
                Err(WhitenoiseError::Account(AccountError::SignerError(
                    SignerError::FeatureNotEnabled(format!("{:?}", signer_kind)),
                )))
            }
        }
    }

    /// Gets the signer for an account by public key.
    ///
    /// This is a convenience method that combines `find_account_by_pubkey` and
    /// `get_signer_for_account`.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The public key of the account
    pub async fn get_signer_for_pubkey(&self, pubkey: &PublicKey) -> Result<Arc<dyn NostrSigner>> {
        let account = self.find_account_by_pubkey(pubkey).await?;
        self.get_signer_for_account(&account)
    }

    // ========================================================================
    // Account Creation Helpers
    // ========================================================================

    #[cfg(feature = "insecure-local-signer")]
    async fn create_base_account_with_private_key(
        &self,
        keys: &Keys,
        signer_kind: SignerKind,
    ) -> Result<Account> {
        let (account, _keys) = Account::new(self, Some(keys.clone())).await?;

        self.secrets_store.store_private_key(keys).map_err(|e| {
            tracing::error!(target: "whitenoise::setup_account", "Failed to store private key: {}", e);
            e
        })?;

        // Store the signer kind for this account
        self.secrets_store
            .store_signer_kind(&account.pubkey, &signer_kind)
            .map_err(|e| {
                // Try to clean up stored private key on failure
                let _ = self.secrets_store.remove_private_key_for_pubkey(&account.pubkey);
                tracing::error!(target: "whitenoise::setup_account", "Failed to store signer kind: {}", e);
                WhitenoiseError::Other(anyhow::anyhow!(e))
            })?;

        let account = self.persist_account(&account).await?;

        Ok(account)
    }

    /// Creates a base account from a public key only (for external signers like Amber).
    ///
    /// This method creates an account without storing any private key material.
    /// The signer kind is stored to track how signing should be performed.
    #[cfg(target_os = "android")]
    async fn create_base_account_from_pubkey(
        &self,
        pubkey: PublicKey,
        signer_kind: SignerKind,
    ) -> Result<Account> {
        let account = Account::new_from_pubkey(self, pubkey).await?;

        // Store the signer kind for this account
        self.secrets_store
            .store_signer_kind(&account.pubkey, &signer_kind)
            .map_err(|e| {
                tracing::error!(target: "whitenoise::setup_account", "Failed to store signer kind: {}", e);
                WhitenoiseError::Other(anyhow::anyhow!(e))
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

    async fn setup_metadata(&self, account: &Account, user: &mut User) -> Result<()> {
        let petname = petname::petname(2, " ")
            .unwrap_or_else(|| "Anonymous User".to_string())
            .split_whitespace()
            .map(Whitenoise::capitalize_first_letter)
            .collect::<Vec<_>>()
            .join(" ");

        let metadata = Metadata {
            name: Some(petname.clone()),
            display_name: Some(petname),
            ..Default::default()
        };

        user.metadata = metadata.clone();
        user.save(&self.database).await?;
        self.background_publish_account_metadata(account).await?;

        let default_name = "Unknown".to_string();
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
        let signer = self.get_signer_for_account(account)?;

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
            self.publish_relay_list(
                &nip65_relays,
                RelayType::Nip65,
                &nip65_relays,
                signer.clone(),
            )
            .await?;
        }
        if should_publish_inbox {
            self.publish_relay_list(
                &inbox_relays,
                RelayType::Inbox,
                &nip65_relays,
                signer.clone(),
            )
            .await?;
        }
        if should_publish_key_package {
            self.publish_relay_list(
                &key_package_relays,
                RelayType::KeyPackage,
                &nip65_relays,
                signer.clone(),
            )
            .await?;
        }

        Ok((nip65_relays, inbox_relays, key_package_relays))
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
        signer: Arc<dyn NostrSigner>,
    ) -> Result<()> {
        let relays_urls = Relay::urls(relays);
        let target_relays_urls = Relay::urls(target_relays);
        self.nostr
            .publish_relay_list_with_signer(&relays_urls, relay_type, &target_relays_urls, signer)
            .await?;
        Ok(())
    }

    pub(crate) async fn background_publish_account_metadata(
        &self,
        account: &Account,
    ) -> Result<()> {
        let account_clone = account.clone();
        let nostr = self.nostr.clone();
        let signer = self.get_signer_for_account(account)?;
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
        let signer = self.get_signer_for_account(account)?;
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
                .publish_relay_list_with_signer(
                    &relays_urls,
                    relay_type,
                    &target_relays_urls,
                    signer,
                )
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
        let signer = self.get_signer_for_account(account)?;
        let follows = account.follows(&self.database).await?;
        let follows_pubkeys = follows.iter().map(|f| f.pubkey).collect::<Vec<_>>();

        tokio::spawn(async move {
            tracing::debug!(target: "whitenoise::accounts::background_publish_account_follow_list", "Background task: Publishing follow list for account: {:?}", account_clone.pubkey);

            let relays_urls = Relay::urls(&relays);
            match nostr
                .publish_follow_list_with_signer(&follows_pubkeys, &relays_urls, signer)
                .await
            {
                Ok(_) => {
                    tracing::debug!(target: "whitenoise::accounts::background_publish_account_follow_list", "Successfully published follow list for account: {:?}", account_clone.pubkey);
                }
                Err(e) => {
                    tracing::error!(target: "whitenoise::accounts::background_publish_account_follow_list", "Failed to publish follow list for account {:?}: {}", account_clone.pubkey, e);
                }
            }
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

        let signer = self.get_signer_for_account(account)?;

        self.nostr
            .setup_account_subscriptions_with_signer(
                account.pubkey,
                &user_relays,
                &inbox_relays,
                &group_relays_urls,
                &nostr_group_ids,
                since,
                signer,
            )
            .await?;

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

        let signer = self.get_signer_for_account(account)?;

        self.nostr
            .update_account_subscriptions_with_signer(
                account.pubkey,
                &user_relays,
                &inbox_relays,
                &group_relays_urls,
                &nostr_group_ids,
                signer,
            )
            .await
            .map_err(WhitenoiseError::from)
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
    #[cfg(feature = "insecure-local-signer")]
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
        whitenoise
            .secrets_store
            .store_signer_kind(
                &account1.pubkey,
                &crate::whitenoise::signers::SignerKind::LocalInsecure,
            )
            .unwrap();
        whitenoise
            .secrets_store
            .store_signer_kind(
                &account2.pubkey,
                &crate::whitenoise::signers::SignerKind::LocalInsecure,
            )
            .unwrap();

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
    #[cfg(feature = "insecure-local-signer")]
    async fn test_update_metadata() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_test_account(&whitenoise).await;
        account.save(&whitenoise.database).await.unwrap();

        whitenoise.secrets_store.store_private_key(&keys).unwrap();
        whitenoise
            .secrets_store
            .store_signer_kind(
                &account.pubkey,
                &crate::whitenoise::signers::SignerKind::LocalInsecure,
            )
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
}
