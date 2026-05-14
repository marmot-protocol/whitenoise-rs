use std::collections::HashSet;
use std::path::{Path, PathBuf};

use mdk_core::{
    GroupId,
    encrypted_media::{manager::EncryptedMediaManager, types::MediaReference},
    prelude::MdkStorageProvider,
};
use nostr_sdk::prelude::*;
use sqlx::SqlitePool;

pub use crate::whitenoise::database::media_files::MediaFile;
use crate::{
    perf_instrument,
    whitenoise::{
        database::{
            Database,
            media_files::{FileMetadata, MediaFileParams},
        },
        error::{Result, WhitenoiseError},
        storage::Storage,
    },
};

/// Display metadata for audio chat media.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default)]
pub struct AudioMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub waveform: Option<Vec<u8>>,
}

impl AudioMetadata {
    pub fn new(duration_ms: Option<u64>, waveform: Option<Vec<u8>>) -> Result<Self> {
        if let Some(samples) = waveform.as_deref()
            && !FileMetadata::is_valid_waveform(samples)
        {
            return Err(WhitenoiseError::InvalidInput(
                "waveform samples must be integers in the inclusive range 0..100".to_string(),
            ));
        }

        Ok(Self {
            duration_ms,
            waveform,
        })
    }

    fn from_file_metadata(metadata: &FileMetadata) -> Self {
        Self {
            duration_ms: metadata.duration_ms,
            waveform: metadata.waveform.clone(),
        }
    }

    fn is_empty(&self) -> bool {
        self.duration_ms.is_none() && self.waveform.is_none()
    }
}

/// Builds a MIP-04 `imeta` tag for encrypted chat media.
///
/// Required decryption fields (`url`, `m`, `filename`, `x`, `n`, `v`) are
/// strict. Optional display fields are included when present and valid.
pub fn build_chat_media_imeta_tag(
    media_file: &MediaFile,
    audio_metadata: Option<&AudioMetadata>,
) -> Result<Tag> {
    let blossom_url = media_file.blossom_url.as_deref().ok_or_else(|| {
        WhitenoiseError::InvalidInput("chat media is missing blossom_url".to_string())
    })?;
    let filename = media_file
        .file_metadata
        .as_ref()
        .and_then(|metadata| metadata.original_filename.as_deref())
        .ok_or_else(|| {
            WhitenoiseError::InvalidInput("chat media is missing filename metadata".to_string())
        })?;
    let original_file_hash = media_file.original_file_hash.as_deref().ok_or_else(|| {
        WhitenoiseError::InvalidInput("chat media is missing original_file_hash".to_string())
    })?;
    if original_file_hash.len() != 32 {
        return Err(WhitenoiseError::InvalidInput(format!(
            "chat media original_file_hash must be 32 bytes, got {}",
            original_file_hash.len()
        )));
    }
    let nonce = media_file
        .nonce
        .as_deref()
        .ok_or_else(|| WhitenoiseError::InvalidInput("chat media is missing nonce".to_string()))?;
    let scheme_version = media_file.scheme_version.as_deref().ok_or_else(|| {
        WhitenoiseError::InvalidInput("chat media is missing scheme_version".to_string())
    })?;

    let mut tag_values = vec![
        format!("url {}", blossom_url),
        format!("m {}", media_file.mime_type),
        format!("filename {}", filename),
    ];

    if let Some(metadata) = media_file.file_metadata.as_ref() {
        if let Some(dimensions) = metadata.dimensions.as_ref() {
            tag_values.push(format!("dim {}", dimensions));
        }
        if let Some(blurhash) = metadata.blurhash.as_ref() {
            tag_values.push(format!("blurhash {}", blurhash));
        }
        if let Some(thumbhash) = metadata.thumbhash.as_ref() {
            tag_values.push(format!("thumbhash {}", thumbhash));
        }
    }

    tag_values.push(format!("x {}", hex::encode(original_file_hash)));
    tag_values.push(format!("n {}", nonce));
    tag_values.push(format!("v {}", scheme_version));

    let file_audio_metadata = media_file
        .file_metadata
        .as_ref()
        .map(AudioMetadata::from_file_metadata)
        .filter(|metadata| !metadata.is_empty());
    let effective_audio_metadata = audio_metadata.or(file_audio_metadata.as_ref());
    if let Some(metadata) = effective_audio_metadata {
        if let Some(duration_ms) = metadata.duration_ms {
            tag_values.push(format!("duration {}", format_duration_seconds(duration_ms)));
        }
        if let Some(waveform) = metadata.waveform.as_deref() {
            if !FileMetadata::is_valid_waveform(waveform) {
                return Err(WhitenoiseError::InvalidInput(
                    "waveform samples must be integers in the inclusive range 0..100".to_string(),
                ));
            }
            let samples = waveform
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(" ");
            tag_values.push(format!("waveform {}", samples));
        }
    }

    Ok(Tag::custom(TagKind::Custom("imeta".into()), tag_values))
}

fn format_duration_seconds(duration_ms: u64) -> String {
    let seconds = duration_ms / 1000;
    let millis = duration_ms % 1000;
    if millis == 0 {
        return seconds.to_string();
    }

    let mut fractional = format!("{:03}", millis);
    while fractional.ends_with('0') {
        fractional.pop();
    }
    format!("{}.{}", seconds, fractional)
}

