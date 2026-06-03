//! Group media operations scoped to an [`AccountSession`].

use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use base64ct::{Base64, Encoding as _};
use cgka_traits::types::GroupId as MarmotGroupId;
use futures::StreamExt;
use nostr_blossom::bud01::{
    BlossomAuthorization, BlossomAuthorizationScope, BlossomAuthorizationVerb,
    BlossomBuilderExtension,
};
use nostr_sdk::prelude::hashes::Hash;
use nostr_sdk::prelude::hashes::sha256::Hash as Sha256Hash;
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};

use crate::marmot::media::{
    ChatMediaReference, ENCRYPTED_MEDIA_VERSION, decrypt_chat_media, encrypt_chat_media,
};
use crate::marmot::{Group, GroupId};
use crate::perf_instrument;
use crate::whitenoise::database::media_files::{FileMetadata, MediaFile};
use crate::whitenoise::error::{Result, WhitenoiseError};
use crate::whitenoise::groups::blossom_error::BlossomError;
use crate::whitenoise::groups::media::{is_debug_local_blossom_url, require_https};
use crate::whitenoise::media_files::{AudioMetadata, MediaFileUpload};
use crate::whitenoise::media_processing::{
    MediaProcessingOptions, process_for_chat_media_encryption,
};
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
            "http" if is_debug_local_blossom_url(url) => true,
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

const DARKMATTER_MEDIA_UNSUPPORTED: &str =
    "Darkmatter group image operations require a group image component implementation";

#[derive(Debug)]
struct PreparedChatMedia {
    plaintext_data: Vec<u8>,
    encrypted_data: Vec<u8>,
    original_hash: [u8; 32],
    encrypted_hash: [u8; 32],
    mime_type: String,
    filename: String,
    dimensions: Option<(u32, u32)>,
    blurhash: Option<String>,
    thumbhash: Option<String>,
    duration_ms: Option<u64>,
    waveform: Option<Vec<u8>>,
    nonce: [u8; 12],
}

