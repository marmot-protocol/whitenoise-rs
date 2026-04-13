mod login;
mod login_external_signer;
mod login_multistep;
mod setup;

use chrono::{DateTime, Utc};
use mdk_core::prelude::*;
use mdk_sqlite_storage::MdkSqliteStorage;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use thiserror::Error;

use crate::RelayType;
use crate::nostr_manager::NostrManagerError;
use crate::perf_instrument;
use crate::types::ImageType;
use crate::whitenoise::error::Result;
use crate::whitenoise::groups::blossom_error::BlossomError;
use crate::whitenoise::relays::Relay;
use crate::whitenoise::secrets_store::SecretsStoreError;
use crate::whitenoise::user_streaming::{UserUpdate, UserUpdateTrigger};
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
pub struct LoginResult {
    /// The account that was created or found.
    pub account: Account,
    /// Whether login completed or needs further action.
    pub status: LoginStatus,
}

/// The three relay lists discovered during login.
///
/// Each field holds the relays found on the network for that list type.
/// `None` means that list was **not found** on the network and must be
/// published (either from defaults or from a user-provided relay) before
/// login can complete.  `Some(vec![])` means the author intentionally
/// published an empty relay list.  Use [`DiscoveredRelayLists::is_complete`]
/// to check whether all three are present.
#[derive(Debug, Clone)]
pub struct DiscoveredRelayLists {
    /// NIP-65 relay list (kind 10002).  `None` if not found on the network.
    pub nip65: Option<Vec<Relay>>,
    /// Inbox relays (kind 10050).  `None` if not found on the network.
    pub inbox: Option<Vec<Relay>>,
    /// Key-package relays (kind 10051).  `None` if not found on the network.
    pub key_package: Option<Vec<Relay>>,
}

impl DiscoveredRelayLists {
    /// Returns `true` when all three relay lists were found on the network.
    pub fn is_complete(&self) -> bool {
        self.nip65.is_some() && self.inbox.is_some() && self.key_package.is_some()
    }

    /// Returns `true` when the relay list for `relay_type` was found on the
    /// network (even if it was intentionally empty).
    pub fn found(&self, relay_type: RelayType) -> bool {
        match relay_type {
            RelayType::Nip65 => self.nip65.is_some(),
            RelayType::Inbox => self.inbox.is_some(),
            RelayType::KeyPackage => self.key_package.is_some(),
        }
    }

    /// Returns the relay slice for the given `relay_type`, or an empty slice
    /// when the list was not found.
    pub fn relays(&self, relay_type: RelayType) -> &[Relay] {
        match relay_type {
            RelayType::Nip65 => self.nip65.as_deref().unwrap_or(&[]),
            RelayType::Inbox => self.inbox.as_deref().unwrap_or(&[]),
            RelayType::KeyPackage => self.key_package.as_deref().unwrap_or(&[]),
        }
    }

    /// Returns the relays for `relay_type` when the list was found, otherwise `fallback`.
    pub fn relays_or<'a>(&'a self, relay_type: RelayType, fallback: &'a [Relay]) -> &'a [Relay] {
        if self.found(relay_type) {
            self.relays(relay_type)
        } else {
            fallback
        }
    }