/// Parsed media reference with additional fields not in MDK's MediaReference
///
/// Wraps MDK's MediaReference and adds fields we need that MDK doesn't parse
#[derive(Debug, Clone)]
pub(crate) struct ParsedMediaReference {
    /// MDK's parsed reference (url, original_hash, mime_type, filename, dimensions)
    reference: MediaReference,
    /// Encrypted hash extracted from Blossom URL (needed for our DB schema)
    encrypted_hash: [u8; 32],
    /// Blurhash for image preview (optional, not parsed by MDK)
    blurhash: Option<String>,
    /// Thumbhash for image preview (optional, not parsed by MDK)
    thumbhash: Option<String>,
    /// Audio duration in milliseconds (optional, not parsed by MDK 0.8)
    duration_ms: Option<u64>,
    /// Audio waveform samples normalized to 0..100 (optional, not parsed by MDK 0.8)
    waveform: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ParsedDisplayMetadata {
    blurhash: Option<String>,
    thumbhash: Option<String>,
    duration_ms: Option<u64>,
    waveform: Option<Vec<u8>>,
}

/// Extracts encrypted hash from Blossom URL
///
/// Blossom URL format: https://blossom.server/<hash>
/// The hash is the encrypted_file_hash (SHA-256 of encrypted blob)
///
/// # Returns
/// * `Ok([u8; 32])` - The encrypted file hash
/// * `Err(WhitenoiseError)` - If URL is malformed or hash is invalid
fn extract_hash_from_blossom_url(url: &str) -> Result<[u8; 32]> {
    let parsed_url = Url::parse(url).map_err(|e| {
        WhitenoiseError::InvalidInput(format!("Invalid Blossom URL '{}': {}", url, e))
    })?;

    let segments = parsed_url.path_segments().ok_or_else(|| {
        WhitenoiseError::InvalidInput(format!(
            "Invalid Blossom URL '{}': missing hash segment",
            url
        ))
    })?;

    // Collect non-empty segments and get the last one
    // (Filtering handles trailing slashes which create empty segments)
    let non_empty_segments: Vec<_> = segments.filter(|s| !s.is_empty()).collect();
    let last_segment = non_empty_segments
        .last()
        .ok_or_else(|| WhitenoiseError::InvalidInput("Blossom URL contains no hash".to_string()))?;

    // Strip file extension if present (e.g., "hash.png" -> "hash")
    // Blossom URLs may include extensions like hash.png, hash.jpg, etc.
    let hash_hex = match last_segment.rfind('.') {
        Some(dot_idx) => &last_segment[..dot_idx],
        None => last_segment,
    };

    if hash_hex.is_empty() {
        return Err(WhitenoiseError::InvalidInput(
            "Blossom URL contains no hash".to_string(),
        ));
    }

    let hash_bytes = hex::decode(hash_hex).map_err(|e| {
        WhitenoiseError::InvalidInput(format!(
            "Invalid hex in Blossom URL hash '{}': {}",
            hash_hex, e
        ))
    })?;

    let hash_len = hash_bytes.len();
    hash_bytes.try_into().map_err(|_| {
        WhitenoiseError::InvalidInput(format!(
            "Invalid hash length in Blossom URL: expected 32 bytes, got {}",
            hash_len
        ))
    })
}

fn parse_duration_ms(value: &str) -> Option<u64> {
    let (seconds, fractional) = match value.split_once('.') {
        Some((seconds, fractional)) => (seconds, Some(fractional)),
        None => (value, None),
    };
    if seconds.is_empty() || !seconds.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let seconds_ms = seconds.parse::<u64>().ok()?.checked_mul(1000)?;
    let fractional_ms = match fractional {
        Some(fractional) => {
            if fractional.is_empty()
                || fractional.len() > 3
                || !fractional.chars().all(|c| c.is_ascii_digit())
            {
                return None;
            }
            let mut padded = fractional.to_string();
            while padded.len() < 3 {
                padded.push('0');
            }
            padded.parse::<u64>().ok()?
        }
        None => 0,
    };

    seconds_ms.checked_add(fractional_ms)
}

fn parse_waveform(value: &str) -> Option<Vec<u8>> {
    let mut samples = Vec::new();
    for sample in value.split_whitespace() {
        let sample = sample.parse::<u8>().ok()?;
        if sample > 100 {
            return None;
        }
        samples.push(sample);
    }

    (!samples.is_empty()).then_some(samples)
}

/// Intermediate type for media file storage operations
///
/// This type abstracts over different MDK upload types (GroupImageUpload, EncryptedMediaUpload)
/// and provides a unified interface for storing media files.
pub(crate) struct MediaFileUpload<'a> {
    /// The decrypted file data to store
    pub data: &'a [u8],
    /// SHA-256 hash of the original/decrypted content (for MIP-04 x field, MDK key derivation)
    /// None for group images (which use key/nonce encryption), Some for chat media (MDK)
    pub original_file_hash: Option<&'a [u8; 32]>,
    /// SHA-256 hash of the encrypted file (for Blossom verification)
    pub encrypted_file_hash: [u8; 32],
    /// MIME type of the file
    pub mime_type: &'a str,
    /// Type of media (e.g., "group_image", "chat_media")
    pub media_type: &'a str,
    /// Optional Blossom URL where the encrypted file is stored
    pub blossom_url: Option<&'a str>,
    /// Optional Nostr key (hex-encoded secret key) used for upload authentication/cleanup
    /// For group images: deterministically derived from image_key (stored for convenience)
    /// For chat images: randomly generated per upload (must be stored)
    pub nostr_key: Option<String>,
    /// Optional file metadata (original filename, dimensions, blurhash, duration, etc.)
    pub file_metadata: Option<&'a FileMetadata>,
    /// Encryption nonce (hex-encoded, for chat_media with MDK encryption)
    /// None for group images (which use key/nonce encryption), Some for chat media
    pub nonce: Option<String>,
    /// Encryption scheme version (e.g., "mip04-v2", for chat_media with MDK encryption)
    /// None for group images, Some for chat media
    pub scheme_version: Option<&'a str>,
}

