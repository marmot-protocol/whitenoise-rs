use std::path::{Path, PathBuf};
use std::time::Duration;

use mdk_core::encrypted_media::types::MediaReference;
use mdk_core::extension::group_image;
use mdk_core::media_processing::MediaProcessingOptions;
use mdk_core::prelude::{GroupId, group_types};
use mdk_storage_traits::Secret;
use nostr_blossom::client::BlossomClient;
use nostr_sdk::prelude::hashes::{Hash, sha256::Hash as Sha256Hash};
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};

use crate::perf_instrument;
use crate::types::ImageType;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::accounts::Account;
use crate::whitenoise::database::media_files::{FileMetadata, MediaFile};
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::media_files::MediaFileUpload;

impl Whitenoise {
    /// Default timeout for Blossom HTTP operations (download and upload)
    /// Set to 300 seconds to accommodate large image files over slow connections
    pub(crate) const BLOSSOM_TIMEOUT: Duration = Duration::from_secs(300);

    /// Returns the default Blossom server URL based on build configuration
    ///
    /// In debug builds, uses localhost:3000 for local testing.
    /// In release builds, uses the production Blossom server.
    fn default_blossom_url() -> Url {
        let url = if cfg!(debug_assertions) {
            "http://localhost:3000"
        } else {
            "https://blossom.primal.net"
        };
        Url::parse(url).expect("Hardcoded Blossom URL should be valid")
    }

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
    #[perf_instrument("media")]
    pub(crate) async fn sync_group_image_cache_if_needed(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<()> {
        let group: group_types::Group;
        {
            // Get group data to check if it has an image
            let mdk = self.create_mdk_for_account(account.pubkey)?;
            group = mdk
                .get_group(group_id)
                .map_err(WhitenoiseError::from)?
                .ok_or(WhitenoiseError::GroupNotFound)?;
        }

        // Check if group has an image set
        let (image_hash, image_key, image_nonce) =
            match (group.image_hash, group.image_key, group.image_nonce) {
                (Some(hash), Some(key), Some(nonce)) => (hash, key, nonce),
                _ => return Ok(()), // No image set, nothing to do
            };

        // Try to get the stored blossom_url from the database
        let blossom_url = if let Some(media_file) =
            crate::whitenoise::database::media_files::MediaFile::find_by_hash(
                &self.database,
                &image_hash,
            )
            .await?
        {
            media_file
                .blossom_url
                .and_then(|url_str| Url::parse(&url_str).ok())
        } else {
            None
        };

        // Download and cache the image
        self.download_and_cache_group_image(
            blossom_url,
            &account.pubkey,
            group_id,
            &image_hash,
            &image_key,
            &image_nonce,
        )
        .await?;

        Ok(())
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
        account: &Account,
        group_id: &GroupId,
    ) {
        let account_clone = account.clone();
        let group_id_clone = group_id.clone();
        tokio::spawn(async move {
            let whitenoise = match Whitenoise::get_instance() {
                Ok(wn) => wn,
                Err(e) => {
                    tracing::error!(
                        target: "whitenoise::groups::background_sync_group_image_cache_if_needed",
                        "Failed to get Whitenoise instance for background image cache: {}",
                        e
                    );
                    return;
                }
            };

            if let Err(e) = whitenoise
                .sync_group_image_cache_if_needed(&account_clone, &group_id_clone)
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
    #[perf_instrument("media")]
    pub async fn upload_group_image(
        &self,
        account: &Account,
        group_id: &GroupId,
        file_path: &str,
        blossom_server_url: Option<Url>,
        options: Option<MediaProcessingOptions>,
    ) -> Result<([u8; 32], [u8; 32], [u8; 12])> {
        let admins = self.group_admins(account, group_id).await?;
        if !admins.contains(&account.pubkey) {
            return Err(WhitenoiseError::AccountNotAuthorized);
        }

        let image_data = tokio::fs::read(file_path).await?;
        let image_type = ImageType::detect(&image_data).map_err(|e| {
            WhitenoiseError::UnsupportedMediaFormat(format!(
                "Failed to detect or validate image from {}: {}",
                file_path, e
            ))
        })?;

        let prepared =
            mdk_core::extension::group_image::prepare_group_image_for_upload_with_options(
                &image_data,
                image_type.mime_type(),
                &options.unwrap_or_default(),
            )
            .map_err(|e| {
                WhitenoiseError::Other(anyhow::anyhow!("Failed to prepare group image: {}", e))
            })?;

        let blossom_server_url = blossom_server_url.unwrap_or(Self::default_blossom_url());
        let descriptor = Self::upload_encrypted_blob_to_blossom(
            &blossom_server_url,
            prepared.encrypted_data.as_ref().clone(),
            image_type.mime_type(),
            &prepared.upload_keypair,
        )
        .await?;

        let returned_hash_bytes: [u8; 32] = *descriptor.sha256.as_ref();
        if returned_hash_bytes != prepared.encrypted_hash {
            return Err(WhitenoiseError::HashMismatch {
                expected: hex::encode(prepared.encrypted_hash),
                actual: hex::encode(returned_hash_bytes),
            });
        }

        let hash_hex = hex::encode(prepared.encrypted_hash);
        let filename = format!("{}.{}", hash_hex, image_type.extension());

        let mut hasher = sha2::Sha256::new();
        sha2::Digest::update(&mut hasher, &image_data);
        let original_hash: [u8; 32] = hasher.finalize().into();

        let upload = MediaFileUpload {
            data: &image_data,
            original_file_hash: Some(&original_hash),
            encrypted_file_hash: prepared.encrypted_hash,
            mime_type: image_type.mime_type(),
            media_type: "group_image",
            blossom_url: Some(descriptor.url.as_str()),
            nostr_key: Some(prepared.upload_keypair.secret_key().to_secret_hex()),
            file_metadata: None,
            nonce: None,
            scheme_version: None,
        };

        if let Err(e) = self
            .media_files()
            .store_and_record(&account.pubkey, group_id, &filename, upload)
            .await
        {
            tracing::warn!(
                target: "whitenoise::groups::upload_group_image",
                "Failed to cache uploaded group image: {}. Image will be downloaded on next access.",
                e
            );
        }

        Ok((
            prepared.encrypted_hash,
            *prepared.image_key.as_ref(),
            *prepared.image_nonce.as_ref(),
        ))
    }

    #[perf_instrument("media")]
    pub async fn upload_chat_media(
        &self,
        account: &Account,
        group_id: &GroupId,
        file_path: &str,
        blossom_server_url: Option<Url>,
        options: Option<MediaProcessingOptions>,
    ) -> Result<MediaFile> {
        let file_data = tokio::fs::read(file_path).await?;
        let media_detection = crate::types::detect_media_type(&file_data)?;

        let original_filename = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| WhitenoiseError::Other(anyhow::anyhow!("Invalid file path")))?;

        let prepared = {
            let mdk = self.create_mdk_for_account(account.pubkey)?;
            let media_manager = mdk.media_manager(group_id.clone());

            media_manager
                .encrypt_for_upload_with_options(
                    &file_data,
                    media_detection.mime_type(),
                    original_filename,
                    &options.unwrap_or_default(),
                )
                .map_err(|e| {
                    WhitenoiseError::Other(anyhow::anyhow!("Failed to encrypt chat media: {}", e))
                })?
        };

        let blossom_server_url = blossom_server_url.unwrap_or_else(Self::default_blossom_url);
        let upload_keys = nostr_sdk::Keys::generate();
        let upload_keys_hex = upload_keys.secret_key().to_secret_hex();

        let descriptor = Self::upload_encrypted_blob_to_blossom(
            &blossom_server_url,
            prepared.encrypted_data,
            &prepared.mime_type,
            &upload_keys,
        )
        .await?;

        let returned_hash_bytes: [u8; 32] = *descriptor.sha256.as_ref();
        if returned_hash_bytes != prepared.encrypted_hash {
            return Err(WhitenoiseError::HashMismatch {
                expected: hex::encode(prepared.encrypted_hash),
                actual: hex::encode(returned_hash_bytes),
            });
        }

        let hash_hex = hex::encode(prepared.encrypted_hash);
        let cached_filename = format!("{}.{}", hash_hex, media_detection.extension());

        let file_metadata = if prepared.dimensions.is_some() || prepared.blurhash.is_some() {
            Some(FileMetadata {
                original_filename: Some(prepared.filename.clone()),
                dimensions: prepared.dimensions.map(|(w, h)| format!("{}x{}", w, h)),
                blurhash: prepared.blurhash.clone(),
            })
        } else {
            None
        };

        let upload = MediaFileUpload {
            data: &file_data,
            original_file_hash: Some(&prepared.original_hash),
            encrypted_file_hash: prepared.encrypted_hash,
            mime_type: &prepared.mime_type,
            media_type: "chat_media",
            blossom_url: Some(descriptor.url.as_str()),
            nostr_key: Some(upload_keys_hex),
            file_metadata: file_metadata.as_ref(),
            nonce: Some(hex::encode(prepared.nonce)),
            scheme_version: Some("mip04-v2"),
        };

        self.media_files()
            .store_and_record(&account.pubkey, group_id, &cached_filename, upload)
            .await
    }

    #[perf_instrument("media")]
    pub async fn download_chat_media(
        &self,
        account: &Account,
        group_id: &GroupId,
        original_file_hash: &[u8; 32],
    ) -> Result<MediaFile> {
        let media_file = MediaFile::find_by_original_hash_and_group(
            &self.database,
            original_file_hash,
            group_id,
            &account.pubkey,
        )
        .await?
        .ok_or_else(|| {
            WhitenoiseError::MediaCache(format!(
                "MediaFile not found for original_hash={} in group for account {}",
                hex::encode(original_file_hash),
                account.pubkey.to_hex()
            ))
        })?;

        if !media_file.file_path.as_os_str().is_empty() && media_file.file_path.exists() {
            return Ok(media_file);
        }

        if media_file.media_type != "chat_media" {
            return Err(WhitenoiseError::MediaCache(format!(
                "Not chat media: media_type={}",
                media_file.media_type
            )));
        }

        let decrypted_data = Self::download_and_decrypt_chat_media_blob(
            &account.pubkey,
            &self.config.data_dir,
            &self.config.keyring_service_id,
            group_id,
            &media_file,
            original_file_hash,
        )
        .await?;

        let media_detection = crate::types::detect_media_type(&decrypted_data)?;
        let hash_hex = hex::encode(&media_file.encrypted_file_hash);
        let cached_filename = format!("{}.{}", hash_hex, media_detection.extension());

        let cache_path = self
            .storage
            .media_files
            .store_file(&cached_filename, &decrypted_data)
            .await?;

        let media_file_id = media_file.id.ok_or_else(|| {
            WhitenoiseError::MediaCache("MediaFile record missing id".to_string())
        })?;

        MediaFile::update_file_path(&self.database, media_file_id, &cache_path).await
    }

    #[perf_instrument("media")]
    pub async fn get_media_files_for_group(&self, group_id: &GroupId) -> Result<Vec<MediaFile>> {
        MediaFile::find_by_group(&self.database, group_id).await
    }

    #[perf_instrument("media")]
    pub async fn get_group_image_path(
        &self,
        account: &Account,
        group_id: &GroupId,
    ) -> Result<Option<PathBuf>> {
        let mdk = self.create_mdk_for_account(account.pubkey)?;
        let group = mdk
            .get_group(group_id)
            .map_err(WhitenoiseError::from)?
            .ok_or(WhitenoiseError::GroupNotFound)?;

        self.resolve_group_image_path(account, &group).await
    }

    #[perf_instrument("media")]
    pub(crate) async fn resolve_group_image_path(
        &self,
        account: &Account,
        group: &group_types::Group,
    ) -> Result<Option<PathBuf>> {
        let (image_hash, image_key, image_nonce) =
            match (&group.image_hash, &group.image_key, &group.image_nonce) {
                (Some(hash), Some(key), Some(nonce)) => (hash, key, nonce),
                _ => return Ok(None),
            };

        let blossom_url = if let Some(media_file) =
            crate::whitenoise::database::media_files::MediaFile::find_by_hash(
                &self.database,
                image_hash,
            )
            .await?
        {
            media_file
                .blossom_url
                .and_then(|url_str| Url::parse(&url_str).ok())
        } else {
            None
        };

        let media_file = self
            .download_and_cache_group_image(
                blossom_url,
                &account.pubkey,
                &group.mls_group_id,
                image_hash,
                image_key.as_ref(),
                image_nonce.as_ref(),
            )
            .await?;

        Ok(Some(media_file.file_path))
    }

    /// Downloads, decrypts, and caches a group image if not already cached
    #[perf_instrument("media")]
    async fn download_and_cache_group_image(
        &self,
        blossom_url: Option<Url>,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        image_hash: &[u8; 32],
        image_key: &[u8; 32],
        image_nonce: &[u8; 12],
    ) -> Result<MediaFile> {
        let hash_hex = hex::encode(image_hash);

        if let Some(cached_path) = self.check_cached_image(&hash_hex).await? {
            let media_file = self
                .link_cached_image_to_group(
                    account_pubkey,
                    group_id,
                    &cached_path,
                    image_hash,
                    image_key,
                )
                .await?;
            return Ok(media_file);
        }

        let blossom_url = blossom_url.unwrap_or_else(Self::default_blossom_url);

        tracing::info!(
            target: "whitenoise::groups::download_and_cache_group_image",
            "Downloading group image {} for group {} from {}",
            hash_hex,
            hex::encode(group_id.as_slice()),
            blossom_url
        );

        let encrypted_data = Self::download_blob_from_blossom(&blossom_url, image_hash).await?;

        let secret_key = Secret::new(*image_key);
        let secret_nonce = Secret::new(*image_nonce);
        let decrypted_data = Self::decrypt_group_image(
            &encrypted_data,
            Some(image_hash),
            &secret_key,
            &secret_nonce,
        )?;
        let image_type = ImageType::detect(&decrypted_data).map_err(|e| {
            WhitenoiseError::UnsupportedMediaFormat(format!("Failed to detect image type: {}", e))
        })?;

        tracing::debug!(
            target: "whitenoise::groups::download_and_cache_group_image",
            "Detected image type: {} for group image {}",
            image_type.mime_type(),
            hash_hex
        );

        let media_file = self
            .store_and_record_group_image(
                account_pubkey,
                group_id,
                &decrypted_data,
                image_hash,
                image_key,
                &image_type,
                &blossom_url,
            )
            .await?;

        tracing::info!(
            target: "whitenoise::groups::download_and_cache_group_image",
            "Cached group image at: {}",
            media_file.file_path.display()
        );

        Ok(media_file)
    }

    #[perf_instrument("media")]
    async fn check_cached_image(&self, hash_hex: &str) -> Result<Option<PathBuf>> {
        let media_files = self.media_files();
        if let Some(cached_path) = media_files.find_file_with_prefix(hash_hex).await {
            tracing::debug!(
                target: "whitenoise::groups::check_cached_image",
                "Group image already cached at: {}",
                cached_path.display()
            );
            Ok(Some(cached_path))
        } else {
            Ok(None)
        }
    }

    #[perf_instrument("media")]
    async fn link_cached_image_to_group(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        cached_path: &Path,
        image_hash: &[u8; 32],
        image_key: &[u8; 32],
    ) -> Result<MediaFile> {
        let existing_record_opt = MediaFile::find_by_hash(&self.database, image_hash).await?;

        if let Some(existing_record) = existing_record_opt {
            self.link_cached_image_from_existing_record(
                account_pubkey,
                group_id,
                cached_path,
                image_hash,
                existing_record,
            )
            .await
        } else {
            self.link_cached_image_with_detection(
                account_pubkey,
                group_id,
                cached_path,
                image_hash,
                image_key,
            )
            .await
        }
    }

    #[perf_instrument("media")]
    async fn link_cached_image_from_existing_record(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        cached_path: &Path,
        image_hash: &[u8; 32],
        existing_record: crate::whitenoise::database::media_files::MediaFile,
    ) -> Result<MediaFile> {
        let metadata_ref = existing_record.file_metadata.as_ref();
        let original_hash_ref = existing_record
            .original_file_hash
            .as_ref()
            .and_then(|hash| hash.as_slice().try_into().ok());
        let upload = MediaFileUpload {
            data: &[],
            original_file_hash: original_hash_ref.as_ref(),
            encrypted_file_hash: *image_hash,
            mime_type: &existing_record.mime_type,
            media_type: &existing_record.media_type,
            blossom_url: existing_record.blossom_url.as_deref(),
            nostr_key: existing_record.nostr_key.clone(),
            file_metadata: metadata_ref,
            nonce: existing_record.nonce.as_deref().map(|s| s.to_string()),
            scheme_version: existing_record.scheme_version.as_deref(),
        };

        self.media_files()
            .record_in_database(account_pubkey, group_id, cached_path, upload)
            .await
    }

    #[perf_instrument("media")]
    async fn link_cached_image_with_detection(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        cached_path: &Path,
        image_hash: &[u8; 32],
        image_key: &[u8; 32],
    ) -> Result<MediaFile> {
        tracing::debug!(
            target: "whitenoise::groups::link_cached_image_with_detection",
            "No existing database record for hash {}, detecting MIME type from cached file",
            hex::encode(image_hash)
        );

        let file_data = tokio::fs::read(cached_path)
            .await
            .map_err(WhitenoiseError::from)?;

        let image_type = ImageType::detect(&file_data).map_err(|e| {
            WhitenoiseError::UnsupportedMediaFormat(format!(
                "Failed to detect image type for cached file {}: {}",
                cached_path.display(),
                e
            ))
        })?;

        let mut hasher = Sha256::new();
        hasher.update(&file_data);
        let original_hash: [u8; 32] = hasher.finalize().into();

        let secret_key = Secret::new(*image_key);
        let upload_keypair = group_image::derive_upload_keypair(&secret_key, 2).map_err(|e| {
            WhitenoiseError::Other(anyhow::anyhow!("Failed to derive upload keypair: {}", e))
        })?;

        let upload = MediaFileUpload {
            data: &[],
            original_file_hash: Some(&original_hash),
            encrypted_file_hash: *image_hash,
            mime_type: image_type.mime_type(),
            media_type: "group_image",
            blossom_url: None,
            nostr_key: Some(upload_keypair.secret_key().to_secret_hex()),
            file_metadata: None,
            nonce: None,
            scheme_version: None,
        };

        self.media_files()
            .record_in_database(account_pubkey, group_id, cached_path, upload)
            .await
    }

    /// Rejects non-HTTPS Blossom URLs to prevent cleartext metadata leakage.
    /// Debug builds also allow `http://localhost` for local testing.
    pub(crate) fn require_https(url: &Url) -> Result<()> {
        match url.scheme() {
            "https" => Ok(()),
            "http" if cfg!(debug_assertions) && url.host_str() == Some("localhost") => Ok(()),
            _ => Err(WhitenoiseError::BlossomInsecureUrl(url.to_string())),
        }
    }

    /// Blossom client that enforces HTTPS on the server URL.
    pub(crate) fn blossom_client(url: &Url) -> Result<BlossomClient> {
        Self::require_https(url)?;
        Ok(BlossomClient::new(url.clone()))
    }

    #[perf_instrument("media")]
    async fn download_blob_from_blossom(
        blossom_url: &Url,
        image_hash: &[u8; 32],
    ) -> Result<Vec<u8>> {
        let client = Self::blossom_client(blossom_url)?;
        let sha256 = Sha256Hash::from_slice(image_hash)
            .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!("Invalid SHA256 hash: {}", e)))?;

        let download_future = client.get_blob(sha256, None, None, None::<&Keys>);

        tokio::time::timeout(Self::BLOSSOM_TIMEOUT, download_future)
            .await
            .map_err(|_| {
                WhitenoiseError::BlossomDownload(format!(
                    "Download timed out after {} seconds",
                    Self::BLOSSOM_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|e| {
                WhitenoiseError::BlossomDownload(format!("Failed to download blob: {}", e))
            })
    }

    fn decrypt_group_image(
        encrypted_data: &[u8],
        expected_hash: Option<&[u8; 32]>,
        image_key: &Secret<[u8; 32]>,
        image_nonce: &Secret<[u8; 12]>,
    ) -> Result<Vec<u8>> {
        group_image::decrypt_group_image(encrypted_data, expected_hash, image_key, image_nonce)
            .map_err(|e| {
                WhitenoiseError::ImageDecryptionFailed(format!(
                    "Failed to decrypt group image: {}",
                    e
                ))
            })
    }

    #[perf_instrument("media")]
    async fn download_and_decrypt_chat_media_blob(
        account_pubkey: &PublicKey,
        data_dir: &Path,
        keyring_service_id: &str,
        group_id: &GroupId,
        media_file: &MediaFile,
        original_file_hash: &[u8; 32],
    ) -> Result<Vec<u8>> {
        let filename = media_file
            .file_metadata
            .as_ref()
            .and_then(|m| m.original_filename.as_deref())
            .ok_or_else(|| {
                WhitenoiseError::MediaCache("Missing required filename metadata".to_string())
            })?;
        let encrypted_hash: [u8; 32] = media_file
            .encrypted_file_hash
            .as_slice()
            .try_into()
            .map_err(|_| {
                WhitenoiseError::MediaCache("Invalid encrypted_file_hash length".to_string())
            })?;

        let blossom_url_str = media_file
            .blossom_url
            .as_ref()
            .ok_or_else(|| WhitenoiseError::MediaCache("No Blossom URL".to_string()))?;

        let blossom_url = Url::parse(blossom_url_str).map_err(|e| {
            WhitenoiseError::MediaCache(format!("Invalid Blossom URL '{}': {}", blossom_url_str, e))
        })?;

        tracing::debug!(
            target: "whitenoise::groups::download_and_decrypt",
            "Downloading encrypted blob from: {}",
            blossom_url
        );

        let encrypted_data =
            Self::download_blob_from_blossom(&blossom_url, &encrypted_hash).await?;

        let mdk = Account::create_mdk(*account_pubkey, data_dir, keyring_service_id)?;
        let media_manager = mdk.media_manager(group_id.clone());

        let nonce_hex = media_file.nonce.as_ref().ok_or_else(|| {
            WhitenoiseError::MediaCache("Missing nonce for chat media".to_string())
        })?;
        let scheme_version = media_file
            .scheme_version
            .as_ref()
            .ok_or_else(|| {
                WhitenoiseError::MediaCache("Missing scheme_version for chat media".to_string())
            })?
            .clone();

        let nonce_bytes = hex::decode(nonce_hex)
            .map_err(|_| WhitenoiseError::MediaCache("Invalid nonce hex".to_string()))?;
        let nonce: [u8; 12] = nonce_bytes
            .try_into()
            .map_err(|_| WhitenoiseError::MediaCache("Invalid nonce length".to_string()))?;

        let reference = MediaReference {
            url: String::new(),
            original_hash: *original_file_hash,
            mime_type: media_file.mime_type.clone(),
            filename: filename.to_string(),
            dimensions: None,
            scheme_version,
            nonce,
        };

        let decrypted = media_manager
            .decrypt_from_download(&encrypted_data, &reference)
            .map_err(|e| {
                WhitenoiseError::Other(anyhow::anyhow!("Failed to decrypt chat media: {}", e))
            })?;

        // MIP-04 requires an explicit SHA-256 check on the plaintext after decryption.
        // ChaCha20-Poly1305 authenticates the ciphertext, but does not guarantee the
        // decrypted output matches the original file — a different plaintext encrypted
        // under a valid derived key would pass AEAD authentication. Verify here.
        let mut hasher = sha2::Sha256::new();
        sha2::Digest::update(&mut hasher, &decrypted);
        let actual_hash: [u8; 32] = hasher.finalize().into();
        if actual_hash != *original_file_hash {
            return Err(WhitenoiseError::HashMismatch {
                expected: hex::encode(original_file_hash),
                actual: hex::encode(actual_hash),
            });
        }

        Ok(decrypted)
    }

    #[perf_instrument("media")]
    async fn store_and_record_group_image(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        decrypted_data: &[u8],
        image_hash: &[u8; 32],
        image_key: &[u8; 32],
        image_type: &ImageType,
        blossom_server: &Url,
    ) -> Result<MediaFile> {
        let hash_hex = hex::encode(image_hash);
        let filename = format!("{}.{}", hash_hex, image_type.extension());
        let blossom_url = blossom_server.join(&hash_hex).map_err(|e| {
            WhitenoiseError::Other(anyhow::anyhow!("Failed to construct Blossom URL: {}", e))
        })?;

        let mut hasher = Sha256::new();
        hasher.update(decrypted_data);
        let original_hash: [u8; 32] = hasher.finalize().into();

        let secret_key = Secret::new(*image_key);
        let upload_keypair = group_image::derive_upload_keypair(&secret_key, 2).map_err(|e| {
            WhitenoiseError::Other(anyhow::anyhow!("Failed to derive upload keypair: {}", e))
        })?;

        let upload = MediaFileUpload {
            data: decrypted_data,
            original_file_hash: Some(&original_hash),
            encrypted_file_hash: *image_hash,
            mime_type: image_type.mime_type(),
            media_type: "group_image",
            blossom_url: Some(blossom_url.as_str()),
            nostr_key: Some(upload_keypair.secret_key().to_secret_hex()),
            file_metadata: None,
            nonce: None,
            scheme_version: None,
        };

        self.media_files()
            .store_and_record(account_pubkey, group_id, &filename, upload)
            .await
    }

    #[perf_instrument("media")]
    async fn upload_encrypted_blob_to_blossom(
        blossom_server_url: &Url,
        encrypted_data: Vec<u8>,
        mime_type: &str,
        upload_keypair: &Keys,
    ) -> Result<nostr_blossom::bud02::BlobDescriptor> {
        let client = Self::blossom_client(blossom_server_url)?;
        let upload_future = client.upload_blob(
            encrypted_data,
            Some(mime_type.to_string()),
            None,
            Some(upload_keypair),
        );

        let descriptor = tokio::time::timeout(Self::BLOSSOM_TIMEOUT, upload_future)
            .await
            .map_err(|_| {
                WhitenoiseError::Other(anyhow::anyhow!(
                    "Upload timed out after {} seconds",
                    Self::BLOSSOM_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|err| WhitenoiseError::Other(anyhow::anyhow!(err)))?;

        Self::require_https(&descriptor.url)?;

        Ok(descriptor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blossom_client_accepts_https() {
        let url = Url::parse("https://blossom.primal.net").unwrap();
        assert!(Whitenoise::blossom_client(&url).is_ok());
    }

    #[test]
    fn blossom_client_rejects_http() {
        let url = Url::parse("http://evil.example.com").unwrap();
        let err = Whitenoise::blossom_client(&url).unwrap_err();
        assert!(
            matches!(err, WhitenoiseError::BlossomInsecureUrl(_)),
            "Expected BlossomInsecureUrl, got: {err:?}"
        );
    }

    #[test]
    fn blossom_client_rejects_ftp() {
        let url = Url::parse("ftp://files.example.com/blob").unwrap();
        assert!(Whitenoise::blossom_client(&url).is_err());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn blossom_client_allows_localhost_http_in_debug() {
        let url = Url::parse("http://localhost:3000").unwrap();
        assert!(Whitenoise::blossom_client(&url).is_ok());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn blossom_client_rejects_non_localhost_http_in_debug() {
        let url = Url::parse("http://192.168.1.1:3000").unwrap();
        assert!(Whitenoise::blossom_client(&url).is_err());
    }
}