    /// Merge `other` into `self`, keeping any `Some` field from either side.
    ///
    /// Each field is updated only when the current value is `None` and the
    /// incoming value is `Some`, so previously discovered relay lists are
    /// never discarded.
    pub fn merge(&mut self, other: Self) {
        if self.nip65.is_none() && other.nip65.is_some() {
            self.nip65 = other.nip65;
        }
        if self.inbox.is_none() && other.inbox.is_some() {
            self.inbox = other.inbox;
        }
        if self.key_package.is_none() && other.key_package.is_some() {
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

    /// The platform keyring/credential store is not available.
    #[error("{0}")]
    KeyringUnavailable(String),

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
            WhitenoiseError::SecretsStore(ref e) => match e {
                SecretsStoreError::KeyringError(_)
                | SecretsStoreError::KeyringNotInitialized(_)
                | SecretsStoreError::KeyringUnavailable(_) => {
                    Self::KeyringUnavailable(e.to_string())
                }
                SecretsStoreError::KeyNotFound | SecretsStoreError::KeyError(_) => {
                    Self::Internal(e.to_string())
                }
            },
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
    #[perf_instrument("accounts")]
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
    #[perf_instrument("accounts")]
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
    #[perf_instrument("accounts")]
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
    #[perf_instrument("accounts")]
    pub(crate) async fn nip65_relays(&self, whitenoise: &Whitenoise) -> Result<Vec<Relay>> {
        let user = self.user(&whitenoise.database).await?;
        let relays = user.relays(RelayType::Nip65, &whitenoise.database).await?;
        Ok(relays)
    }

    /// Helper method to retrieve the inbox relays for this account.
    #[perf_instrument("accounts")]
    pub(crate) async fn inbox_relays(&self, whitenoise: &Whitenoise) -> Result<Vec<Relay>> {
        let user = self.user(&whitenoise.database).await?;
        let relays = user.relays(RelayType::Inbox, &whitenoise.database).await?;
        Ok(relays)
    }

    /// Returns inbox relays for this account, falling back to NIP-65 relays
    /// when no inbox relay list (kind 10050) has been published.
    ///
    /// Accounts created before PR #515 may lack a kind 10050 event. Without
    /// this fallback, giftwrap subscriptions would be set up with zero relays,
    /// silently preventing the account from receiving DMs.
    #[perf_instrument("accounts")]
    pub(crate) async fn effective_inbox_relays(
        &self,
        whitenoise: &Whitenoise,
    ) -> Result<Vec<Relay>> {
        let inbox = self.inbox_relays(whitenoise).await?;
        if !inbox.is_empty() {
            return Ok(inbox);
        }
        tracing::warn!(
            target: "whitenoise::accounts",
            "Account {} has no inbox relays, falling back to NIP-65 relays for giftwrap subscription",
            self.pubkey.to_hex()
        );
        self.nip65_relays(whitenoise).await
    }

    /// Helper method to retrieve the key package relays for this account.
    #[perf_instrument("accounts")]
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
    #[perf_instrument("accounts")]
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
    #[perf_instrument("accounts")]
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
    #[perf_instrument("accounts")]
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
    #[perf_instrument("accounts")]
    pub async fn update_metadata(
        &self,
        metadata: &Metadata,
        whitenoise: &Whitenoise,
    ) -> Result<()> {
        tracing::debug!(target: "whitenoise::accounts", "Updating metadata for account: {:?}", self.pubkey);
        let mut user = self.user(&whitenoise.database).await?;
        user.metadata = metadata.clone();
        user.mark_metadata_known_now();
        let saved_user = user.save(&whitenoise.database).await?;
        let user_pubkey = saved_user.pubkey;
        whitenoise.user_stream_manager.emit(
            &user_pubkey,
            UserUpdate {
                trigger: UserUpdateTrigger::LocalMetadataChanged,
                user: saved_user,
            },
        );
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
    #[perf_instrument("accounts")]
    pub async fn upload_profile_picture(
        &self,
        file_path: &str,
        image_type: ImageType,
        server: Url,
        whitenoise: &Whitenoise,
    ) -> Result<String> {
        let client = Whitenoise::blossom_client(&server)?;
        let signer = whitenoise.get_signer_for_account(self)?;
        let data = tokio::fs::read(file_path).await?;

        let upload_future = client.upload_blob(
            data,
            Some(image_type.mime_type().to_string()),
            None,
            Some(&signer),
        );

        let descriptor = tokio::time::timeout(Whitenoise::BLOSSOM_TIMEOUT, upload_future)
            .await
            .map_err(|_| BlossomError::Timeout(Whitenoise::BLOSSOM_TIMEOUT))?
            .map_err(BlossomError::client)?;

        Whitenoise::require_https(&descriptor.url)?;

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

#[cfg(test)]
mod tests {
    use crate::whitenoise::accounts::Account;
    use crate::whitenoise::relays::{Relay, RelayType};
    use crate::whitenoise::test_utils::*;
    use crate::whitenoise::user_streaming::UserUpdateTrigger;
    use nostr_sdk::prelude::*;
    use nostr_sdk::{Metadata, RelayUrl};

    #[tokio::test]
    async fn test_effective_inbox_relays_returns_inbox_when_present() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, _keys) = create_test_account(&whitenoise).await;
        let user = account.user(&whitenoise.database).await.unwrap();

        let inbox_url = RelayUrl::parse("wss://inbox.example.com").unwrap();
        let nip65_url = RelayUrl::parse("wss://nip65.example.com").unwrap();

        let inbox_relay = Relay::find_or_create_by_url(&inbox_url, &whitenoise.database)
            .await
            .unwrap();
        let nip65_relay = Relay::find_or_create_by_url(&nip65_url, &whitenoise.database)
            .await
            .unwrap();

        user.add_relay(&inbox_relay, RelayType::Inbox, &whitenoise.database)
            .await
            .unwrap();
        user.add_relay(&nip65_relay, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        let result = account.effective_inbox_relays(&whitenoise).await.unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].url, inbox_url);
    }

    #[tokio::test]
    async fn test_effective_inbox_relays_falls_back_to_nip65() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, _keys) = create_test_account(&whitenoise).await;
        let user = account.user(&whitenoise.database).await.unwrap();

        // Only add NIP-65 relays — no inbox relays
        let nip65_url = RelayUrl::parse("wss://nip65.example.com").unwrap();
        let nip65_relay = Relay::find_or_create_by_url(&nip65_url, &whitenoise.database)
            .await
            .unwrap();
        user.add_relay(&nip65_relay, RelayType::Nip65, &whitenoise.database)
            .await
            .unwrap();

        let result = account.effective_inbox_relays(&whitenoise).await.unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].url, nip65_url);
    }

    #[tokio::test]
    async fn test_effective_inbox_relays_returns_empty_when_no_relays() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, _keys) = create_test_account(&whitenoise).await;

        // No relays at all
        let result = account.effective_inbox_relays(&whitenoise).await.unwrap();

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_update_metadata_emits_local_metadata_changed_update() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = account.save(&whitenoise.database).await.unwrap();

        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        let default_relays = whitenoise.load_default_relays().await.unwrap();
        whitenoise
            .add_relays_to_account(&account, &default_relays, RelayType::Nip65)
            .await
            .unwrap();

        let mut subscription = whitenoise.subscribe_to_user(&account.pubkey).await.unwrap();
        let new_metadata = Metadata::new().name("Local Update");

        account
            .update_metadata(&new_metadata, &whitenoise)
            .await
            .unwrap();

        let update = subscription
            .updates
            .try_recv()
            .expect("should receive update");

        assert_eq!(update.trigger, UserUpdateTrigger::LocalMetadataChanged);
        assert_eq!(update.user.pubkey, account.pubkey);
        assert_eq!(update.user.metadata.name, Some("Local Update".to_string()));
        assert!(update.user.metadata_known_at.is_some());
    }

    // -----------------------------------------------------------------------
    // Account / AccountType struct and type tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_since_timestamp_none_when_never_synced() {
        use chrono::Utc;
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: crate::whitenoise::accounts::AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(account.since_timestamp(10).is_none());
    }

    #[test]
    fn test_since_timestamp_applies_buffer() {
        use chrono::{TimeDelta, Utc};
        let now = Utc::now();
        let last = now - TimeDelta::seconds(100);
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: crate::whitenoise::accounts::AccountType::Local,
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
        use chrono::Utc;
        // Choose a timestamp very close to the epoch so that buffer would underflow
        let epochish = chrono::DateTime::<Utc>::from_timestamp(5, 0).unwrap();
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: crate::whitenoise::accounts::AccountType::Local,
            last_synced_at: Some(epochish),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let ts = account.since_timestamp(10).unwrap();
        assert_eq!(ts.as_secs(), 0);
    }

    #[test]
    fn test_since_timestamp_clamps_future_to_now_minus_buffer() {
        use chrono::Utc;
        let now = Utc::now();
        let future = now + chrono::TimeDelta::seconds(3600 * 24); // 24h in the future
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: crate::whitenoise::accounts::AccountType::Local,
            last_synced_at: Some(future),
            created_at: now,
            updated_at: now,
        };
        let buffer = 10u64;
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

    #[test]
    fn test_has_local_key_returns_true_for_local_account() {
        use chrono::Utc;
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: crate::whitenoise::accounts::AccountType::Local,
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
        use chrono::Utc;
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: crate::whitenoise::accounts::AccountType::External,
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
    fn test_uses_external_signer_returns_true_for_external() {
        use chrono::Utc;
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: crate::whitenoise::accounts::AccountType::External,
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
        use chrono::Utc;
        let account = Account {
            id: None,
            pubkey: Keys::generate().public_key(),
            user_id: 1,
            account_type: crate::whitenoise::accounts::AccountType::Local,
            last_synced_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(
            !account.uses_external_signer(),
            "Local account should not use external signer"
        );
    }

    #[test]
    fn test_account_type_from_str_local() {
        use crate::whitenoise::accounts::AccountType;
        let result: std::result::Result<AccountType, String> = "local".parse();
        assert_eq!(result.unwrap(), AccountType::Local);
        let result: std::result::Result<AccountType, String> = "LOCAL".parse();
        assert_eq!(result.unwrap(), AccountType::Local);
        let result: std::result::Result<AccountType, String> = "Local".parse();
        assert_eq!(result.unwrap(), AccountType::Local);
    }

    #[test]
    fn test_account_type_from_str_external() {
        use crate::whitenoise::accounts::AccountType;
        let result: std::result::Result<AccountType, String> = "external".parse();
        assert_eq!(result.unwrap(), AccountType::External);
        let result: std::result::Result<AccountType, String> = "EXTERNAL".parse();
        assert_eq!(result.unwrap(), AccountType::External);
        let result: std::result::Result<AccountType, String> = "External".parse();
        assert_eq!(result.unwrap(), AccountType::External);
    }

    #[test]
    fn test_account_type_from_str_invalid() {
        use crate::whitenoise::accounts::AccountType;
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
        use crate::whitenoise::accounts::AccountType;
        assert_eq!(format!("{}", AccountType::Local), "local");
        assert_eq!(format!("{}", AccountType::External), "external");
    }

    #[test]
    fn test_account_type_default() {
        use crate::whitenoise::accounts::AccountType;
        let default_type = AccountType::default();
        assert_eq!(default_type, AccountType::Local);
    }

    #[tokio::test]
    async fn test_new_external_creates_external_account() {
        use crate::whitenoise::accounts::AccountType;
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = create_test_keys();
        let pubkey = keys.public_key();
        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        assert_eq!(account.account_type, AccountType::External);
        assert_eq!(account.pubkey, pubkey);
        assert!(account.id.is_none());
        assert!(account.last_synced_at.is_none());
    }

    #[tokio::test]
    async fn test_new_external_sets_correct_fields() {
        use crate::whitenoise::accounts::AccountType;
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = nostr_sdk::Keys::generate();
        let pubkey = keys.public_key();
        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        assert_eq!(account.pubkey, pubkey);
        assert_eq!(account.account_type, AccountType::External);
        assert!(account.id.is_none(), "New account should not be persisted");
        assert!(account.last_synced_at.is_none());
        assert!(account.user_id > 0, "Should have a valid user_id");
    }

    #[tokio::test]
    async fn test_external_account_uses_external_signer_and_has_local_key() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = nostr_sdk::Keys::generate();
        let pubkey = keys.public_key();
        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();
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
    async fn test_local_account_uses_external_signer_and_has_local_key() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, _keys) = Account::new(&whitenoise, None).await.unwrap();
        assert!(
            !account.uses_external_signer(),
            "Local account should not report using external signer"
        );
        assert!(
            account.has_local_key(),
            "Local account should report having local key"
        );
    }

    #[tokio::test]
    async fn test_new_creates_local_account_with_generated_keys() {
        use crate::whitenoise::accounts::AccountType;
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = Account::new(&whitenoise, None).await.unwrap();
        assert_eq!(account.account_type, AccountType::Local);
        assert_eq!(account.pubkey, keys.public_key());
        assert!(account.id.is_none());
        assert!(account.last_synced_at.is_none());
    }

    #[tokio::test]
    async fn test_new_creates_local_account_with_provided_keys() {
        use crate::whitenoise::accounts::AccountType;
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

    #[tokio::test]
    async fn test_external_account_database_roundtrip() {
        use crate::whitenoise::accounts::AccountType;
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = nostr_sdk::Keys::generate();
        let pubkey = keys.public_key();

        let account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        let persisted = whitenoise.persist_account(&account).await.unwrap();

        assert!(persisted.id.is_some());
        assert_eq!(persisted.account_type, AccountType::External);

        let found = Account::find_by_pubkey(&pubkey, &whitenoise.database)
            .await
            .unwrap();
        assert_eq!(found.pubkey, pubkey);
        assert_eq!(found.account_type, AccountType::External);
    }

    #[tokio::test]
    async fn test_external_account_no_keys_in_secrets_store() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let keys = nostr_sdk::Keys::generate();
        let pubkey = keys.public_key();
        let _account = Account::new_external(&whitenoise, pubkey).await.unwrap();
        let result = whitenoise.secrets_store.get_nostr_keys_for_pubkey(&pubkey);
        assert!(
            result.is_err(),
            "External account creation should not store any keys"
        );
    }

    #[tokio::test]
    async fn test_account_type_creation() {
        use crate::whitenoise::accounts::AccountType;
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (local_gen, keys_gen) = Account::new(&whitenoise, None).await.unwrap();
        assert_eq!(local_gen.account_type, AccountType::Local);
        assert_eq!(local_gen.pubkey, keys_gen.public_key());

        let provided = create_test_keys();
        let (local_prov, keys_prov) = Account::new(&whitenoise, Some(provided.clone()))
            .await
            .unwrap();
        assert_eq!(local_prov.account_type, AccountType::Local);
        assert_eq!(keys_prov.public_key(), provided.public_key());

        let ext_pubkey = nostr_sdk::Keys::generate().public_key();
        let external = Account::new_external(&whitenoise, ext_pubkey)
            .await
            .unwrap();
        assert_eq!(external.account_type, AccountType::External);
        assert_eq!(external.pubkey, ext_pubkey);
        assert!(!external.has_local_key());
        assert!(external.uses_external_signer());
    }

    #[tokio::test]
    async fn test_account_crud_operations() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        assert_eq!(whitenoise.get_accounts_count().await.unwrap(), 0);
        assert!(whitenoise.all_accounts().await.unwrap().is_empty());

        let (account1, _) = create_test_account(&whitenoise).await;
        let account1 = whitenoise.persist_account(&account1).await.unwrap();
        assert!(account1.id.is_some());

        let (account2, _) = create_test_account(&whitenoise).await;
        whitenoise.persist_account(&account2).await.unwrap();

        assert_eq!(whitenoise.get_accounts_count().await.unwrap(), 2);
        let all = whitenoise.all_accounts().await.unwrap();
        assert_eq!(all.len(), 2);

        let found = whitenoise
            .find_account_by_pubkey(&account1.pubkey)
            .await
            .unwrap();
        assert_eq!(found.pubkey, account1.pubkey);

        let random_pk = nostr_sdk::Keys::generate().public_key();
        assert!(whitenoise.find_account_by_pubkey(&random_pk).await.is_err());

        let user = account1.user(&whitenoise.database).await.unwrap();
        assert_eq!(user.pubkey, account1.pubkey);

        let metadata = account1.metadata(&whitenoise).await.unwrap();
        assert!(metadata.name.is_none() || metadata.name.as_deref() == Some(""));
    }

    #[tokio::test]
    async fn test_account_relay_operations() {
        use nostr_sdk::RelayUrl;
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let (account, keys) = create_test_account(&whitenoise).await;
        let account = whitenoise.persist_account(&account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        assert!(account.nip65_relays(&whitenoise).await.unwrap().is_empty());
        assert!(account.inbox_relays(&whitenoise).await.unwrap().is_empty());
        assert!(
            account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty()
        );

        let default_relays = whitenoise.load_default_relays().await.unwrap();
        #[cfg(debug_assertions)]
        assert_eq!(default_relays.len(), 2);

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

        let nip65 = account.relays(RelayType::Nip65, &whitenoise).await.unwrap();
        let inbox = account.relays(RelayType::Inbox, &whitenoise).await.unwrap();
        let kp = account
            .relays(RelayType::KeyPackage, &whitenoise)
            .await
            .unwrap();
        assert_eq!(nip65.len(), default_relays.len());
        assert_eq!(inbox.len(), default_relays.len());
        assert_eq!(kp.len(), default_relays.len());

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

    #[tokio::test]
    async fn test_account_relay_convenience_methods() {
        use nostr_sdk::RelayUrl;
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let (account, keys) = create_test_account(&whitenoise).await;
        let account = whitenoise.persist_account(&account).await.unwrap();
        whitenoise.secrets_store.store_private_key(&keys).unwrap();

        assert!(account.nip65_relays(&whitenoise).await.unwrap().is_empty());
        assert!(account.inbox_relays(&whitenoise).await.unwrap().is_empty());
        assert!(
            account
                .key_package_relays(&whitenoise)
                .await
                .unwrap()
                .is_empty()
        );

        let user = account.user(&whitenoise.database).await.unwrap();
        let url1 = RelayUrl::parse("ws://127.0.0.1:19010").unwrap();
        let url2 = RelayUrl::parse("ws://127.0.0.1:19011").unwrap();
        let url3 = RelayUrl::parse("ws://127.0.0.1:19012").unwrap();
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

        let nip65 = account.nip65_relays(&whitenoise).await.unwrap();
        assert_eq!(nip65.len(), 1);
        assert_eq!(nip65[0].url, url1);

        let inbox = account.inbox_relays(&whitenoise).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].url, url2);

        let kp = account.key_package_relays(&whitenoise).await.unwrap();
        assert_eq!(kp.len(), 1);
        assert_eq!(kp[0].url, url3);

        let all_nip65 = account.relays(RelayType::Nip65, &whitenoise).await.unwrap();
        assert_eq!(all_nip65.len(), 1);
        assert_eq!(all_nip65[0].url, url1);
    }

    #[test]
    fn test_create_mdk_success() {
        crate::whitenoise::Whitenoise::initialize_mock_keyring_store();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let pubkey = nostr_sdk::Keys::generate().public_key();
        let result = Account::create_mdk(pubkey, temp_dir.path(), "com.whitenoise.test");
        assert!(result.is_ok(), "create_mdk failed: {:?}", result.err());
    }

    #[test]
    fn test_create_mdk_with_invalid_path() {
        let pubkey = nostr_sdk::Keys::generate().public_key();
        let file = tempfile::NamedTempFile::new().unwrap();
        let result = Account::create_mdk(pubkey, file.path(), "test.service");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_load_accounts() {
        use nostr_sdk::prelude::*;
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let accounts = Account::all(&whitenoise.database).await.unwrap();
        assert!(accounts.is_empty());

        let (account1, keys1) = create_test_account(&whitenoise).await;
        let (account2, keys2) = create_test_account(&whitenoise).await;

        account1.save(&whitenoise.database).await.unwrap();
        account2.save(&whitenoise.database).await.unwrap();

        whitenoise.secrets_store.store_private_key(&keys1).unwrap();
        whitenoise.secrets_store.store_private_key(&keys2).unwrap();

        let loaded_accounts = Account::all(&whitenoise.database).await.unwrap();
        assert_eq!(loaded_accounts.len(), 2);
        let pubkeys: Vec<PublicKey> = loaded_accounts.iter().map(|a| a.pubkey).collect();
        assert!(pubkeys.contains(&account1.pubkey));
        assert!(pubkeys.contains(&account2.pubkey));

        let loaded_account1 = loaded_accounts
            .iter()
            .find(|a| a.pubkey == account1.pubkey)
            .unwrap();
        assert_eq!(loaded_account1.pubkey, account1.pubkey);
        assert_eq!(loaded_account1.user_id, account1.user_id);
        assert_eq!(loaded_account1.last_synced_at, account1.last_synced_at);
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
}
