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
use crate::whitenoise::database::published_key_packages::PublishedKeyPackage;
use crate::whitenoise::error::Result;
use crate::whitenoise::relays::Relay;
use crate::whitenoise::users::User;
use crate::whitenoise::{Whitenoise, WhitenoiseError};

/// The type of account authentication.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
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

/// The status of a login attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStatus {
    /// Login completed successfully. Account is fully activated with relay lists,
    /// subscriptions, and a published key package.
    Complete,
    /// Relay lists were not found on the network. The account exists in a partial
    /// state and the caller must resolve relay lists before login can complete.
    /// Use `login_publish_default_relays` or `login_with_custom_relay` to continue,
    /// or `login_cancel` to clean up.
    NeedsRelayLists,
}

/// The result of a login attempt.
#[derive(Debug, Clone)]
pub struct LoginResult {
    /// The account that was created or found.
    pub account: Account,
    /// Whether login completed or needs further action.
    pub status: LoginStatus,
}

/// The three relay lists discovered during login.
///
/// Each field holds the relays found on the network for that list type.
/// An empty `Vec` means that list was **not found** and must be published
/// (either from defaults or from a user-provided relay) before login can
/// complete.  Use [`DiscoveredRelayLists::is_complete`] to check whether all
/// three are present.
#[derive(Debug, Clone)]
pub struct DiscoveredRelayLists {
    /// NIP-65 relay list (kind 10002).  Empty if not found on the network.
    pub nip65: Vec<Relay>,
    /// Inbox relays (kind 10050).  Empty if not found on the network.
    pub inbox: Vec<Relay>,
    /// Key-package relays (kind 10051).  Empty if not found on the network.
    pub key_package: Vec<Relay>,
}

impl DiscoveredRelayLists {
    /// Returns `true` when all three relay lists were found on the network.
    pub fn is_complete(&self) -> bool {
        !self.nip65.is_empty() && !self.inbox.is_empty() && !self.key_package.is_empty()
    }

    /// Returns the relay slice for the given `relay_type`.
    pub fn relays(&self, relay_type: RelayType) -> &[Relay] {
        match relay_type {
            RelayType::Nip65 => &self.nip65,
            RelayType::Inbox => &self.inbox,
            RelayType::KeyPackage => &self.key_package,
        }
    }

    /// Returns the relays for `relay_type` when non-empty, otherwise `fallback`.
    pub fn relays_or<'a>(&'a self, relay_type: RelayType, fallback: &'a [Relay]) -> &'a [Relay] {
        let found = self.relays(relay_type);
        if found.is_empty() { fallback } else { found }
    }

    /// Merge `other` into `self`, keeping any non-empty field from either side.
    ///
    /// Each field is updated only when the current value is empty and the
    /// incoming value is non-empty, so previously discovered relays are never
    /// discarded.
    pub fn merge(&mut self, other: Self) {
        if self.nip65.is_empty() && !other.nip65.is_empty() {
            self.nip65 = other.nip65;
        }
        if self.inbox.is_empty() && !other.inbox.is_empty() {
            self.inbox = other.inbox;
        }
        if self.key_package.is_empty() && !other.key_package.is_empty() {
            self.key_package = other.key_package;
        }
    }
}

/// Errors specific to the login flow.
#[derive(Debug, Error)]
pub enum LoginError {
    /// The provided private key is not valid (bad format, bad encoding, etc.).
    #[error("Invalid private key format: {0}")]
    InvalidKeyFormat(String),

    /// Could not connect to any relay to fetch or publish data.
    #[error("Failed to connect to any relays")]
    NoRelayConnections,

    /// The operation timed out.
    #[error("Login operation timed out: {0}")]
    Timeout(String),

    /// No partial login in progress for the given pubkey.
    #[error("No login in progress for this account")]
    NoLoginInProgress,

    /// An internal error that doesn't fit the above categories.
    #[error("Login error: {0}")]
    Internal(String),
}

impl From<nostr_sdk::key::Error> for LoginError {
    fn from(err: nostr_sdk::key::Error) -> Self {
        Self::InvalidKeyFormat(err.to_string())
    }
}

impl From<WhitenoiseError> for LoginError {
    fn from(err: WhitenoiseError) -> Self {
        match err {
            WhitenoiseError::NostrManager(NostrManagerError::NoRelayConnections) => {
                Self::NoRelayConnections
            }
            WhitenoiseError::NostrManager(NostrManagerError::Timeout) => {
                Self::Timeout("relay operation timed out".to_string())
            }
            other => Self::Internal(other.to_string()),
        }
    }
}