/// High-level media files orchestration layer
///
/// This module provides convenience methods that coordinate between:
/// - Storage layer (filesystem operations)
/// - Database layer (metadata tracking)
/// - Business logic (validation, coordination)
///
/// It does NOT handle:
/// - Network operations (use BlossomClient)
/// - Encryption/decryption (caller's responsibility)
pub struct MediaFiles<'a> {
    storage: &'a Storage,
    /// Shared database — holds `media_blobs` (content-addressed, cross-account).
    shared_db: &'a Database,
    /// Per-account database pool — holds `media_references` (account-scoped).
    account_pool: &'a SqlitePool,
}

impl<'a> MediaFiles<'a> {
    /// Creates a new MediaFiles orchestrator
    ///
    /// # Arguments
    /// * `storage` - Reference to the storage layer
    /// * `shared_db` - Reference to the shared database (media_blobs)
    /// * `account_pool` - Reference to the per-account database pool (media_references)
    pub(crate) fn new(
        storage: &'a Storage,
        shared_db: &'a Database,
        account_pool: &'a SqlitePool,
    ) -> Self {
        Self {
            storage,
            shared_db,
            account_pool,
        }
    }

    /// Stores a file and records it in the database in one operation
    ///
    /// This is a convenience method that:
    /// 1. Stores the file to the filesystem (deduplicated by content)
    /// 2. Records the metadata in the database (linking this group to the file)
    ///
    /// Files with the same content (hash) are stored only once on disk.
    /// Multiple groups can reference the same file through database records.
    ///
    /// # Arguments
    /// * `account_pubkey` - The account accessing this file
    /// * `group_id` - The MLS group ID (for database relationship tracking)
    /// * `filename` - The filename to store as (typically `<hash>.<ext>`)
    /// * `upload` - MediaFileUpload containing file data and metadata
    ///
    /// # Returns
    /// The MediaFile record from the database
    #[perf_instrument("media_files")]
    pub(crate) async fn store_and_record(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        filename: &str,
        upload: MediaFileUpload<'_>,
    ) -> Result<MediaFile> {
        // Store file to filesystem (deduplicated by content)
        let file_path = self
            .storage
            .media_files
            .store_file(filename, upload.data)
            .await?;

        // Record in database (tracks group-file relationship) and return the MediaFile
        self.record_in_database(account_pubkey, group_id, &file_path, upload)
            .await
    }

    /// Records an existing file in the database
    ///
    /// Use this when you already have a file stored and just need to update/record metadata.
    ///
    /// # Arguments
    /// * `account_pubkey` - The account accessing this file
    /// * `group_id` - The MLS group ID
    /// * `file_path` - Path to the cached file
    /// * `upload` - MediaFileUpload containing file metadata
    ///
    /// # Returns
    /// The MediaFile record from the database
    #[perf_instrument("media_files")]
    pub(crate) async fn record_in_database(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        file_path: &Path,
        upload: MediaFileUpload<'_>,
    ) -> Result<MediaFile> {
        let media_file = MediaFile::save(
            self.account_pool,
            self.shared_db,
            group_id,
            account_pubkey,
            MediaFileParams {
                file_path,
                original_file_hash: upload.original_file_hash,
                encrypted_file_hash: &upload.encrypted_file_hash,
                mime_type: upload.mime_type,
                media_type: upload.media_type,
                blossom_url: upload.blossom_url,
                nostr_key: upload.nostr_key.as_deref(),
                file_metadata: upload.file_metadata,
                nonce: upload.nonce.as_deref(),
                scheme_version: upload.scheme_version,
            },
        )
        .await?;

        Ok(media_file)
    }

    /// Finds a file with a given prefix
    ///
    /// Useful when you know the hash but not the exact extension.
    ///
    /// # Arguments
    /// * `prefix` - The filename prefix to search for
    ///
    /// # Returns
    /// The path to the first matching file, if any
    #[perf_instrument("media_files")]
    pub(crate) async fn find_file_with_prefix(&self, prefix: &str) -> Option<PathBuf> {
        self.storage.media_files.find_file_with_prefix(prefix).await
    }