impl PreparedChatMedia {
    fn from_darkmatter(
        encrypted: crate::marmot::media::EncryptedChatMedia,
        plaintext_data: Vec<u8>,
        dimensions: Option<(u32, u32)>,
        blurhash: Option<String>,
        thumbhash: Option<String>,
    ) -> Self {
        Self {
            plaintext_data,
            encrypted_data: encrypted.encrypted_data,
            original_hash: encrypted.original_hash,
            encrypted_hash: encrypted.encrypted_hash,
            mime_type: encrypted.mime_type,
            filename: encrypted.filename,
            dimensions,
            blurhash,
            thumbhash,
            duration_ms: None,
            waveform: None,
            nonce: encrypted.nonce,
        }
    }
}

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

    /// Construct a `MediaFiles` orchestrator over the session's shared storage,
    /// shared database (media_blobs), and per-account database (media_references).
    fn media_files(&self) -> crate::whitenoise::media_files::MediaFiles<'_> {
        crate::whitenoise::media_files::MediaFiles::new(
            &self.session.shared.storage,
            &self.session.shared.database,
            &self.session.account_db.inner.pool,
        )
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

    /// Syncs group image cache if needed.
    ///
    /// Darkmatter group images need a first-class app component before this can
    /// download anything. Until then, projected Darkmatter groups no-op and
    /// legacy-only groups are treated as absent.
    #[perf_instrument("groups")]
    pub async fn sync_group_image_cache_if_needed(&self, group_id: &GroupId) -> Result<()> {
        if self.darkmatter_group_projection(group_id)?.is_some() {
            return Ok(());
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Uploads a group image to a Blossom server and returns the encrypted metadata.
    ///
    /// The returned metadata (hash, key, nonce) should be passed to `update_group_data`
    /// to update the group's image settings.
    #[perf_instrument("groups")]
    pub async fn upload_group_image(
        &self,
        group_id: &GroupId,
        _file_path: &str,
        _blossom_server_url: Option<Url>,
        _options: Option<MediaProcessingOptions>,
    ) -> Result<([u8; 32], [u8; 32], [u8; 12])> {
        if let Some(group) = self.darkmatter_group_projection(group_id)? {
            if !group.admin_pubkeys.contains(&self.session.account_pubkey) {
                return Err(WhitenoiseError::AccountNotAuthorized);
            }

            return Err(Self::darkmatter_media_unsupported("group image upload"));
        }

        Err(WhitenoiseError::GroupNotFound)
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
        self.upload_chat_media_with_audio_metadata(
            group_id,
            file_path,
            blossom_server_url,
            options,
            None,
        )
        .await
    }

    /// Uploads a chat media file and persists caller-supplied audio display metadata.
    #[perf_instrument("groups")]
    pub async fn upload_chat_media_with_audio_metadata(
        &self,
        group_id: &GroupId,
        file_path: &str,
        blossom_server_url: Option<Url>,
        options: Option<MediaProcessingOptions>,
        audio_metadata: Option<AudioMetadata>,
    ) -> Result<MediaFile> {
        if self.darkmatter_group_projection(group_id)?.is_none() {
            return Err(WhitenoiseError::GroupNotFound);
        }

        let file_data = tokio::fs::read(file_path).await?;
        let media_detection = crate::types::detect_media_type(&file_data)?;

        let original_filename = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| WhitenoiseError::Internal("Invalid file path".to_string()))?;

        let options = options.unwrap_or_default();
        let processed = process_for_chat_media_encryption(
            &file_data,
            media_detection.mime_type(),
            original_filename,
            &options,
        )?;
        let exporter_secret = self
            .darkmatter_encrypted_media_exporter_secret(group_id)
            .await?;
        let encrypted = encrypt_chat_media(
            exporter_secret.as_ref(),
            &processed.data,
            &processed.mime_type,
            original_filename,
        )?;
        let mut prepared = PreparedChatMedia::from_darkmatter(
            encrypted,
            processed.data,
            processed.dimensions,
            processed.blurhash,
            processed.thumbhash,
        );
        if let Some(metadata) = audio_metadata {
            prepared.duration_ms = metadata.duration_ms;
            prepared.waveform = metadata.waveform;
        }

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

        // MIP-04 always carries a filename, and the receiver needs it on the
        // download side to derive the same encryption parameters. Always
        // persist FileMetadata so MIP-04 decryption has the basename even when
        // dimensions/blurhash/thumbhash aren't extracted (e.g. minimal video
        // fixtures without parsed dimensions). Mirrors `store_media_references`,
        // which always populates the metadata.
        let file_metadata = Some(FileMetadata {
            original_filename: Some(prepared.filename.clone()),
            dimensions: prepared.dimensions.map(|(w, h)| format!("{}x{}", w, h)),
            blurhash: prepared.blurhash.clone(),
            thumbhash: prepared.thumbhash.clone(),
            duration_ms: prepared.duration_ms,
            waveform: prepared.waveform.clone(),
        });

        let upload = MediaFileUpload {
            data: &prepared.plaintext_data,
            original_file_hash: Some(&prepared.original_hash),
            encrypted_file_hash: prepared.encrypted_hash,
            mime_type: &prepared.mime_type,
            media_type: "chat_media",
            blossom_url: Some(descriptor.url.as_str()),
            nostr_key: Some(upload_keys_hex),
            file_metadata: file_metadata.as_ref(),
            nonce: Some(hex::encode(prepared.nonce)),
            scheme_version: Some(ENCRYPTED_MEDIA_VERSION),
        };

        self.media_files()
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
        if self.darkmatter_group_projection(group_id)?.is_none() {
            return Err(WhitenoiseError::GroupNotFound);
        }

        let media_file = MediaFile::find_by_original_hash_and_group(
            &self.session.account_db.inner.pool,
            &self.session.shared.database,
            &self.session.account_pubkey,
            original_file_hash,
            group_id,
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

        let cache_path = self
            .session
            .shared
            .storage
            .media_files
            .store_file(&cached_filename, &decrypted_data)
            .await?;

        let media_file_id = media_file.id.ok_or_else(|| {
            WhitenoiseError::MediaCache("MediaFile record missing id".to_string())
        })?;

        MediaFile::update_file_path(
            &self.session.account_db.inner.pool,
            &self.session.shared.database,
            &self.session.account_pubkey,
            media_file_id,
            &cache_path,
        )
        .await
    }

    /// Returns all media files for a group.
    #[perf_instrument("groups")]
    pub async fn get_media_files_for_group(&self, group_id: &GroupId) -> Result<Vec<MediaFile>> {
        MediaFile::find_by_group(
            &self.session.account_db.inner.pool,
            &self.session.shared.database,
            &self.session.account_pubkey,
            group_id,
        )
        .await
    }

    /// Returns the filesystem path of the cached group image, downloading if needed.
    #[perf_instrument("groups")]
    pub async fn get_group_image_path(&self, group_id: &GroupId) -> Result<Option<PathBuf>> {
        if self.darkmatter_group_projection(group_id)?.is_some() {
            return Ok(None);
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    /// Resolves the filesystem path for a group's image.
    ///
    /// Darkmatter group-image projection is intentionally unavailable until the
    /// app component exists. Legacy-only groups are treated as absent.
    #[perf_instrument("groups")]
    pub(crate) async fn resolve_group_image_path(&self, group: &Group) -> Result<Option<PathBuf>> {
        if self
            .darkmatter_group_projection(&group.mls_group_id)?
            .is_some()
        {
            return Ok(None);
        }

        Err(WhitenoiseError::GroupNotFound)
    }

    // ── Private helpers ───────────────────────────────────────────────

    fn darkmatter_group_projection(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<crate::marmot::MarmotCreatedGroupProjection>> {
        self.session
            .marmot_storage
            .find_group_projection(&MarmotGroupId::new(group_id.as_slice().to_vec()))
            .map_err(WhitenoiseError::from)
    }

    async fn darkmatter_encrypted_media_exporter_secret(
        &self,
        group_id: &GroupId,
    ) -> Result<cgka_traits::SecretBytes> {
        let marmot_group_id = MarmotGroupId::new(group_id.as_slice().to_vec());
        let marmot =
            self.session
                .marmot
                .as_ref()
                .ok_or(WhitenoiseError::MarmotSessionUnavailable(
                    self.session.account_pubkey,
                ))?;
        marmot
            .lock()
            .await
            .encrypted_media_exporter_secret(&marmot_group_id)
    }

    fn darkmatter_media_unsupported(operation: &str) -> WhitenoiseError {
        WhitenoiseError::UnsupportedMarmotOperation(format!(
            "{operation} is not available for Darkmatter groups yet; {DARKMATTER_MEDIA_UNSUPPORTED}"
        ))
    }

    /// Downloads, decrypts, and verifies a chat media blob.
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

        if self.darkmatter_group_projection(group_id)?.is_none() {
            return Err(WhitenoiseError::GroupNotFound);
        }

        tracing::debug!(
            target: "whitenoise::session::groups::media",
            "Downloading encrypted blob from: {}",
            blossom_url
        );

        let encrypted_data =
            Self::download_blob_from_blossom(&blossom_url, &encrypted_hash).await?;

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

        let actual_encrypted_hash: [u8; 32] = Sha256::digest(&encrypted_data).into();
        if actual_encrypted_hash != encrypted_hash {
            return Err(WhitenoiseError::HashMismatch {
                expected: hex::encode(encrypted_hash),
                actual: hex::encode(actual_encrypted_hash),
            });
        }

        let exporter_secret = self
            .darkmatter_encrypted_media_exporter_secret(group_id)
            .await?;
        let decrypted = decrypt_chat_media(
            exporter_secret.as_ref(),
            &encrypted_data,
            ChatMediaReference {
                original_hash: original_file_hash,
                mime_type: &media_file.mime_type,
                filename,
                scheme_version: &scheme_version,
                nonce,
            },
        )?;

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

    pub(crate) async fn download_blob_from_blossom(
        blossom_url: &Url,
        image_hash: &[u8; 32],
    ) -> Result<Vec<u8>> {
        // Enforce HTTPS before making any network contact.
        require_https(blossom_url)?;

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
        require_https(blossom_server_url)?;

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

        require_https(&descriptor.url)?;

        Ok(descriptor)
    }
}

#[cfg(test)]
mod tests {
    use mockito::Server;
    use nostr_sdk::prelude::Url;

    use super::MediaOps;
    use crate::whitenoise::error::WhitenoiseError;
    use crate::whitenoise::groups::media::blossom_client;

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
        assert!(blossom_client(&url).is_ok());
    }

    #[test]
    fn blossom_client_rejects_http() {
        let url = Url::parse("http://evil.example.com").unwrap();
        let err = blossom_client(&url).unwrap_err();
        assert!(
            matches!(err, WhitenoiseError::BlossomInsecureUrl(_)),
            "Expected BlossomInsecureUrl, got: {err:?}"
        );
    }

    #[test]
    fn blossom_client_rejects_ftp() {
        let url = Url::parse("ftp://files.example.com/blob").unwrap();
        assert!(blossom_client(&url).is_err());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn blossom_client_allows_localhost_http_in_debug() {
        let url = Url::parse("http://localhost:3000").unwrap();
        assert!(blossom_client(&url).is_ok());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn blossom_client_allows_loopback_http_in_debug() {
        let url = Url::parse("http://127.0.0.1:3000").unwrap();
        assert!(blossom_client(&url).is_ok());
    }

    #[test]
    #[cfg(debug_assertions)]
    fn blossom_client_rejects_non_localhost_http_in_debug() {
        let url = Url::parse("http://192.168.1.1:3000").unwrap();
        assert!(blossom_client(&url).is_err());
    }
}