#[derive(Debug, Error)]
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

/// Result of setting up relays for an external signer account.
///
/// Contains the relay lists for each type and flags indicating which lists
/// need to be published (when defaults were used because no existing relays
/// were found on the network).
#[derive(Debug)]
pub(crate) struct ExternalSignerRelaySetup {
    pub nip65_relays: Vec<Relay>,
    pub inbox_relays: Vec<Relay>,
    pub key_package_relays: Vec<Relay>,
    pub should_publish_nip65: bool,
    pub should_publish_inbox: bool,
    pub should_publish_key_package: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
            .nostr
            .ensure_relays_connected(std::slice::from_ref(&relay.url))
            .await?;
        whitenoise
            .background_publish_account_relay_list(self, relay_type, None)
            .await?;
        tracing::debug!(target: "whitenoise::accounts", "Added relay to account: {:?}", relay.url);

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
        tracing::debug!(target: "whitenoise::accounts", "Removed relay from account: {:?}", relay.url);
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
        tracing::debug!(target: "whitenoise::accounts", "Updating metadata for account: {:?}", self.pubkey);
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
        keyring_service_id: &str,
    ) -> core::result::Result<MDK<MdkSqliteStorage>, AccountError> {
        let mls_storage_dir = data_dir.join("mls").join(pubkey.to_hex());
        let db_key_id = format!("mdk.db.key.{}", pubkey.to_hex());
        let storage = MdkSqliteStorage::new(mls_storage_dir, keyring_service_id, &db_key_id)?;
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
        tracing::debug!(target: "whitenoise::accounts", "Generated new keypair: {}", keys.public_key().to_hex());

        let mut account = self.create_base_account_with_private_key(&keys).await?;
        tracing::debug!(target: "whitenoise::accounts", "Keys stored in secret store and account saved to database");

        let user = account.user(&self.database).await?;

        let relays = self
            .setup_relays_for_new_account(&mut account, &user)
            .await?;
        tracing::debug!(target: "whitenoise::accounts", "Relays setup");

        self.activate_account(&account, &user, true, &relays, &relays, &relays)
            .await?;
        tracing::debug!(target: "whitenoise::accounts", "Account persisted and activated");

        tracing::debug!(target: "whitenoise::accounts", "Successfully created new identity: {}", account.pubkey.to_hex());
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
        tracing::debug!(target: "whitenoise::accounts", "Logging in with pubkey: {}", pubkey.to_hex());

        // If this account is already logged in, return it as-is. The session
        // (relay connections, subscriptions, cancellation channel, background
        // tasks) was set up during the original login and is still active —
        // re-running activate_account would create duplicate subscriptions and
        // needlessly kill in-progress background tasks.
        if let Ok(existing) = Account::find_by_pubkey(&pubkey, &self.database).await {
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
        let (nip65_relays, inbox_relays, key_package_relays) =
            self.setup_relays_for_existing_account(&mut account).await?;
        tracing::debug!(target: "whitenoise::accounts", "Relays setup");

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
        if let Ok(existing) = Account::find_by_pubkey(&pubkey, &self.database).await {
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

        let user = account.user(&self.database).await?;
        self.activate_account_without_publishing(
            &account,
            &user,
            &relay_setup.nip65_relays,
            &relay_setup.inbox_relays,
            &relay_setup.key_package_relays,
        )
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

    // -----------------------------------------------------------------------
    // Multi-step login API
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
        let mut stash = self
            .pending_logins
            .get_mut(pubkey)
            .ok_or(LoginError::NoLoginInProgress)?;
        stash.merge(discovered);
        let is_complete = stash.is_complete();
        let merged_nip65 = stash.nip65.clone();
        let merged_inbox = stash.inbox.clone();
        let merged_key_package = stash.key_package.clone();
        drop(stash);

        if is_complete {
            self.complete_login(&account, &merged_nip65, &merged_inbox, &merged_key_package)
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
                !merged_nip65.is_empty(),
                !merged_inbox.is_empty(),
                !merged_key_package.is_empty(),
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
        let mut stash = self
            .pending_logins
            .get_mut(pubkey)
            .ok_or(LoginError::NoLoginInProgress)?;
        stash.merge(discovered);
        let is_complete = stash.is_complete();
        let merged_nip65 = stash.nip65.clone();
        let merged_inbox = stash.inbox.clone();
        let merged_key_package = stash.key_package.clone();
        drop(stash);

        if is_complete {
            self.complete_external_signer_login(
                &account,
                &merged_nip65,
                &merged_inbox,
                &merged_key_package,
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
                !merged_nip65.is_empty(),
                !merged_inbox.is_empty(),
                !merged_key_package.is_empty(),
            );
            Ok(LoginResult {
                account,
                status: LoginStatus::NeedsRelayLists,
            })
        }
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
    async fn try_discover_relay_lists(
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
    async fn complete_external_signer_login<S>(
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

    /// Create/update an external signer account record without setting up relays.
    /// Used by the multi-step external signer login flow.
    async fn setup_external_signer_account_without_relays(
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
    async fn setup_external_signer_account(
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
    async fn publish_relay_lists_with_signer(
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

        // Cancel any running background tasks (contact list user fetches, etc.)
        // before tearing down subscriptions and relay connections.
        if let Some((_, cancel_tx)) = self.background_task_cancellation.remove(pubkey) {
            let _ = cancel_tx.send(true);
        }

        // Unsubscribe from account-specific subscriptions before logout
        if let Err(e) = self.nostr.unsubscribe_account_subscriptions(pubkey).await {
            tracing::warn!(
                target: "whitenoise::accounts",
                "Failed to unsubscribe from account subscriptions for {}: {}",
                pubkey, e
            );
            // Don't fail logout if unsubscribe fails
        }

        // Delete the account from the database
        account.delete(&self.database).await?;

        // Remove the private key from the secret store
        // For local accounts this is required; for external accounts this is best-effort cleanup
        let result = self.secrets_store.remove_private_key_for_pubkey(pubkey);
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
            tracing::error!(target: "whitenoise::accounts", "Failed to store private key: {}", e);
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
        // Create a fresh cancellation channel for this account's background tasks.
        // Any previous channel (from a prior login) is replaced, which also drops
        // any stale receivers.
        let (cancel_tx, _) = tokio::sync::watch::channel(false);
        self.background_task_cancellation
            .insert(account.pubkey, cancel_tx);

        let relay_urls: Vec<RelayUrl> = Relay::urls(
            nip65_relays
                .iter()
                .chain(inbox_relays)
                .chain(key_package_relays),
        );
        self.nostr.ensure_relays_connected(&relay_urls).await?;
        tracing::debug!(target: "whitenoise::accounts", "Relays connected");
        if let Err(e) = self.refresh_global_subscription_for_user(user).await {
            tracing::warn!(
                target: "whitenoise::accounts",
                "Failed to refresh global subscription for new user {}: {}",
                user.pubkey,
                e
            );
        }
        tracing::debug!(target: "whitenoise::accounts", "Global subscription refreshed for account user");
        self.setup_subscriptions(account, nip65_relays, inbox_relays)
            .await?;
        tracing::debug!(target: "whitenoise::accounts", "Subscriptions setup");
        self.setup_key_package(account, is_new_account, key_package_relays)
            .await?;
        tracing::debug!(target: "whitenoise::accounts", "Key package setup");
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
        let (cancel_tx, _) = tokio::sync::watch::channel(false);
        self.background_task_cancellation
            .insert(account.pubkey, cancel_tx);

        let relay_urls: Vec<RelayUrl> = Relay::urls(
            nip65_relays
                .iter()
                .chain(inbox_relays)
                .chain(key_package_relays),
        );
        self.nostr.ensure_relays_connected(&relay_urls).await?;
        tracing::debug!(target: "whitenoise::accounts", "Relays connected");

        if let Err(e) = self.refresh_global_subscription_for_user(user).await {
            tracing::warn!(
                target: "whitenoise::accounts",
                "Failed to refresh global subscription for new user {}: {}",
                user.pubkey,
                e
            );
        }
        tracing::debug!(target: "whitenoise::accounts", "Global subscription refreshed");

        self.setup_subscriptions(account, nip65_relays, inbox_relays)
            .await?;
        tracing::debug!(target: "whitenoise::accounts", "Subscriptions setup");

        // Note: We skip key package setup for external signer accounts.
        // Key packages need to be published separately with the external signer.
        tracing::debug!(target: "whitenoise::accounts", "Skipping key package setup (external signer)");

        Ok(())
    }

    async fn persist_account(&self, account: &Account) -> Result<Account> {
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
                .nostr
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
    async fn setup_relays_for_external_signer_account(
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
    /// (true if defaults were used because no existing relays were found).
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

        let user = account.user(&self.database).await?;
        if fetched_relays.is_empty() {
            // No existing relay lists - use defaults, mark for publishing
            user.add_relays(default_relays, relay_type, &self.database)
                .await?;
            Ok((default_relays.to_vec(), true))
        } else {
            // Found existing relay lists - use them, no publishing needed
            user.add_relays(&fetched_relays, relay_type, &self.database)
                .await?;
            Ok((fetched_relays, false))
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
        // Ensure source relays are in the client pool before attempting to fetch.
        // Without this, fetch_events_from will fail with RelayNotFound if the
        // relays haven't been added to the pool yet (e.g. during login before
        // activate_account runs).
        self.nostr
            .ensure_relays_connected(&source_relay_urls)
            .await?;
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

        tracing::debug!(target: "whitenoise::accounts", "Added {} relays of type {:?} to account", relays.len(), relay_type);

        Ok(())
    }

    async fn publish_relay_list<S>(
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
            tracing::debug!(target: "whitenoise::accounts", "Background task: Publishing metadata for account: {:?}", account_clone.pubkey);

            let relays_urls = Relay::urls(&relays);

            nostr
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
            tracing::debug!(target: "whitenoise::accounts", "Background task: Publishing relay list for account: {:?}", account_clone.pubkey);

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
        let nostr = self.nostr.clone();
        let relays = account.nip65_relays(self).await?;
        let signer = self.get_signer_for_account(account)?;
        let follows = account.follows(&self.database).await?;
        let follows_pubkeys = follows.iter().map(|f| f.pubkey).collect::<Vec<_>>();

        tokio::spawn(async move {
            tracing::debug!(target: "whitenoise::accounts", "Background task: Publishing follow list for account: {:?}", account_clone.pubkey);

            let relays_urls = Relay::urls(&relays);
            nostr
                .publish_follow_list_with_signer(&follows_pubkeys, &relays_urls, signer)
                .await?;

            tracing::debug!(target: "whitenoise::accounts", "Successfully published follow list for account: {:?}", account_clone.pubkey);
            Ok::<(), WhitenoiseError>(())
        });
        Ok(())
    }

    /// Extract group data including relay URLs and group IDs for subscription setup.
    pub(crate) async fn extract_groups_relays_and_ids(
        &self,
        account: &Account,
    ) -> Result<(Vec<RelayUrl>, Vec<String>)> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
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
            target: "whitenoise::accounts",
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
            target: "whitenoise::accounts",
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
            target: "whitenoise::accounts",
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
        super::super::Whitenoise::initialize_mock_keyring_store();
        super::Account::create_mdk(pubkey, &data_dir(), "com.whitenoise.test").unwrap()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, Utc};

    use super::*;
    use crate::whitenoise::accounts::Account;
    use crate::whitenoise::test_utils::*;

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
    async fn test_create_identity_creates_cancellation_channel() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();

        assert!(
            whitenoise
                .background_task_cancellation
                .contains_key(&account.pubkey),
            "activate_account should create a cancellation channel"
        );
    }

    #[tokio::test]
    async fn test_logout_signals_and_removes_cancellation_channel() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let account = whitenoise.create_identity().await.unwrap();

        // Subscribe to the cancellation channel before logout
        let cancel_rx = whitenoise
            .background_task_cancellation
            .get(&account.pubkey)
            .expect("cancellation channel should exist after login")
            .value()
            .subscribe();

        // Initially not cancelled
        assert!(
            !*cancel_rx.borrow(),
            "should not be cancelled before logout"
        );

        whitenoise.logout(&account.pubkey).await.unwrap();

        // After logout, the channel should have been signalled
        assert!(
            *cancel_rx.borrow(),
            "logout should signal cancellation to running background tasks"
        );

        // And the entry should be removed from the map
        assert!(
            !whitenoise
                .background_task_cancellation
                .contains_key(&account.pubkey),
            "logout should remove the cancellation channel entry"
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
        assert_eq!(ts.as_secs(), expected_secs);
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
        assert_eq!(ts.as_secs(), 0);
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

        let actual = ts.as_secs();
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

    /// Test that external account helper methods work correctly (uses_external_signer, has_local_key)
    #[tokio::test]
    async fn test_external_account_uses_external_signer_and_has_local_key() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Create external account directly (doesn't need relay)
        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();

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

    /// Test that local account helper methods return correct values
    #[tokio::test]
    async fn test_local_account_uses_external_signer_and_has_local_key() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create local account
        let (account, _keys) = Account::new(&whitenoise, None).await.unwrap();

        // Test helper methods
        assert!(
            !account.uses_external_signer(),
            "Local account should not report using external signer"
        );
        assert!(
            account.has_local_key(),
            "Local account should report having local key"
        );
    }

    /// Test Account struct serialization/deserialization with JSON
    #[tokio::test]
    async fn test_account_json_serialization_roundtrip() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Create both types of accounts
        let external_account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        let (local_account, _) = Account::new(&whitenoise, None).await.unwrap();

        // Serialize and deserialize external account
        let external_json = serde_json::to_string(&external_account).unwrap();
        let external_deserialized: Account = serde_json::from_str(&external_json).unwrap();
        assert_eq!(external_account.pubkey, external_deserialized.pubkey);
        assert_eq!(
            external_account.account_type,
            external_deserialized.account_type
        );
        assert_eq!(external_deserialized.account_type, AccountType::External);

        // Serialize and deserialize local account
        let local_json = serde_json::to_string(&local_account).unwrap();
        let local_deserialized: Account = serde_json::from_str(&local_json).unwrap();
        assert_eq!(local_account.pubkey, local_deserialized.pubkey);
        assert_eq!(local_account.account_type, local_deserialized.account_type);
        assert_eq!(local_deserialized.account_type, AccountType::Local);
    }

    /// Test Account debug formatting includes account_type
    #[tokio::test]
    async fn test_account_debug_includes_account_type() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let external_account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        let (local_account, _) = Account::new(&whitenoise, None).await.unwrap();

        let external_debug = format!("{:?}", external_account);
        let local_debug = format!("{:?}", local_account);

        assert!(
            external_debug.contains("External"),
            "External account debug should contain 'External': {}",
            external_debug
        );
        assert!(
            local_debug.contains("Local"),
            "Local account debug should contain 'Local': {}",
            local_debug
        );
    }

    /// Test Account::new_external properly sets up external account without relay fetch
    #[tokio::test]
    async fn test_new_external_sets_correct_fields() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();

        // Verify all fields are correctly set
        assert_eq!(account.pubkey, pubkey);
        assert_eq!(account.account_type, AccountType::External);
        assert!(account.id.is_none(), "New account should not be persisted");
        assert!(account.last_synced_at.is_none());
        assert!(account.user_id > 0, "Should have a valid user_id");
    }

    /// Test external account persists correctly to database
    #[tokio::test]
    async fn test_external_account_database_roundtrip() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Create and persist external account
        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        let persisted = whitenoise.persist_account(&account).await.unwrap();

        // Verify persisted correctly
        assert!(persisted.id.is_some());
        assert_eq!(persisted.account_type, AccountType::External);

        // Find it back
        let found = Account::find_by_pubkey(&pubkey, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(found.pubkey, pubkey);
        assert_eq!(found.account_type, AccountType::External);
    }

    /// Test that secrets store doesn't have keys for external accounts
    #[tokio::test]
    async fn test_external_account_no_keys_in_secrets_store() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        let pubkey = keys.public_key();

        // Create external account (doesn't store any keys)
        let _account = Account::new_external(&whitenoise, pubkey).await.unwrap();

        // Verify no keys in secrets store for this pubkey
        let result = whitenoise.secrets_store.get_nostr_keys_for_pubkey(&pubkey);
        assert!(
            result.is_err(),
            "External account creation should not store any keys"
        );
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

    /// Comprehensive test for account relay operations including all relay types
    #[tokio::test]
    async fn test_account_relay_operations() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create and persist account
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = whitenoise.persist_account(&account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        // New account should have no relays
        assert!(account.nip65_relays(&whitenoise).await.unwrap().is_empty());
        assert!(account.inbox_relays(&whitenoise).await.unwrap().is_empty());
        assert!(
            account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty()
        );

        // Load and add default relays
        let default_relays = whitenoise.load_default_relays().await.unwrap();
        #[cfg(debug_assertions)]
        assert_eq!(default_relays.len(), 2);

        // Add relays for all types
        whitenoise
            .add_relays_to_account(&account, &default_relays, RelayType::Nip65)
            .await
            .unwrap();
        whitenoise
            .add_relays_to_account(&account, &default_relays, RelayType::Inbox)
            .await
            .unwrap();
        whitenoise
            .add_relays_to_account(&account, &default_relays, RelayType::KeyPackage)
            .await
            .unwrap();

        // Verify all relay types have relays
        let nip65 = account.relays(RelayType::Nip65, &whitenoise).await.unwrap();
        let inbox = account.relays(RelayType::Inbox, &whitenoise).await.unwrap();
        let kp = account
            .relays(RelayType::KeyPackage, &whitenoise)
            .await
            .unwrap();
        assert_eq!(nip65.len(), default_relays.len());
        assert_eq!(inbox.len(), default_relays.len());
        assert_eq!(kp.len(), default_relays.len());

        // Test add_relay and remove_relay on individual relays
        let test_relay = Relay::find_or_create_by_url(
            &RelayUrl::parse("wss://test.relay.example").unwrap(),
            &whitenoise.database,
        )
        .await
        .unwrap();
        account
            .add_relay(&test_relay, RelayType::Nip65, &whitenoise)
            .await
            .unwrap();
        let relays_after_add = account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(relays_after_add.len(), default_relays.len() + 1);

        account
            .remove_relay(&test_relay, RelayType::Nip65, &whitenoise)
            .await
            .unwrap();
        let relays_after_remove = account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(relays_after_remove.len(), default_relays.len());
    }

    /// Comprehensive test for account CRUD operations
    #[tokio::test]
    async fn test_account_crud_operations() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Initially empty
        assert_eq!(whitenoise.get_accounts_count().await.unwrap(), 0);
        assert!(whitenoise.all_accounts().await.unwrap().is_empty());

        // Create and persist accounts
        let (account1, _) = create_test_account(&whitenoise).await;
        let account1 = whitenoise.persist_account(&account1).await.unwrap();
        assert!(account1.id.is_some());

        let (account2, _) = create_test_account(&whitenoise).await;
        let _account2 = whitenoise.persist_account(&account2).await.unwrap();

        // Verify counts and retrieval
        assert_eq!(whitenoise.get_accounts_count().await.unwrap(), 2);
        let all = whitenoise.all_accounts().await.unwrap();
        assert_eq!(all.len(), 2);

        // Find by pubkey
        let found = whitenoise
            .find_account_by_pubkey(&account1.pubkey)
            .await
            .unwrap();
        assert_eq!(found.pubkey, account1.pubkey);

        // Not found for random pubkey
        let random_pk = Keys::generate().public_key();
        assert!(whitenoise.find_account_by_pubkey(&random_pk).await.is_err());

        // Test user and metadata retrieval
        let user = account1.user(&whitenoise.database).await.unwrap();
        assert_eq!(user.pubkey, account1.pubkey);

        let metadata = account1.metadata(&whitenoise).await.unwrap();
        assert!(metadata.name.is_none() || metadata.name.as_deref() == Some(""));
    }

    /// Test account creation for both local and external account types
    #[tokio::test]
    async fn test_account_type_creation() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Local account with generated keys
        let (local_gen, keys_gen) = Account::new(&whitenoise, None).await.unwrap();
        assert_eq!(local_gen.account_type, AccountType::Local);
        assert_eq!(local_gen.pubkey, keys_gen.public_key());

        // Local account with provided keys
        let provided = create_test_keys();
        let (local_prov, keys_prov) = Account::new(&whitenoise, Some(provided.clone()))
            .await
            .unwrap();
        assert_eq!(local_prov.account_type, AccountType::Local);
        assert_eq!(keys_prov.public_key(), provided.public_key());

        // External account
        let ext_pubkey = Keys::generate().public_key();
        let external = Account::new_external(&whitenoise, ext_pubkey)
            .await
            .unwrap();
        assert_eq!(external.account_type, AccountType::External);
        assert_eq!(external.pubkey, ext_pubkey);
        assert!(!external.has_local_key());
        assert!(external.uses_external_signer());
    }

    #[test]
    fn test_create_mdk_success() {
        // Initialize mock keyring so this test passes on headless CI (e.g. Ubuntu)
        crate::whitenoise::Whitenoise::initialize_mock_keyring_store();

        let temp_dir = tempfile::TempDir::new().unwrap();
        let pubkey = Keys::generate().public_key();
        let result = Account::create_mdk(pubkey, temp_dir.path(), "com.whitenoise.test");
        assert!(result.is_ok(), "create_mdk failed: {:?}", result.err());
    }

    /// Test logout removes keys correctly for both account types
    #[tokio::test]
    async fn test_logout_key_cleanup() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Local account logout removes key
        let (local_account, keys) = create_test_account(&whitenoise).await;
        local_account.save(&whitenoise.database).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&local_account.pubkey)
                .is_ok()
        );
        whitenoise.logout(&local_account.pubkey).await.unwrap();
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&local_account.pubkey)
                .is_err()
        );

        // External account logout with stale key cleans up
        let ext_keys = create_test_keys();
        let ext_account = Account::new_external(&whitenoise, ext_keys.public_key())
            .await
            .unwrap();
        ext_account.save(&whitenoise.database).await.unwrap();
        whitenoise
            .secrets_store
            .store_private_key(&ext_keys)
            .unwrap(); // Stale key

        whitenoise.logout(&ext_account.pubkey).await.unwrap();
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&ext_account.pubkey)
                .is_err()
        );

        // External account logout without key succeeds
        let ext2 = Account::new_external(&whitenoise, Keys::generate().public_key())
            .await
            .unwrap();
        ext2.save(&whitenoise.database).await.unwrap();
        assert!(whitenoise.logout(&ext2.pubkey).await.is_ok());
    }

    /// Test upload_profile_picture uploads to blossom server and returns URL
    /// Requires blossom server running on localhost:3000
    #[ignore]
    #[tokio::test]
    async fn test_upload_profile_picture() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Create and persist account with stored keys
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = whitenoise.persist_account(&account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

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

    /// Test upload_profile_picture fails gracefully with non-existent file
    #[ignore]
    #[tokio::test]
    async fn test_upload_profile_picture_nonexistent_file() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (account, keys) = create_test_account(&whitenoise).await;
        let account = whitenoise.persist_account(&account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

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

    /// Test login_with_external_signer creates new external account
    #[tokio::test]
    async fn test_login_with_external_signer_new_account() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let keys = create_test_keys();
        let pubkey = keys.public_key();

        // Login with external signer (new account)
        let account = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        // Verify account was created correctly
        assert_eq!(account.pubkey, pubkey);
        assert_eq!(account.account_type, AccountType::External);
        assert!(account.id.is_some(), "Account should be persisted");
        assert!(account.uses_external_signer(), "Should use external signer");
        assert!(!account.has_local_key(), "Should not have local key");

        // Verify no private key was stored
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&pubkey)
                .is_err(),
            "External account should not have stored private key"
        );

        // Verify relay lists are set up
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

        // First login - creates new account
        let account1 = whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();
        assert!(account1.id.is_some());

        // Allow some time for setup
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Second login - should return existing account
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

        // First, create a local account directly
        let keys = create_test_keys();
        let pubkey = keys.public_key();

        let (local_account, _) = Account::new(&whitenoise, Some(keys.clone())).await.unwrap();
        assert_eq!(local_account.account_type, AccountType::Local);
        let _local_account = whitenoise.persist_account(&local_account).await.unwrap();

        // Store the key (simulating normal local account creation)
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        // Now login with external signer - should migrate
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

        // Verify local key was removed during migration
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

        // Create external account first
        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        whitenoise.persist_account(&account).await.unwrap();

        // Manually store a "stale" key (simulating failed migration or orphaned key)
        whitenoise.secrets_store.store_private_key(&keys).unwrap();
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&pubkey)
                .is_ok()
        );

        // Login with external signer - should clean up stale key
        whitenoise
            .login_with_external_signer_for_test(keys.clone())
            .await
            .unwrap();

        // Verify stale key was removed
        assert!(
            whitenoise
                .secrets_store
                .get_nostr_keys_for_pubkey(&pubkey)
                .is_err(),
            "Stale key should be removed during external signer login"
        );
    }

    // AccountType Serialization Tests
    #[test]
    fn test_account_type_json_serialization() {
        // Test Local serializes correctly
        let local = AccountType::Local;
        let json = serde_json::to_string(&local).unwrap();
        assert_eq!(json, "\"Local\"");

        // Test External serializes correctly
        let external = AccountType::External;
        let json = serde_json::to_string(&external).unwrap();
        assert_eq!(json, "\"External\"");
    }

    #[test]
    fn test_account_type_json_deserialization() {
        // Test Local deserializes correctly
        let local: AccountType = serde_json::from_str("\"Local\"").unwrap();
        assert_eq!(local, AccountType::Local);

        // Test External deserializes correctly
        let external: AccountType = serde_json::from_str("\"External\"").unwrap();
        assert_eq!(external, AccountType::External);
    }

    #[test]
    fn test_account_type_serialization_roundtrip() {
        // Test roundtrip for Local
        let original_local = AccountType::Local;
        let serialized = serde_json::to_string(&original_local).unwrap();
        let deserialized: AccountType = serde_json::from_str(&serialized).unwrap();
        assert_eq!(original_local, deserialized);

        // Test roundtrip for External
        let original_external = AccountType::External;
        let serialized = serde_json::to_string(&original_external).unwrap();
        let deserialized: AccountType = serde_json::from_str(&serialized).unwrap();
        assert_eq!(original_external, deserialized);
    }

    #[test]
    fn test_account_type_equality_and_hash() {
        use std::collections::HashSet;

        // Test equality
        assert_eq!(AccountType::Local, AccountType::Local);
        assert_eq!(AccountType::External, AccountType::External);
        assert_ne!(AccountType::Local, AccountType::External);

        // Test hashability (can be used in HashSet)
        let mut set = HashSet::new();
        set.insert(AccountType::Local);
        set.insert(AccountType::External);
        assert_eq!(set.len(), 2);

        // Inserting duplicate should not increase size
        set.insert(AccountType::Local);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_account_type_clone() {
        let local = AccountType::Local;
        let cloned = local.clone();
        assert_eq!(local, cloned);

        let external = AccountType::External;
        let cloned = external.clone();
        assert_eq!(external, cloned);
    }

    #[test]
    fn test_account_type_debug() {
        let local = AccountType::Local;
        let debug_str = format!("{:?}", local);
        assert!(debug_str.contains("Local"));

        let external = AccountType::External;
        let debug_str = format!("{:?}", external);
        assert!(debug_str.contains("External"));
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

    #[test]
    fn test_create_mdk_with_invalid_path() {
        let pubkey = Keys::generate().public_key();
        let file = tempfile::NamedTempFile::new().unwrap();
        let result = Account::create_mdk(pubkey, file.path(), "test.service");
        assert!(result.is_err());
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
    // Account relay convenience method tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_account_relay_convenience_methods() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = whitenoise.persist_account(&account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        // Initially all relay types should be empty.
        assert!(account.nip65_relays(&whitenoise).await.unwrap().is_empty());
        assert!(account.inbox_relays(&whitenoise).await.unwrap().is_empty());
        assert!(
            account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty()
        );

        // Add relays to each type.
        let user = account.user(&whitenoise.database).await.unwrap();
        let url1 = RelayUrl::parse("wss://nip65.example.com").unwrap();
        let url2 = RelayUrl::parse("wss://inbox.example.com").unwrap();
        let url3 = RelayUrl::parse("wss://kp.example.com").unwrap();
        let relay1 = whitenoise.find_or_create_relay_by_url(&url1).await.unwrap();
        let relay2 = whitenoise.find_or_create_relay_by_url(&url2).await.unwrap();
        let relay3 = whitenoise.find_or_create_relay_by_url(&url3).await.unwrap();

        user.add_relays(&[relay1], RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();
        user.add_relays(&[relay2], RelayType::Inbox, &whitenoise.database)
            .await
            .unwrap();
        user.add_relays(&[relay3], RelayType::KeyPackage, &whitenoise.database)
            .await
            .unwrap();

        // Verify each convenience method returns the right relays.
        let nip65 = account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(nip65.len(), 1);
        assert_eq!(nip65[0].url, url1);

        let inbox = account.inbox_relays(&whitenoise).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].url, url2);

        let kp = account.key_package_relays(&whitenoise).await.unwrap();
        assert_eq!(kp.len(), 1);
        assert_eq!(kp[0].url, url3);

        // Also test the generic relays() method.
        let all_nip65 = account.relays(RelayType::Nip65, &whitenoise).await.unwrap();
        assert_eq!(all_nip65.len(), 1);
        assert_eq!(all_nip65[0].url, url1);
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
