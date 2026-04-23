//! Group media operations scoped to an [`AccountSession`].

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use base64ct::{Base64, Encoding as _};
use futures::StreamExt;
use mdk_core::encrypted_media::types::MediaReference;
use mdk_core::extension::group_image;
use mdk_core::media_processing::MediaProcessingOptions;
use mdk_core::prelude::GroupId;
use mdk_storage_traits::Secret;
use nostr_blossom::bud01::{
    BlossomAuthorization, BlossomAuthorizationScope, BlossomAuthorizationVerb,
    BlossomBuilderExtension,
};
use nostr_sdk::prelude::hashes::Hash;
use nostr_sdk::prelude::hashes::sha256::Hash as Sha256Hash;
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};

use crate::perf_instrument;
use crate::types::ImageType;
use crate::whitenoise::Whitenoise;
use crate::whitenoise::database::media_files::{FileMetadata, MediaFile};
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::groups::blossom_error::BlossomError;
use crate::whitenoise::media_files::MediaFileUpload;
use crate::whitenoise::session::AccountSession;

/// Shared HTTP client for Blossom blob downloads.
///
/// `reqwest::Client` holds a connection pool, a DNS resolver, and TLS state.
/// Building it once here lets all downloads reuse keep-alive connections and
/// avoids repeated TLS handshakes.  The ring TLS provider is installed here
/// too, so the `install_default()` call happens exactly once.
static BLOSSOM_HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    // reqwest 0.13 with `rustls-no-provider` requires an explicit provider.
    // The `let _ =` suppresses the error when it is already installed (e.g.
    // during tests where multiple call sites race to install the same provider).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Use embedded Mozilla root certificates instead of rustls-platform-verifier.
    // The platform verifier requires Android-specific JNI initialization that is
    // not performed in the Flutter app, causing a panic on first HTTPS request.
    // Embedded roots work on all platforms and match nostr-blossom's approach.
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    // Custom redirect policy that mirrors `require_https()`:
    // - Always allow HTTPS redirect targets.
    // - In debug builds, also allow HTTP loopback hosts (for local test servers).
    // - Reject everything else to prevent HTTPS → HTTP downgrade attacks.
    // - Cap the chain at 10 hops to guard against redirect loops.
    let redirect_policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        let url = attempt.url();
        let allowed = match url.scheme() {
            "https" => true,
            // TODO(phase-16): Move is_debug_local_blossom_url to a free fn to remove Whitenoise coupling.
            "http" if Whitenoise::is_debug_local_blossom_url(url) => true,
            _ => false,
        };
        if allowed {
            attempt.follow()
        } else {
            let msg = format!("redirect to non-HTTPS URL is not allowed: {url}");
            attempt.error(msg)
        }
    });

    reqwest::Client::builder()
        .redirect(redirect_policy)
        .use_preconfigured_tls(tls_config)
        .build()
        .expect("Failed to build shared Blossom HTTP client")
});

/// View over [`AccountSession`] for group media operations.
///
/// Obtain via [`super::GroupOps::media`].
pub struct MediaOps<'a> {
    session: &'a AccountSession,
}

impl<'a> MediaOps<'a> {
    pub(super) fn new(session: &'a AccountSession) -> Self {
        Self { session }
    }

    // ── Singleton bridge ─────────────────────────────────────────────

