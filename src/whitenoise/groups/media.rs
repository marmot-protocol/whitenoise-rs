use std::path::PathBuf;

use mdk_core::media_processing::MediaProcessingOptions;
use mdk_core::prelude::{GroupId, group_types};
use nostr_blossom::client::BlossomClient;
use nostr_sdk::prelude::*;

use crate::perf_instrument;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::database::media_files::MediaFile;
use crate::whitenoise::error::{Result, WhitenoiseError};

impl Whitenoise {
    /// Syncs group image cache if needed (smart, hash-based check)
    ///
    /// This method is called after processing welcomes and commits to proactively
    /// cache group images. It only downloads if:
    /// 1. The group has an image set
    /// 2. The image_hash is not already cached
    ///
    /// This ensures images are ready before the UI needs them, while avoiding
    /// redundant downloads.
    ///
    /// # Arguments
    /// * `account` - The account viewing the group
    /// * `group_id` - The MLS group ID
    #[deprecated(
        note = "Use AccountSession::groups().media().sync_group_image_cache_if_needed() instead."
    )]
    #[perf_instrument("media")]
    pub(crate) async fn sync_group_image_cache_if_needed(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<()> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session
            .groups()
            .media()
            .sync_group_image_cache_if_needed(group_id)
            .await
    }

    /// Spawns a background task to sync group image cache without blocking
    ///
    /// This is used by event handlers to proactively cache group images
    /// without blocking event processing. Failures are logged but don't
    /// affect the caller - images will download on-demand if needed.
    ///
    /// # Arguments
    /// * `account` - The account viewing the group
    /// * `group_id` - The MLS group ID
    pub(crate) fn background_sync_group_image_cache_if_needed(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) {
        let Some(session) = self.session(&account.pubkey) else {
            tracing::error!(
                target: "whitenoise::groups::background_sync_group_image_cache_if_needed",
                account = %account.pubkey,
                "No active session for background image cache"
            );
            return;
        };
        let group_id = group_id.clone();
        tokio::spawn(async move {
            if let Err(e) = session
                .groups()
                .media()
                .sync_group_image_cache_if_needed(&group_id)
                .await
            {
                tracing::warn!(
                    target: "whitenoise::groups::background_sync_group_image_cache_if_needed",
                    "Background image cache failed: {}. Image will download on-demand.",
                    e
                );
            }
        });
    }

    /// Uploads a group image to a Blossom server and returns the encrypted metadata.
    ///
    /// The returned metadata (hash, key, nonce) should be passed to `update_group_data`
    /// to update the group's image settings.
    #[deprecated(note = "Use AccountSession::groups().media().upload_group_image() instead.")]
    #[perf_instrument("media")]
    pub async fn upload_group_image(
        &self,
        account: &Account,
        group_id: &GroupId,
        file_path: &str,
        blossom_server_url: Option<Url>,
        options: Option<MediaProcessingOptions>,
    ) -> Result<([u8; 32], [u8; 32], [u8; 12])> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session
            .groups()
            .media()
            .upload_group_image(group_id, file_path, blossom_server_url, options)
            .await
    }

    #[deprecated(note = "Use AccountSession::groups().media().upload_chat_media() instead.")]
    #[perf_instrument("media")]
    pub async fn upload_chat_media(
        &self,
        account: &Account,
        group_id: &GroupId,
        file_path: &str,
        blossom_server_url: Option<Url>,
        options: Option<MediaProcessingOptions>,
    ) -> Result<MediaFile> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session
            .groups()
            .media()
            .upload_chat_media(group_id, file_path, blossom_server_url, options)
            .await
    }

    #[deprecated(note = "Use AccountSession::groups().media().download_chat_media() instead.")]
    #[perf_instrument("media")]
    pub async fn download_chat_media(
        &self,
        account: &Account,
        group_id: &GroupId,
        original_file_hash: &[u8; 32],
    ) -> Result<MediaFile> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session
            .groups()
            .media()
            .download_chat_media(group_id, original_file_hash)
            .await
    }

    #[deprecated(
        note = "Use AccountSession::groups().media().get_media_files_for_group() instead."
    )]
    #[perf_instrument("media")]
    pub async fn get_media_files_for_group(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Vec<MediaFile>> {
        let session = self
            .account_manager
            .get_session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        MediaFile::find_by_group(
            &session.account_db.inner.pool,
            &self.shared.database,
            &account.pubkey,
            group_id,
        )
        .await
    }

    #[deprecated(note = "Use AccountSession::groups().media().get_group_image_path() instead.")]
    #[perf_instrument("media")]
    pub async fn get_group_image_path(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Option<PathBuf>> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session
            .groups()
            .media()
            .get_group_image_path(group_id)
            .await
    }

    #[deprecated(note = "Use AccountSession::groups().media().resolve_group_image_path() instead.")]
    #[perf_instrument("media")]
    pub(crate) async fn resolve_group_image_path(
        &self,
        account: &Account,
        group: &group_types::Group,
    ) -> Result<Option<PathBuf>> {
        let session = self
            .session(&account.pubkey)
            .ok_or(WhitenoiseError::AccountNotFound)?;
        session
            .groups()
            .media()
            .resolve_group_image_path(group)
            .await
    }
}

/// Blossom client that enforces HTTPS on the server URL.
pub(crate) fn blossom_client(url: &Url) -> Result<BlossomClient> {
    require_https(url)?;
    Ok(BlossomClient::new(url.clone()))
}

pub(crate) fn is_debug_local_blossom_url(url: &Url) -> bool {
    if !cfg!(debug_assertions) {
        return false;
    }

    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

/// Rejects non-HTTPS Blossom URLs to prevent cleartext metadata leakage.
/// Debug builds also allow loopback `http://` URLs for local testing.
pub(crate) fn require_https(url: &Url) -> Result<()> {
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_debug_local_blossom_url(url) => Ok(()),
        _ => Err(WhitenoiseError::BlossomInsecureUrl(url.to_string())),
    }
}