    /// Parses imeta tags from an event using MDK's MIP-04 compliant parser
    ///
    /// This is a synchronous operation that doesn't involve I/O.
    /// Individual malformed imeta tags are logged and skipped.
    ///
    /// Uses MDK's `parse_imeta_tag` which validates:
    /// - Required fields (url, m, filename, x, v)
    /// - MIME type canonicalization
    /// - Filename validation
    /// - Version compatibility
    ///
    /// Additionally extracts fields that MDK doesn't parse:
    /// - encrypted_hash (from Blossom URL)
    /// - blurhash (optional field for image previews)
    ///
    /// # Arguments
    /// * `inner_event` - The decrypted inner event containing imeta tags
    /// * `media_manager` - MDK's EncryptedMediaManager for parsing
    ///
    /// # Returns
    /// Vector of ParsedMediaReference ready for storage
    pub(crate) fn parse_imeta_tags_from_event<S>(
        &self,
        inner_event: &UnsignedEvent,
        media_manager: &EncryptedMediaManager<'_, S>,
    ) -> Result<Vec<ParsedMediaReference>>
    where
        S: MdkStorageProvider,
    {
        let mut parsed = Vec::new();

        // Filter for imeta tags and parse using MDK
        for tag in inner_event.tags.iter() {
            if tag.kind() != TagKind::Custom("imeta".into()) {
                continue;
            }

            // Parse using MDK's MIP-04 compliant parser
            let reference = match media_manager.parse_imeta_tag(tag) {
                Ok(ref_) => ref_,
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::store_media_references",
                        "Skipping malformed imeta tag: {}",
                        e
                    );
                    continue;
                }
            };

            // Extract encrypted_file_hash from Blossom URL (REQUIRED - NOT NULL in DB)
            // MDK's MediaReference stores URL as-is, but we need the hash for our database schema
            let encrypted_file_hash = match extract_hash_from_blossom_url(&reference.url) {
                Ok(hash) => hash,
                Err(e) => {
                    tracing::warn!(
                        target: "whitenoise::store_media_references",
                        "Skipping imeta tag: failed to extract encrypted hash from Blossom URL '{}': {}",
                        reference.url,
                        e
                    );
                    continue;
                }
            };

            // Extract optional display metadata not exposed by MDK 0.8's MediaReference.
            let display_metadata = Self::extract_display_metadata_from_tag(tag);

