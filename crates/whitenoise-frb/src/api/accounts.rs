use crate::api::{
    DEFAULT_RELAY_URLS, error::ApiError, metadata::FlutterMetadata, relays::Relay, users::User,
};
use crate::api::{wn, wn_session};
use chrono::{DateTime, TimeZone, Utc};
use flutter_rust_bridge::frb;
use nostr_sdk::prelude::*;
use whitenoise::{
    Account as WhitenoiseAccount, AccountSettings as WhitenoiseAccountSettings,
    AccountType as WhitenoiseAccountType, ImageType, LoginResult as WhitenoiseLoginResult,
    LoginStatus as WhitenoiseLoginStatus, RelayType,
};

/// The type of account authentication.
#[frb(non_opaque)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountType {
    /// Account with locally stored private key.
    Local,
    /// Account using external signer (e.g., Amber via NIP-55).
    External,
}

impl From<WhitenoiseAccountType> for AccountType {
    fn from(account_type: WhitenoiseAccountType) -> Self {
        match account_type {
            WhitenoiseAccountType::Local => AccountType::Local,
            WhitenoiseAccountType::External => AccountType::External,
        }
    }
}

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct Account {
    pub pubkey: String,
    /// The type of account (local key or external signer).
    pub account_type: AccountType,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<WhitenoiseAccount> for Account {
    fn from(account: WhitenoiseAccount) -> Self {
        Self {
            pubkey: account.pubkey.to_hex(),
            account_type: account.account_type.into(),
            last_synced_at: account.last_synced_at,
            created_at: account.created_at,
            updated_at: account.updated_at,
        }
    }
}

/// The status of a login attempt.
#[frb(non_opaque)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStatus {
    /// Login completed successfully.
    Complete,
    /// Relay lists were not found. The caller must resolve relay lists before
    /// login can complete.
    NeedsRelayLists,
}

impl From<WhitenoiseLoginStatus> for LoginStatus {
    fn from(status: WhitenoiseLoginStatus) -> Self {
        match status {
            WhitenoiseLoginStatus::Complete => LoginStatus::Complete,
            WhitenoiseLoginStatus::NeedsRelayLists => LoginStatus::NeedsRelayLists,
        }
    }
}

/// The result of a login attempt.
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct LoginResult {
    pub account: Account,
    pub status: LoginStatus,
}

impl From<WhitenoiseLoginResult> for LoginResult {
    fn from(result: WhitenoiseLoginResult) -> Self {
        Self {
            account: result.account.into(),
            status: result.status.into(),
        }
    }
}

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct FlutterEvent {
    pub id: String,
    pub pubkey: String,
    pub created_at: DateTime<Utc>,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
}

impl From<Event> for FlutterEvent {
    fn from(event: Event) -> Self {
        Self {
            id: event.id.to_hex(),
            pubkey: event.pubkey.to_hex(),
            created_at: {
                let ts = i64::try_from(event.created_at.as_secs()).unwrap_or(0);
                Utc.timestamp_opt(ts, 0)
                    .single()
                    .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap())
            },
            kind: event.kind.as_u16(),
            tags: event
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
            content: event.content,
        }
    }
}

#[frb]
pub async fn get_accounts() -> Result<Vec<Account>, ApiError> {
    let whitenoise = wn()?;
    let accounts = whitenoise.all_accounts().await?;
    Ok(accounts.into_iter().map(|a| a.into()).collect())
}

#[frb]
pub async fn get_account(pubkey: String) -> Result<Account, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    Ok(account.into())
}

#[frb]
pub async fn create_identity() -> Result<Account, ApiError> {
    let whitenoise = wn()?;
    let account = whitenoise.create_identity().await?;
    Ok(account.into())
}

// -----------------------------------------------------------------------
// Multi-step login API (nsec / hex private key)
// -----------------------------------------------------------------------

#[frb]
pub async fn login_start(nsec_or_hex_privkey: String) -> Result<LoginResult, ApiError> {
    let whitenoise = wn()?;
    let result = whitenoise.login_start(nsec_or_hex_privkey).await?;
    Ok(result.into())
}

#[frb]
pub async fn login_publish_default_relays(pubkey: String) -> Result<LoginResult, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let result = whitenoise.login_publish_default_relays(&pubkey).await?;
    Ok(result.into())
}

#[frb]
pub async fn login_with_custom_relay(
    pubkey: String,
    relay_url: String,
) -> Result<LoginResult, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let relay_url = RelayUrl::parse(&relay_url)?;
    let result = whitenoise
        .login_with_custom_relay(&pubkey, relay_url)
        .await?;
    Ok(result.into())
}

#[frb]
pub async fn login_cancel(pubkey: String) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    whitenoise.login_cancel(&pubkey).await?;
    Ok(())
}

#[frb]
pub async fn logout(pubkey: String) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    whitenoise.logout(&pubkey).await.map_err(ApiError::from)
}

