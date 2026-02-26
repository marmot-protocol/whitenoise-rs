mod login;
mod setup;

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
use crate::nostr_manager::NostrManagerError;
use crate::types::ImageType;
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