    /// Obtain a `&'static Whitenoise` reference via the global singleton.
    ///
    /// Methods that need `storage`, `media_files()`, or `config` still reach
    /// the singleton until those fields migrate to `SharedServices`.
    // TODO(phase-16): Remove singleton bridge when storage and config move to session.
    fn wn() -> Result<&'static Whitenoise> {
        Whitenoise::get_instance()
            .map_err(|_| WhitenoiseError::Internal("Whitenoise singleton unavailable".to_string()))
    }

    // ── Constants ─────────────────────────────────────────────────────

    /// Default timeout for Blossom HTTP operations (download and upload).
    /// Set to 300 seconds to accommodate large image files over slow connections.
    pub(crate) const BLOSSOM_TIMEOUT: Duration = Duration::from_secs(300);

    /// Maximum number of bytes accepted for a single blob download (100 MiB).
    ///
    /// This cap applies both to the `Content-Length` header check (early rejection)
    /// and to the streaming byte counter (guards against servers that lie about or
    /// omit the header).  A 100 MiB limit is generous for encrypted media files
    /// while still bounding worst-case memory pressure.
    const MAX_BLOB_BYTES: usize = 100 * 1024 * 1024; // 100 MiB

    // ── Public operations ─────────────────────────────────────────────

    /// Syncs group image cache if needed (smart, hash-based check).
    ///
    /// This method is called after processing welcomes and commits to proactively
    /// cache group images. It only downloads if:
    /// 1. The group has an image set
    /// 2. The image_hash is not already cached
    ///
    /// This ensures images are ready before the UI needs them, while avoiding
    /// redundant downloads.
    #[perf_instrument("groups")]
    pub async fn sync_group_image_cache_if_needed(&self, group_id: &GroupId) -> Result<()> {
        let group = self
            .session
            .mdk
            .get_group(group_id)
            .map_err(WhitenoiseError::from)?
            .ok_or(WhitenoiseError::GroupNotFound)?;

        // Check if group has an image set
        let (image_hash, image_key, image_nonce) =
            match (group.image_hash, group.image_key, group.image_nonce) {
                (Some(hash), Some(key), Some(nonce)) => (hash, key, nonce),
                _ => return Ok(()), // No image set, nothing to do
            };

        // Try to get the stored blossom_url from the database
        let blossom_url = if let Some(media_file) =
            MediaFile::find_by_hash(&self.session.database, &image_hash).await?
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
            group_id,
            &image_hash,
            &image_key,
            &image_nonce,
        )
        .await?;

        Ok(())
    }

    /// Uploads a group image to a Blossom server and returns the encrypted metadata.
    ///
    /// The returned metadata (hash, key, nonce) should be passed to `update_group_data`
    /// to update the group's image settings.
    #[perf_instrument("groups")]
    pub async fn upload_group_image(
        &self,
        group_id: &GroupId,
        file_path: &str,
        blossom_server_url: Option<Url>,
        options: Option<MediaProcessingOptions>,
    ) -> Result<([u8; 32], [u8; 32], [u8; 12])> {
        let admins: Vec<PublicKey> = self
            .session
            .mdk
            .get_group(group_id)
            .map_err(WhitenoiseError::from)?
            .ok_or(WhitenoiseError::GroupNotFound)?
            .admin_pubkeys
            .into_iter()
            .collect();
        if !admins.contains(&self.session.account_pubkey) {
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
            )?;

        let blossom_server_url = blossom_server_url.unwrap_or_else(Self::default_blossom_url);
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

        // TODO(phase-16): Remove singleton bridge when storage moves to session.
        match Self::wn() {
            Err(e) => {
                tracing::warn!(
                    target: "whitenoise::session::groups::media",
                    "Failed to get singleton for image cache ({}). Image will be downloaded on next access.",
                    e
                );
            }
            Ok(wn) => {
                if let Err(e) = wn
                    .media_files()
                    .store_and_record(&self.session.account_pubkey, group_id, &filename, upload)
                    .await
                {
                    tracing::warn!(
                        target: "whitenoise::session::groups::media",
                        "Failed to cache uploaded group image: {}. Image will be downloaded on next access.",
                        e
                    );
                }
            }
        }

        Ok((
            prepared.encrypted_hash,
            *prepared.image_key.as_ref(),
            *prepared.image_nonce.as_ref(),
        ))
    }

    /// Uploads a chat media file to a Blossom server, encrypts it, and records it in the database.
    #[perf_instrument("groups")]
    pub async fn upload_chat_media(
        &self,
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
            .ok_or_else(|| WhitenoiseError::Internal("Invalid file path".to_string()))?;

        let prepared = {
            let media_manager = self.session.mdk.media_manager(group_id.clone());

            media_manager.encrypt_for_upload_with_options(
                &file_data,
                media_detection.mime_type(),
                original_filename,
                &options.unwrap_or_default(),
            )?
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

        let file_metadata = if prepared.dimensions.is_some()
            || prepared.blurhash.is_some()
            || prepared.thumbhash.is_some()
        {
            Some(FileMetadata {
                original_filename: Some(prepared.filename.clone()),
                dimensions: prepared.dimensions.map(|(w, h)| format!("{}x{}", w, h)),
                blurhash: prepared.blurhash.clone(),
                thumbhash: prepared.thumbhash.clone(),
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

        // TODO(phase-16): Remove singleton bridge when storage moves to session.
        let wn = Self::wn()?;
        wn.media_files()
            .store_and_record(
                &self.session.account_pubkey,
                group_id,
                &cached_filename,
                upload,
            )
            .await
    }

    /// Downloads and decrypts a chat media file, caching it locally.
    #[perf_instrument("groups")]
    pub async fn download_chat_media(
        &self,
        group_id: &GroupId,
        original_file_hash: &[u8; 32],
    ) -> Result<MediaFile> {
        let media_file = MediaFile::find_by_original_hash_and_group(
            &self.session.database,
            original_file_hash,
            group_id,
            &self.session.account_pubkey,
        )
        .await?
        .ok_or_else(|| {
            WhitenoiseError::MediaCache(format!(
                "MediaFile not found for original_hash={} in group for account {}",
                hex::encode(original_file_hash),
                self.session.account_pubkey.to_hex()
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

        let decrypted_data = self
            .decrypt_downloaded_chat_media_blob(group_id, &media_file, original_file_hash)
            .await?;

        let media_detection = crate::types::detect_media_type(&decrypted_data)?;
        let hash_hex = hex::encode(&media_file.encrypted_file_hash);
        let cached_filename = format!("{}.{}", hash_hex, media_detection.extension());

        // TODO(phase-16): Remove singleton bridge when storage moves to session.
        let wn = Self::wn()?;
        let cache_path = wn
            .storage
            .media_files
            .store_file(&cached_filename, &decrypted_data)
            .await?;

        let media_file_id = media_file.id.ok_or_else(|| {
            WhitenoiseError::MediaCache("MediaFile record missing id".to_string())
        })?;

        MediaFile::update_file_path(&self.session.database, media_file_id, &cache_path).await
    }

    /// Returns all media files for a group.
    #[perf_instrument("groups")]
    pub async fn get_media_files_for_group(&self, group_id: &GroupId) -> Result<Vec<MediaFile>> {
        MediaFile::find_by_group(&self.session.database, group_id).await
    }

    /// Returns the filesystem path of the cached group image, downloading if needed.
    #[perf_instrument("groups")]
    pub async fn get_group_image_path(&self, group_id: &GroupId) -> Result<Option<PathBuf>> {
        let group = self
            .session
            .mdk
            .get_group(group_id)
            .map_err(WhitenoiseError::from)?
            .ok_or(WhitenoiseError::GroupNotFound)?;

        self.resolve_group_image_path(&group).await
    }

    /// Resolves the filesystem path for a group's image, downloading and caching if needed.
    #[perf_instrument("groups")]
    pub(crate) async fn resolve_group_image_path(
        &self,
        group: &mdk_core::prelude::group_types::Group,
    ) -> Result<Option<PathBuf>> {
        let (image_hash, image_key, image_nonce) =
            match (&group.image_hash, &group.image_key, &group.image_nonce) {
                (Some(hash), Some(key), Some(nonce)) => (hash, key, nonce),
                _ => return Ok(None),
            };

        let blossom_url = if let Some(media_file) =
            MediaFile::find_by_hash(&self.session.database, image_hash).await?
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
                &group.mls_group_id,
                image_hash,
                image_key.as_ref(),
                image_nonce.as_ref(),
            )
            .await?;

        Ok(Some(media_file.file_path))
    }

    // ── Private helpers ───────────────────────────────────────────────

    /// Downloads, decrypts, and caches a group image if not already cached.
    #[perf_instrument("groups")]
    async fn download_and_cache_group_image(
        &self,
        blossom_url: Option<Url>,
        group_id: &GroupId,
        image_hash: &[u8; 32],
        image_key: &[u8; 32],
        image_nonce: &[u8; 12],
    ) -> Result<MediaFile> {
        let hash_hex = hex::encode(image_hash);

        if let Some(cached_path) = self.check_cached_image(&hash_hex).await? {
            let media_file = self
                .link_cached_image_to_group(group_id, &cached_path, image_hash, image_key)
                .await?;
            return Ok(media_file);
        }

        let blossom_url = blossom_url.unwrap_or_else(Self::default_blossom_url);

        tracing::info!(
            target: "whitenoise::session::groups::media",
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
            target: "whitenoise::session::groups::media",
            "Detected image type: {} for group image {}",
            image_type.mime_type(),
            hash_hex
        );

        let media_file = self
            .store_and_record_group_image(
                group_id,
                &decrypted_data,
                image_hash,
                image_key,
                &image_type,
                &blossom_url,
            )
            .await?;

        tracing::info!(
            target: "whitenoise::session::groups::media",
            "Cached group image at: {}",
            media_file.file_path.display()
        );

        Ok(media_file)
    }

    /// Checks whether a group image is already cached on disk.
    async fn check_cached_image(&self, hash_hex: &str) -> Result<Option<PathBuf>> {
        // TODO(phase-16): Remove singleton bridge when storage moves to session.
        let wn = Self::wn()?;
        let media_files = wn.media_files();
        if let Some(cached_path) = media_files.find_file_with_prefix(hash_hex).await {
            tracing::debug!(
                target: "whitenoise::session::groups::media",
                "Group image already cached at: {}",
                cached_path.display()
            );
            Ok(Some(cached_path))
        } else {
            Ok(None)
        }
    }

    async fn link_cached_image_to_group(
        &self,
        group_id: &GroupId,
        cached_path: &Path,
        image_hash: &[u8; 32],
        image_key: &[u8; 32],
    ) -> Result<MediaFile> {
        let existing_record_opt =
            MediaFile::find_by_hash(&self.session.database, image_hash).await?;

        match existing_record_opt {
            Some(existing_record) => {
                self.link_cached_image_from_existing_record(
                    group_id,
                    cached_path,
                    image_hash,
                    existing_record,
                )
                .await
            }
            None => {
                self.link_cached_image_with_detection(group_id, cached_path, image_hash, image_key)
                    .await
            }
        }
    }

    async fn link_cached_image_from_existing_record(
        &self,
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

        // TODO(phase-16): Remove singleton bridge when storage moves to session.
        let wn = Self::wn()?;
        wn.media_files()
            .record_in_database(&self.session.account_pubkey, group_id, cached_path, upload)
            .await
    }

    async fn link_cached_image_with_detection(
        &self,
        group_id: &GroupId,
        cached_path: &Path,
        image_hash: &[u8; 32],
        image_key: &[u8; 32],
    ) -> Result<MediaFile> {
        tracing::debug!(
            target: "whitenoise::session::groups::media",
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
        let upload_keypair = group_image::derive_upload_keypair(&secret_key, 2)?;

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

        // TODO(phase-16): Remove singleton bridge when storage moves to session.
        let wn = Self::wn()?;
        wn.media_files()
            .record_in_database(&self.session.account_pubkey, group_id, cached_path, upload)
            .await
    }

    async fn store_and_record_group_image(
        &self,
        group_id: &GroupId,
        decrypted_data: &[u8],
        image_hash: &[u8; 32],
        image_key: &[u8; 32],
        image_type: &ImageType,
        blossom_server: &Url,
    ) -> Result<MediaFile> {
        let hash_hex = hex::encode(image_hash);
        let filename = format!("{}.{}", hash_hex, image_type.extension());
        let blossom_url = blossom_server.join(&hash_hex).map_err(BlossomError::from)?;

        let mut hasher = Sha256::new();
        hasher.update(decrypted_data);
        let original_hash: [u8; 32] = hasher.finalize().into();

        let secret_key = Secret::new(*image_key);
        let upload_keypair = group_image::derive_upload_keypair(&secret_key, 2)?;

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

        // TODO(phase-16): Remove singleton bridge when storage moves to session.
        let wn = Self::wn()?;
        wn.media_files()
            .store_and_record(&self.session.account_pubkey, group_id, &filename, upload)
            .await
    }

    /// Downloads, decrypts, and verifies a chat media blob.
    ///
    /// Uses `self.session.mdk` directly (no `config.data_dir` or keyring needed).
    async fn decrypt_downloaded_chat_media_blob(
        &self,
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
            target: "whitenoise::session::groups::media",
            "Downloading encrypted blob from: {}",
            blossom_url
        );

        let encrypted_data =
            Self::download_blob_from_blossom(&blossom_url, &encrypted_hash).await?;

        let media_manager = self.session.mdk.media_manager(group_id.clone());

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

        let decrypted = media_manager.decrypt_from_download(&encrypted_data, &reference)?;

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

    // ── Static helpers ────────────────────────────────────────────────

    /// Returns the default Blossom server URL based on build configuration.
    fn default_blossom_url() -> Url {
        let url = if cfg!(debug_assertions) {
            "http://localhost:3000"
        } else {
            "https://blossom.primal.net"
        };
        Url::parse(url).expect("Hardcoded Blossom URL should be valid")
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

    pub(crate) async fn download_blob_from_blossom(
        blossom_url: &Url,
        image_hash: &[u8; 32],
    ) -> Result<Vec<u8>> {
        // Enforce HTTPS before making any network contact.
        // TODO(phase-16): Move require_https to a free fn to remove Whitenoise coupling.
        Whitenoise::require_https(blossom_url)?;

        // Build the Blossom download URL: <base>/<hex-hash>
        let hash_hex = hex::encode(image_hash);
        let download_url = blossom_url.join(&hash_hex).map_err(|e| {
            WhitenoiseError::BlossomDownload(format!("Failed to build download URL: {}", e))
        })?;

        tokio::time::timeout(Self::BLOSSOM_TIMEOUT, async {
            let response = BLOSSOM_HTTP_CLIENT
                .get(download_url)
                .send()
                .await
                .map_err(|e| {
                    WhitenoiseError::BlossomDownload(format!("HTTP request failed: {}", e))
                })?;

            if !response.status().is_success() {
                return Err(WhitenoiseError::BlossomDownload(format!(
                    "Server returned status {}",
                    response.status()
                )));
            }

            let hint_capacity = match response.content_length() {
                Some(cl) => Self::check_content_length(cl, Self::MAX_BLOB_BYTES)?,
                None => 0,
            };

            let mut body: Vec<u8> = Vec::with_capacity(hint_capacity);
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|e| {
                    WhitenoiseError::BlossomDownload(format!("Error reading response body: {}", e))
                })?;
                if chunk.len() > Self::MAX_BLOB_BYTES - body.len() {
                    return Err(WhitenoiseError::DownloadSizeLimitExceeded {
                        limit: Self::MAX_BLOB_BYTES,
                    });
                }
                body.extend_from_slice(&chunk);
            }

            Ok(body)
        })
        .await
        .map_err(|_| {
            WhitenoiseError::BlossomDownload(format!(
                "Download timed out after {} seconds",
                Self::BLOSSOM_TIMEOUT.as_secs()
            ))
        })?
    }

    pub(crate) fn check_content_length(content_length: u64, limit: usize) -> Result<usize> {
        let content_length = usize::try_from(content_length).unwrap_or(usize::MAX);
        if content_length > limit {
            return Err(WhitenoiseError::DownloadSizeLimitExceeded { limit });
        }
        Ok(content_length)
    }

    async fn upload_encrypted_blob_to_blossom(
        blossom_server_url: &Url,
        encrypted_data: Vec<u8>,
        mime_type: &str,
        upload_keypair: &Keys,
    ) -> Result<nostr_blossom::bud02::BlobDescriptor> {
        // TODO(phase-16): Move require_https to a free fn to remove Whitenoise coupling.
        Whitenoise::require_https(blossom_server_url)?;

        let sha256 = Sha256Hash::hash(&encrypted_data);
        let expiration = Timestamp::now() + Duration::from_secs(300);
        let auth = BlossomAuthorization::new(
            "Blossom upload authorization".to_string(),
            expiration,
            BlossomAuthorizationVerb::Upload,
            BlossomAuthorizationScope::BlobSha256Hashes(vec![sha256]),
        );
        let auth_event = EventBuilder::blossom_auth(auth)
            .sign(upload_keypair)
            .await
            .map_err(BlossomError::from)?;
        let auth_json = auth_event.as_json();
        let auth_header_value = format!("Nostr {}", Base64::encode_string(auth_json.as_bytes()));

        let upload_url = blossom_server_url
            .join("upload")
            .map_err(BlossomError::from)?;

        let upload_future = async {
            let response = BLOSSOM_HTTP_CLIENT
                .put(upload_url)
                .header(reqwest::header::CONTENT_TYPE, mime_type)
                .header(reqwest::header::AUTHORIZATION, &auth_header_value)
                .body(encrypted_data)
                .send()
                .await
                .map_err(BlossomError::from)?;

            if !response.status().is_success() {
                return Err(BlossomError::UploadStatus(response.status()).into());
            }

            response
                .json::<nostr_blossom::bud02::BlobDescriptor>()
                .await
                .map_err(|e| WhitenoiseError::from(BlossomError::ResponseBody(e)))
        };

        let descriptor = tokio::time::timeout(Self::BLOSSOM_TIMEOUT, upload_future)
            .await
            .map_err(|_| BlossomError::Timeout(Self::BLOSSOM_TIMEOUT))??;

        // TODO(phase-16): Move require_https to a free fn to remove Whitenoise coupling.
        Whitenoise::require_https(&descriptor.url)?;

        Ok(descriptor)
    }
}

#[cfg(test)]
mod tests {
    use mockito::Server;
    use nostr_sdk::prelude::Url;

    use super::MediaOps;
    use crate::whitenoise::Whitenoise;
    use crate::whitenoise::error::WhitenoiseError;

    // ── download_blob_from_blossom size-cap tests ─────────────────────────────
    //
    // These tests spin up a local mockito server and call the real streaming
    // path.  They run only in debug builds because `require_https` allows
    // loopback `http://` URLs in debug mode only.

    /// Returns a fixed 32-byte hash for use as a fake blob hash in tests.
    fn test_hash() -> [u8; 32] {
        [0xab; 32]
    }

    /// Returns the URL path that `download_blob_from_blossom` will request for
    /// `test_hash()`, i.e. `/<hex-encoded-hash>`.
    fn test_path() -> String {
        format!("/{}", hex::encode(test_hash()))
    }

    /// Returns a `localhost`-based URL for the mockito server.
    ///
    /// `mockito::Server` binds to a loopback address; using `localhost` keeps the
    /// test URL readable while still reaching the local mock server.
    fn localhost_url(server: &Server) -> Url {
        let port = server.socket_address().port();
        Url::parse(&format!("http://localhost:{port}")).unwrap()
    }

    /// Fast-path rejection: `Content-Length` header alone exceeds the limit.
    ///
    /// `check_content_length` is the pure function behind the fast-path guard.
    /// Testing it directly avoids the need for a mock HTTP server and verifies
    /// the logic independently of the network layer.
    #[test]
    fn content_length_guard_rejects_oversized_header() {
        let limit = MediaOps::MAX_BLOB_BYTES;
        let oversized = (limit + 1) as u64;
        let err = MediaOps::check_content_length(oversized, limit).unwrap_err();
        assert!(
            matches!(err, WhitenoiseError::DownloadSizeLimitExceeded { limit: l } if l == MediaOps::MAX_BLOB_BYTES),
            "Expected DownloadSizeLimitExceeded, got: {err:?}"
        );
    }

    /// Fast-path accepts a `Content-Length` exactly at the limit.
    #[test]
    fn content_length_guard_accepts_exact_limit() {
        let limit = MediaOps::MAX_BLOB_BYTES;
        let result = MediaOps::check_content_length(limit as u64, limit).unwrap();
        assert_eq!(result, limit);
    }

    /// Fast-path accepts a `Content-Length` below the limit.
    #[test]
    fn content_length_guard_accepts_small_value() {
        let result = MediaOps::check_content_length(1024, MediaOps::MAX_BLOB_BYTES).unwrap();
        assert_eq!(result, 1024);
    }

    /// Authoritative guard: server streams > 100 MiB with no `Content-Length`.
    ///
    /// The streaming byte counter must catch this even when the header is absent.
    /// `with_chunked_body` is used (not `with_body`) because mockito's `with_body`
    /// automatically sets `Content-Length`, which would trigger the fast-path header
    /// check instead of exercising the streaming byte counter.
    #[tokio::test]
    #[cfg(debug_assertions)]
    async fn download_rejects_oversized_streaming_body() {
        let mut server = mockito::Server::new_async().await;
        // One byte over the limit, sent as a chunked response without Content-Length.
        let oversized_body = vec![0u8; MediaOps::MAX_BLOB_BYTES + 1];
        let _mock = server
            .mock("GET", test_path().as_str())
            .with_status(200)
            .with_chunked_body(move |w| std::io::Write::write_all(w, &oversized_body))
            .create_async()
            .await;

        let url = localhost_url(&server);
        let err = MediaOps::download_blob_from_blossom(&url, &test_hash())
            .await
            .unwrap_err();

        assert!(
            matches!(
                err,
                WhitenoiseError::DownloadSizeLimitExceeded {
                    limit: MediaOps::MAX_BLOB_BYTES
                }
            ),
            "Expected DownloadSizeLimitExceeded, got: {err:?}"
        );
    }

    /// Happy path: a small response within the limit is returned successfully.
    #[tokio::test]
    #[cfg(debug_assertions)]
    async fn download_accepts_small_body() {
        let mut server = mockito::Server::new_async().await;
        let body = b"hello blossom";
        let _mock = server
            .mock("GET", test_path().as_str())
            .with_status(200)
            .with_body(&body[..])
            .create_async()
            .await;

        let url = localhost_url(&server);
        let result = MediaOps::download_blob_from_blossom(&url, &test_hash())
            .await
            .unwrap();

        assert_eq!(result, body);
    }

    // ── require_https / blossom_client tests ─────────────────────────────────

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
    fn blossom_client_allows_loopback_http_in_debug() {
        let url = Url::parse("http://127.0.0.1:3000").unwrap();
        assert!(Whitenoise::blossom_client(&url).is_ok());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn blossom_client_rejects_non_localhost_http_in_debug() {
        let url = Url::parse("http://192.168.1.1:3000").unwrap();
        assert!(Whitenoise::blossom_client(&url).is_err());
    }
}