#[frb]
pub async fn export_account_nsec(pubkey: String) -> Result<String, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    whitenoise
        .export_account_nsec(&account)
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn update_account_metadata(
    pubkey: String,
    metadata: &FlutterMetadata,
) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    account
        .update_metadata(&metadata.into(), whitenoise)
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn upload_account_profile_picture(
    pubkey: String,
    server_url: String,
    file_path: String,
    image_type: String,
) -> Result<String, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let image_type = ImageType::try_from(image_type).map_err(|error| ApiError::Other {
        message: error.to_string(),
    })?;

    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    let server = Url::parse(&server_url)?;

    account
        .upload_profile_picture(&file_path, image_type, server, &whitenoise)
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn account_relays(pubkey: String, relay_type: RelayType) -> Result<Vec<Relay>, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    let relays = account.relays(relay_type, &whitenoise.shared).await?;
    Ok(relays.into_iter().map(|r| r.into()).collect())
}

#[frb]
pub async fn restore_default_relays(pubkey: String) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;

    for relay_type in [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage] {
        let current_relays = account
            .relays(relay_type.clone(), &whitenoise.shared)
            .await?;
        for relay in &current_relays {
            account
                .remove_relay(relay, relay_type.clone(), whitenoise)
                .await?;
        }
        for url in DEFAULT_RELAY_URLS {
            let relay_url = RelayUrl::parse(url)?;
            let relay = whitenoise.find_or_create_relay_by_url(&relay_url).await?;
            account
                .add_relay(&relay, relay_type.clone(), whitenoise)
                .await?;
        }
    }

    Ok(())
}

#[frb]
pub async fn add_account_relay(
    pubkey: String,
    url: String,
    relay_type: RelayType,
) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    let relay_url = RelayUrl::parse(&url)?;
    let relay = whitenoise.find_or_create_relay_by_url(&relay_url).await?;
    account
        .add_relay(&relay, relay_type, whitenoise)
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn remove_account_relay(
    pubkey: String,
    url: String,
    relay_type: RelayType,
) -> Result<(), ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let account = whitenoise.find_account_by_pubkey(&pubkey).await?;
    let relay_url = RelayUrl::parse(&url)?;
    let relay = whitenoise.find_or_create_relay_by_url(&relay_url).await?;
    account
        .remove_relay(&relay, relay_type, whitenoise)
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn account_key_package(pubkey: String) -> Result<Option<FlutterEvent>, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let user = whitenoise.find_user_by_pubkey(&pubkey).await?;
    let event = user.key_package_event(&whitenoise.shared).await?;
    Ok(event.map(|e| e.into()))
}

#[frb]
pub async fn account_key_packages(account_pubkey: String) -> Result<Vec<FlutterEvent>, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    let key_packages = session.key_packages().fetch_all().await?;
    Ok(key_packages.into_iter().map(|e| e.into()).collect())
}

#[frb]
pub async fn publish_account_key_package(account_pubkey: String) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    session
        .key_packages()
        .publish()
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn delete_account_key_package(
    account_pubkey: String,
    key_package_id: String,
) -> Result<bool, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    let key_package_id = EventId::parse(&key_package_id)?;
    session
        .key_packages()
        .delete(&key_package_id, true)
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn delete_account_key_packages(account_pubkey: String) -> Result<usize, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    let deleted_count = session.key_packages().delete_legacy().await?;
    Ok(deleted_count)
}

#[frb]
pub async fn account_follows(pubkey: String) -> Result<Vec<User>, ApiError> {
    let pubkey = PublicKey::parse(&pubkey)?;
    let session = wn_session(&pubkey)?;
    let follows = session.social().follows().await?;
    Ok(follows.into_iter().map(|u| u.into()).collect())
}

#[frb]
pub async fn follow_user(
    account_pubkey: String,
    user_to_follow_pubkey: String,
) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    let user_to_follow_pubkey = PublicKey::parse(&user_to_follow_pubkey)?;
    session
        .social()
        .follow_user(&user_to_follow_pubkey)
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn unfollow_user(
    account_pubkey: String,
    user_to_unfollow_pubkey: String,
) -> Result<(), ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    let user_to_unfollow_pubkey = PublicKey::parse(&user_to_unfollow_pubkey)?;
    session
        .social()
        .unfollow_user(&user_to_unfollow_pubkey)
        .await
        .map_err(ApiError::from)
}

#[frb]
pub async fn is_following_user(
    account_pubkey: String,
    user_pubkey: String,
) -> Result<bool, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    let user_pubkey = PublicKey::parse(&user_pubkey)?;
    session
        .social()
        .is_following(&user_pubkey)
        .await
        .map_err(ApiError::from)
}

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct AccountSettings {
    pub notifications_enabled: bool,
}

impl From<WhitenoiseAccountSettings> for AccountSettings {
    fn from(s: WhitenoiseAccountSettings) -> Self {
        Self {
            notifications_enabled: s.notifications_enabled,
        }
    }
}

#[frb]
pub async fn account_settings(pubkey: String) -> Result<AccountSettings, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let session = whitenoise
        .session(&pubkey)
        .ok_or_else(|| ApiError::Whitenoise {
            message: "Account session not found".to_string(),
        })?;
    let settings = session.settings().get().await?;
    Ok(settings.into())
}

#[frb]
pub async fn update_notifications_enabled(
    pubkey: String,
    enabled: bool,
) -> Result<AccountSettings, ApiError> {
    let whitenoise = wn()?;
    let pubkey = PublicKey::parse(&pubkey)?;
    let session = whitenoise
        .session(&pubkey)
        .ok_or_else(|| ApiError::Whitenoise {
            message: "Account session not found".to_string(),
        })?;
    let settings = session
        .settings()
        .update_notifications_enabled(enabled)
        .await?;
    Ok(settings.into())
}