            parsed.push(ParsedMediaReference {
                reference,
                encrypted_hash: encrypted_file_hash,
                blurhash: display_metadata.blurhash,
                thumbhash: display_metadata.thumbhash,
                duration_ms: display_metadata.duration_ms,
                waveform: display_metadata.waveform,
            });
        }

        Ok(parsed)
    }

    /// Extracts optional display metadata from an imeta tag in a single pass.
    ///
    /// MDK's parser doesn't expose these fields yet, so we do it ourselves.
    /// Invalid optional values are ignored; required decryption fields remain
    /// strict in MDK's parser.
    fn extract_display_metadata_from_tag(tag: &Tag) -> ParsedDisplayMetadata {
        let tag_vec = tag.clone().to_vec();
        let mut metadata = ParsedDisplayMetadata::default();

        for value in tag_vec.iter().skip(1) {
            let Some((key, raw_value)) = value.split_once(' ') else {
                continue;
            };
            match key {
                "blurhash" if metadata.blurhash.is_none() && !raw_value.is_empty() => {
                    metadata.blurhash = Some(raw_value.to_string());
                }
                "thumbhash" if metadata.thumbhash.is_none() && !raw_value.is_empty() => {
                    metadata.thumbhash = Some(raw_value.to_string());
                }
                "duration" if metadata.duration_ms.is_none() => {
                    metadata.duration_ms = parse_duration_ms(raw_value);
                }
                "waveform" if metadata.waveform.is_none() => {
                    metadata.waveform = parse_waveform(raw_value);
                }
                _ => {}
            }
            if metadata.blurhash.is_some()
                && metadata.thumbhash.is_some()
                && metadata.duration_ms.is_some()
                && metadata.waveform.is_some()
            {
                break;
            }
        }

        metadata
    }

    /// Extracts blurhash and thumbhash from an imeta tag in a single pass.
    ///
    /// Kept as a small compatibility wrapper for existing unit tests.
    #[cfg(test)]
    fn extract_hashes_from_tag(tag: &Tag) -> (Option<String>, Option<String>) {
        let metadata = Self::extract_display_metadata_from_tag(tag);
        (metadata.blurhash, metadata.thumbhash)
    }

    /// Deletes media files from disk that have no database references.
    ///
    /// Scans the media cache directory and removes files not referenced by any
    /// `media_files` row. This reclaims disk space after operations like
    /// `delete_chat` that remove DB records but leave files on disk.
    ///
    /// Files are content-addressed and may be shared across groups -- a file
    /// is only deleted when zero DB records reference it.
    ///
    /// Returns the number of files deleted.
    #[perf_instrument("media_files")]
    pub(crate) async fn cleanup_orphaned_files(&self) -> Result<u64> {
        // 1. Get all distinct file paths referenced in the database
        let referenced_paths: Vec<String> =
            MediaFile::all_referenced_file_paths(self.shared_db).await?;

        let referenced: HashSet<PathBuf> =
            referenced_paths.into_iter().map(PathBuf::from).collect();

        // 2. Scan the cache directory
        let cache_dir = self.storage.media_files.cache_dir();
        if !cache_dir.exists() {
            return Ok(0);
        }

        let mut entries = tokio::fs::read_dir(cache_dir).await.map_err(|e| {
            WhitenoiseError::MediaCache(format!("Failed to read cache directory: {}", e))
        })?;

        // 3. Delete files not referenced by any DB record
        let mut deleted = 0u64;
        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            WhitenoiseError::MediaCache(format!("Failed to read directory entry: {}", e))
        })? {
            let path = entry.path();

            // Skip directories (media_cache should be flat, but be defensive)
            if !path.is_file() {
                continue;
            }

            if !referenced.contains(&path) {
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => {
                        tracing::debug!(
                            target: "whitenoise::media_files",
                            "Deleted orphaned media file: {}",
                            path.display()
                        );
                        deleted += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "whitenoise::media_files",
                            "Failed to delete orphaned file {}: {}",
                            path.display(),
                            e
                        );
                    }
                }
            }
        }

        if deleted > 0 {
            tracing::info!(
                target: "whitenoise::media_files",
                "Cleaned up {} orphaned media file(s)",
                deleted
            );
        }

        Ok(deleted)
    }

    /// Stores parsed media references to the database
    ///
    /// Creates MediaFile records without downloading the actual files.
    /// The file_path will be empty until download_chat_media() is called.
    ///
    /// # Arguments
    /// * `group_id` - The MLS group ID
    /// * `account_pubkey` - The account receiving the message
    /// * `parsed_references` - ParsedMediaReference from parse_imeta_tags_from_event
    ///
    /// # Returns
    /// * `Ok(())` - All references stored successfully
    /// * `Err(WhitenoiseError)` - Database error
    #[perf_instrument("media_files")]
    pub(crate) async fn store_parsed_media_references(
        &self,
        group_id: &GroupId,
        account_pubkey: &PublicKey,
        parsed_references: Vec<ParsedMediaReference>,
    ) -> Result<()> {
        for parsed in parsed_references {
            let reference = parsed.reference;
            let encrypted_hash = parsed.encrypted_hash;

            // Convert dimensions from MDK format (width, height) to our string format
            let dimensions = reference.dimensions.map(|(w, h)| format!("{}x{}", w, h));

            // Create file metadata
            // MIP-04 requires filename, so it's always present
            let file_metadata = Some(FileMetadata {
                original_filename: Some(reference.filename.clone()),
                dimensions,
                blurhash: parsed.blurhash,
                thumbhash: parsed.thumbhash,
                duration_ms: parsed.duration_ms,
                waveform: parsed.waveform,
            });

            // Create MediaFile record (without file yet - empty path until downloaded)
            // Store nonce and scheme_version for MDK decryption
            MediaFile::save(
                self.account_pool,
                self.shared_db,
                group_id,
                account_pubkey,
                MediaFileParams {
                    file_path: &PathBuf::from(""), // Empty until downloaded
                    original_file_hash: Some(&reference.original_hash),
                    encrypted_file_hash: &encrypted_hash,
                    mime_type: &reference.mime_type,
                    media_type: "chat_media",
                    blossom_url: Some(&reference.url),
                    nostr_key: None, // Chat media uses MDK, not key/nonce
                    file_metadata: file_metadata.as_ref(),
                    nonce: Some(&hex::encode(reference.nonce)),
                    scheme_version: Some(&reference.scheme_version),
                },
            )
            .await?;

            tracing::debug!(
                target: "whitenoise::store_media_references",
                "Stored media reference for account {}: original_hash={}, encrypted_hash={}, mime_type={}",
                account_pubkey.to_hex(),
                hex::encode(reference.original_hash),
                hex::encode(encrypted_hash),
                reference.mime_type
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::whitenoise::database::account_db::AccountDatabase;

    use super::*;
    use tempfile::TempDir;

    struct TestCtx {
        shared: Database,
        account_pool: SqlitePool,
        pubkey: PublicKey,
        storage: Storage,
        _dir: TempDir,
    }

    async fn setup() -> TestCtx {
        let dir = TempDir::new().unwrap();
        let pubkey = PublicKey::from_slice(&[2u8; 32]).unwrap();
        let shared = Database::new(dir.path().join("shared.db")).await.unwrap();
        let account_db = AccountDatabase::new(pubkey, dir.path().join("account.db"))
            .await
            .unwrap();
        account_db
            .run_account_migrations(&shared.pool)
            .await
            .unwrap();

        // FK constraints
        create_test_account(&shared, &pubkey).await;

        let storage = Storage::new(dir.path()).await.unwrap();
        TestCtx {
            shared,
            account_pool: account_db.inner.pool,
            pubkey,
            storage,
            _dir: dir,
        }
    }

    async fn create_test_account(db: &Database, pubkey: &PublicKey) {
        sqlx::query("INSERT INTO users (pubkey, created_at, updated_at) VALUES (?, ?, ?)")
            .bind(pubkey.to_hex())
            .bind(chrono::Utc::now().timestamp())
            .bind(chrono::Utc::now().timestamp())
            .execute(&db.pool)
            .await
            .unwrap();

        let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE pubkey = ?")
            .bind(pubkey.to_hex())
            .fetch_one(&db.pool)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO accounts (pubkey, user_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(pubkey.to_hex())
        .bind(user_id)
        .bind(chrono::Utc::now().timestamp())
        .bind(chrono::Utc::now().timestamp())
        .execute(&db.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_store_and_record() {
        let t = setup().await;
        let media_files = MediaFiles::new(&t.storage, &t.shared, &t.account_pool);

        let group_id = GroupId::from_slice(&[1u8; 8]);
        let encrypted_file_hash = [3u8; 32];
        let test_data = b"test file content";

        let upload = MediaFileUpload {
            data: test_data,
            original_file_hash: None,
            encrypted_file_hash,
            mime_type: "image/jpeg",
            media_type: "test_media",
            blossom_url: None,
            nostr_key: None,
            file_metadata: None,
            nonce: None,
            scheme_version: None,
        };
        let media_file = media_files
            .store_and_record(&t.pubkey, &group_id, "test.jpg", upload)
            .await
            .unwrap();

        assert!(media_file.file_path.exists());
        let content = tokio::fs::read(&media_file.file_path).await.unwrap();
        assert_eq!(content, test_data);

        // Idempotency
        let upload2 = MediaFileUpload {
            data: test_data,
            original_file_hash: None,
            encrypted_file_hash,
            mime_type: "image/jpeg",
            media_type: "test_media",
            blossom_url: None,
            nostr_key: None,
            file_metadata: None,
            nonce: None,
            scheme_version: None,
        };
        let media_file2 = media_files
            .store_and_record(&t.pubkey, &group_id, "test.jpg", upload2)
            .await
            .unwrap();

        assert_eq!(media_file.file_path, media_file2.file_path);
    }

    #[tokio::test]
    async fn test_find_file_with_prefix() {
        let t = setup().await;
        let media_files = MediaFiles::new(&t.storage, &t.shared, &t.account_pool);

        t.storage
            .media_files
            .store_file("abc123.jpg", b"jpeg data")
            .await
            .unwrap();

        let found = media_files.find_file_with_prefix("abc123").await.unwrap();
        assert!(found.to_string_lossy().contains("abc123"));
    }

    #[test]
    fn test_extract_hash_from_blossom_url_valid() {
        let url = "https://blossom.example.com/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let result = extract_hash_from_blossom_url(url);
        assert!(result.is_ok());
        let hash = result.unwrap();
        assert_eq!(hash.len(), 32);
        assert_eq!(
            hex::encode(hash),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn test_extract_hash_from_blossom_url_with_trailing_slash() {
        let url = "https://blossom.example.com/abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234/";
        let result = extract_hash_from_blossom_url(url);
        assert!(result.is_ok());
    }

    #[test]
    fn test_extract_hash_from_blossom_url_with_path_prefix() {
        let url = "https://blossom.example.com/api/v1/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.png";
        let result = extract_hash_from_blossom_url(url);
        assert!(result.is_ok());
        let hash = result.unwrap();
        assert_eq!(hash.len(), 32);
        assert_eq!(
            hex::encode(hash),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn test_extract_hash_from_blossom_url_invalid_hex() {
        let url = "https://blossom.example.com/notahexstring";
        let result = extract_hash_from_blossom_url(url);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid hex"));
    }

    #[test]
    fn test_extract_hash_from_blossom_url_wrong_length() {
        let url = "https://blossom.example.com/abc123"; // Too short
        let result = extract_hash_from_blossom_url(url);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid hash length")
        );
    }

    #[test]
    fn test_extract_hash_from_blossom_url_no_hash() {
        let url = "https://blossom.example.com/";
        let result = extract_hash_from_blossom_url(url);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("contains no hash"));
    }

    #[test]
    fn test_extract_hash_from_blossom_url_invalid_url() {
        let url = "not-a-url";
        let result = extract_hash_from_blossom_url(url);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid Blossom URL")
        );
    }

    #[test]
    fn test_extract_hash_from_blossom_url_with_extension() {
        // Hash with .png extension should be stripped
        let hash_hex = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let url = format!("https://blossom.example.com/{}.png", hash_hex);
        let result = extract_hash_from_blossom_url(&url).unwrap();
        assert_eq!(hex::encode(result), hash_hex);
    }

    #[test]
    fn test_extract_hash_from_blossom_url_dot_only_extension() {
        // URL ending in just a dot after the hash -> hash_hex becomes empty before the dot
        let url = "https://blossom.example.com/.";
        let result = extract_hash_from_blossom_url(url);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("contains no hash"));
    }

    #[test]
    fn test_extract_hashes_with_blurhash_only() {
        let tag = Tag::custom(
            TagKind::Custom("imeta".into()),
            [
                "url https://blossom.example.com/abc123",
                "blurhash LEHV6nWB2yk8pyoJadR*.7kCMdnj",
            ],
        );
        let (blurhash, thumbhash) = MediaFiles::extract_hashes_from_tag(&tag);
        assert_eq!(blurhash, Some("LEHV6nWB2yk8pyoJadR*.7kCMdnj".to_string()));
        assert!(thumbhash.is_none());
    }

    #[test]
    fn test_extract_hashes_with_thumbhash_only() {
        let tag = Tag::custom(
            TagKind::Custom("imeta".into()),
            [
                "url https://blossom.example.com/abc123",
                "thumbhash 3OcRJYB4d3h/iIeHeEh3eIhw+j2w",
            ],
        );
        let (blurhash, thumbhash) = MediaFiles::extract_hashes_from_tag(&tag);
        assert!(blurhash.is_none());
        assert_eq!(thumbhash, Some("3OcRJYB4d3h/iIeHeEh3eIhw+j2w".to_string()));
    }

    #[test]
    fn test_extract_hashes_with_both() {
        let tag = Tag::custom(
            TagKind::Custom("imeta".into()),
            [
                "url https://blossom.example.com/abc123",
                "blurhash LEHV6nWB2yk8pyoJadR*.7kCMdnj",
                "thumbhash 3OcRJYB4d3h/iIeHeEh3eIhw+j2w",
            ],
        );
        let (blurhash, thumbhash) = MediaFiles::extract_hashes_from_tag(&tag);
        assert_eq!(blurhash, Some("LEHV6nWB2yk8pyoJadR*.7kCMdnj".to_string()));
        assert_eq!(thumbhash, Some("3OcRJYB4d3h/iIeHeEh3eIhw+j2w".to_string()));
    }

    #[test]
    fn test_extract_hashes_without_either() {
        let tag = Tag::custom(
            TagKind::Custom("imeta".into()),
            ["url https://blossom.example.com/abc123", "m image/jpeg"],
        );
        let (blurhash, thumbhash) = MediaFiles::extract_hashes_from_tag(&tag);
        assert!(blurhash.is_none());
        assert!(thumbhash.is_none());
    }

    #[test]
    fn test_extract_hashes_empty_tag() {
        let tag = Tag::custom(TagKind::Custom("imeta".into()), Vec::<String>::new());
        let (blurhash, thumbhash) = MediaFiles::extract_hashes_from_tag(&tag);
        assert!(blurhash.is_none());
        assert!(thumbhash.is_none());
    }

    #[test]
    fn test_extract_hashes_ignores_empty_values() {
        // Malformed tags like "blurhash " or "thumbhash " should yield None
        let tag = Tag::custom(
            TagKind::Custom("imeta".into()),
            [
                "url https://blossom.example.com/abc123",
                "blurhash ",
                "thumbhash ",
            ],
        );
        let (blurhash, thumbhash) = MediaFiles::extract_hashes_from_tag(&tag);
        assert!(blurhash.is_none());
        assert!(thumbhash.is_none());
    }

    #[test]
    fn test_extract_display_metadata_with_audio_fields() {
        let tag = Tag::custom(
            TagKind::Custom("imeta".into()),
            [
                "url https://blossom.example.com/abc123",
                "duration 12.345",
                "waveform 0 8 42 100",
            ],
        );

        let metadata = MediaFiles::extract_display_metadata_from_tag(&tag);

        assert_eq!(metadata.duration_ms, Some(12_345));
        assert_eq!(metadata.waveform, Some(vec![0, 8, 42, 100]));
    }

    #[test]
    fn test_extract_display_metadata_ignores_invalid_duration() {
        let tag = Tag::custom(
            TagKind::Custom("imeta".into()),
            [
                "url https://blossom.example.com/abc123",
                "duration nope",
                "waveform 0 50 100",
            ],
        );

        let metadata = MediaFiles::extract_display_metadata_from_tag(&tag);

        assert!(metadata.duration_ms.is_none());
        assert_eq!(metadata.waveform, Some(vec![0, 50, 100]));
    }

    #[test]
    fn test_extract_display_metadata_ignores_invalid_waveform() {
        let tag = Tag::custom(
            TagKind::Custom("imeta".into()),
            [
                "url https://blossom.example.com/abc123",
                "duration 7",
                "waveform 0 101",
            ],
        );

        let metadata = MediaFiles::extract_display_metadata_from_tag(&tag);

        assert_eq!(metadata.duration_ms, Some(7_000));
        assert!(metadata.waveform.is_none());
    }

    #[tokio::test]
    async fn test_store_parsed_media_references_persists_audio_metadata() {
        let t = setup().await;
        let media_files = MediaFiles::new(&t.storage, &t.shared, &t.account_pool);
        let group_id = GroupId::from_slice(&[7u8; 8]);
        let original_hash = [9u8; 32];
        let encrypted_hash = [10u8; 32];
        let nonce = [11u8; 12];
        let reference = MediaReference {
            url: format!(
                "https://blossom.example.com/{}",
                hex::encode(encrypted_hash)
            ),
            original_hash,
            mime_type: "audio/mpeg".to_string(),
            filename: "voice.mp3".to_string(),
            dimensions: None,
            scheme_version: "mip04-v2".to_string(),
            nonce,
        };

        media_files
            .store_parsed_media_references(
                &group_id,
                &t.pubkey,
                vec![ParsedMediaReference {
                    reference,
                    encrypted_hash,
                    blurhash: None,
                    thumbhash: None,
                    duration_ms: Some(12_345),
                    waveform: Some(vec![0, 8, 42, 100]),
                }],
            )
            .await
            .unwrap();

        let stored = MediaFile::find_by_original_hash_and_group(
            &t.account_pool,
            &t.shared,
            &t.pubkey,
            &original_hash,
            &group_id,
        )
        .await
        .unwrap()
        .expect("media reference should be stored");
        let metadata = stored
            .file_metadata
            .expect("file metadata should be stored");
        assert_eq!(metadata.original_filename, Some("voice.mp3".to_string()));
        assert_eq!(metadata.duration_ms, Some(12_345));
        assert_eq!(metadata.waveform, Some(vec![0, 8, 42, 100]));
    }

    #[tokio::test]
    async fn test_invalid_audio_metadata_is_omitted_without_dropping_reference() {
        let t = setup().await;
        let media_files = MediaFiles::new(&t.storage, &t.shared, &t.account_pool);
        let group_id = GroupId::from_slice(&[8u8; 8]);
        let original_hash = [12u8; 32];
        let encrypted_hash = [13u8; 32];
        let nonce = [14u8; 12];
        let tag = Tag::custom(
            TagKind::Custom("imeta".into()),
            ["duration not-a-number", "waveform 0 101", "thumbhash thumb"],
        );
        let display_metadata = MediaFiles::extract_display_metadata_from_tag(&tag);
        let reference = MediaReference {
            url: format!(
                "https://blossom.example.com/{}",
                hex::encode(encrypted_hash)
            ),
            original_hash,
            mime_type: "audio/mpeg".to_string(),
            filename: "voice.mp3".to_string(),
            dimensions: None,
            scheme_version: "mip04-v2".to_string(),
            nonce,
        };

        media_files
            .store_parsed_media_references(
                &group_id,
                &t.pubkey,
                vec![ParsedMediaReference {
                    reference,
                    encrypted_hash,
                    blurhash: display_metadata.blurhash,
                    thumbhash: display_metadata.thumbhash,
                    duration_ms: display_metadata.duration_ms,
                    waveform: display_metadata.waveform,
                }],
            )
            .await
            .unwrap();

        let stored = MediaFile::find_by_original_hash_and_group(
            &t.account_pool,
            &t.shared,
            &t.pubkey,
            &original_hash,
            &group_id,
        )
        .await
        .unwrap()
        .expect("media reference should still be stored");
        let metadata = stored
            .file_metadata
            .expect("file metadata should be stored");
        assert_eq!(metadata.original_filename, Some("voice.mp3".to_string()));
        assert_eq!(metadata.thumbhash, Some("thumb".to_string()));
        assert!(metadata.duration_ms.is_none());
        assert!(metadata.waveform.is_none());
    }

    #[test]
    fn test_build_chat_media_imeta_tag_includes_audio_metadata() {
        let original_hash = [9u8; 32];
        let encrypted_hash = [10u8; 32];
        let metadata = FileMetadata::new()
            .with_filename("voice.mp3".to_string())
            .with_duration_ms(12_345)
            .with_waveform(vec![0, 8, 42, 100]);
        let media_file = MediaFile {
            id: Some(1),
            mls_group_id: GroupId::from_slice(&[1u8; 8]),
            account_pubkey: PublicKey::from_slice(&[2u8; 32]).unwrap(),
            file_path: PathBuf::from("/tmp/voice.mp3"),
            original_file_hash: Some(original_hash.to_vec()),
            encrypted_file_hash: encrypted_hash.to_vec(),
            mime_type: "audio/mpeg".to_string(),
            media_type: "chat_media".to_string(),
            blossom_url: Some(format!(
                "https://blossom.example.com/{}",
                hex::encode(encrypted_hash)
            )),
            nostr_key: None,
            file_metadata: Some(metadata),
            nonce: Some("0102030405060708090a0b0c".to_string()),
            scheme_version: Some("mip04-v2".to_string()),
            created_at: chrono::Utc::now(),
        };

        let tag = build_chat_media_imeta_tag(&media_file, None).unwrap();
        let tag_values = tag.to_vec();

        assert!(tag_values.contains(&format!("x {}", hex::encode(original_hash))));
        assert!(tag_values.contains(&"duration 12.345".to_string()));
        assert!(tag_values.contains(&"waveform 0 8 42 100".to_string()));
        assert!(tag_values.contains(&"v mip04-v2".to_string()));
    }

    #[tokio::test]
    async fn test_cleanup_orphaned_files_deletes_unreferenced() {
        let t = setup().await;

        t.storage
            .media_files
            .store_file("orphan.jpg", b"orphan data")
            .await
            .unwrap();

        let media_files = MediaFiles::new(&t.storage, &t.shared, &t.account_pool);
        let deleted = media_files.cleanup_orphaned_files().await.unwrap();

        assert_eq!(deleted, 1);
        assert!(
            t.storage
                .media_files
                .find_file_with_prefix("orphan")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_cleanup_orphaned_files_keeps_referenced() {
        let t = setup().await;
        let media_files = MediaFiles::new(&t.storage, &t.shared, &t.account_pool);

        let group_id = GroupId::from_slice(&[1u8; 8]);
        let upload = MediaFileUpload {
            data: b"referenced data",
            original_file_hash: None,
            encrypted_file_hash: [3u8; 32],
            mime_type: "image/jpeg",
            media_type: "test_media",
            blossom_url: None,
            nostr_key: None,
            file_metadata: None,
            nonce: None,
            scheme_version: None,
        };
        let record = media_files
            .store_and_record(&t.pubkey, &group_id, "referenced.jpg", upload)
            .await
            .unwrap();

        let deleted = media_files.cleanup_orphaned_files().await.unwrap();

        assert_eq!(deleted, 0);
        assert!(record.file_path.exists());
    }

    #[tokio::test]
    async fn test_cleanup_orphaned_files_mixed() {
        let t = setup().await;
        let media_files = MediaFiles::new(&t.storage, &t.shared, &t.account_pool);

        let group_id = GroupId::from_slice(&[1u8; 8]);
        let upload = MediaFileUpload {
            data: b"keep me",
            original_file_hash: None,
            encrypted_file_hash: [4u8; 32],
            mime_type: "image/png",
            media_type: "test_media",
            blossom_url: None,
            nostr_key: None,
            file_metadata: None,
            nonce: None,
            scheme_version: None,
        };
        let kept = media_files
            .store_and_record(&t.pubkey, &group_id, "keep.png", upload)
            .await
            .unwrap();

        t.storage
            .media_files
            .store_file("orphan.png", b"delete me")
            .await
            .unwrap();

        let deleted = media_files.cleanup_orphaned_files().await.unwrap();

        assert_eq!(deleted, 1);
        assert!(kept.file_path.exists());
        assert!(
            t.storage
                .media_files
                .find_file_with_prefix("orphan")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_cleanup_orphaned_files_empty_cache() {
        let t = setup().await;
        let media_files = MediaFiles::new(&t.storage, &t.shared, &t.account_pool);
        let deleted = media_files.cleanup_orphaned_files().await.unwrap();

        assert_eq!(deleted, 0);
    }
}
