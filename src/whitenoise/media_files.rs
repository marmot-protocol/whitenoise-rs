use std::collections::HashSet;
use std::path::{Path, PathBuf};

use nostr_sdk::prelude::*;
use sqlx::SqlitePool;

pub use crate::whitenoise::database::media_files::MediaFile;
use crate::{
    marmot::GroupId,
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
                "waveform samples must be integers in the inclusive range 0..100 and at most 16384 samples".to_string(),
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
        if let Some(duration) = metadata.duration_ms.and_then(format_duration_seconds) {
            tag_values.push(format!("duration {}", duration));
        }
        if let Some(samples) = metadata.waveform.as_deref().and_then(format_waveform) {
            tag_values.push(format!("waveform {}", samples));
        }
    }

    Ok(Tag::custom(TagKind::Custom("imeta".into()), tag_values))
}

fn format_duration_seconds(duration_ms: u64) -> Option<String> {
    if duration_ms == 0 {
        return None;
    }

    let seconds = duration_ms / 1000;
    let millis = duration_ms % 1000;
    if millis == 0 {
        return Some(seconds.to_string());
    }

    let mut fractional = format!("{:03}", millis);
    while fractional.ends_with('0') {
        fractional.pop();
    }
    Some(format!("{}.{}", seconds, fractional))
}

fn format_waveform(waveform: &[u8]) -> Option<String> {
    if !FileMetadata::is_valid_waveform(waveform) {
        return None;
    }

    Some(
        waveform
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

#[derive(Debug, Clone)]
pub(crate) struct ParsedMediaReference {
    url: String,
    original_hash: [u8; 32],
    mime_type: String,
    filename: String,
    dimensions: Option<(u32, u32)>,
    duration_ms: Option<u64>,
    waveform: Option<Vec<u8>>,
    nonce: [u8; 12],
    scheme_version: String,
    encrypted_hash: [u8; 32],
    blurhash: Option<String>,
    thumbhash: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ParsedDisplayMetadata {
    blurhash: Option<String>,
    thumbhash: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ImetaFields {
    url: Option<String>,
    mime_type: Option<String>,
    filename: Option<String>,
    original_hash: Option<String>,
    nonce: Option<String>,
    scheme_version: Option<String>,
    dimensions: Option<String>,
    duration: Option<String>,
    waveform: Option<String>,
}

impl ImetaFields {
    fn from_tag(tag: &Tag) -> Self {
        let mut fields = Self::default();

        for value in tag.clone().to_vec().iter().skip(1) {
            let Some((key, raw_value)) = split_imeta_field(value) else {
                continue;
            };

            match key {
                "url" if fields.url.is_none() => fields.url = Some(raw_value.to_string()),
                "m" if fields.mime_type.is_none() => {
                    fields.mime_type = Some(raw_value.to_string());
                }
                "filename" if fields.filename.is_none() => {
                    fields.filename = Some(raw_value.to_string());
                }
                "x" if fields.original_hash.is_none() => {
                    fields.original_hash = Some(raw_value.to_string());
                }
                "n" if fields.nonce.is_none() => fields.nonce = Some(raw_value.to_string()),
                "v" if fields.scheme_version.is_none() => {
                    fields.scheme_version = Some(raw_value.to_string());
                }
                "dim" if fields.dimensions.is_none() => {
                    fields.dimensions = Some(raw_value.to_string());
                }
                "duration" if fields.duration.is_none() => {
                    fields.duration = Some(raw_value.to_string());
                }
                "waveform" if fields.waveform.is_none() => {
                    fields.waveform = Some(raw_value.to_string());
                }
                _ => {}
            }
        }

        fields
    }
}

fn split_imeta_field(value: &str) -> Option<(&str, &str)> {
    let trimmed = value.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let separator = trimmed.find(|c: char| c.is_ascii_whitespace())?;
    let key = &trimmed[..separator];
    let raw_value = trimmed[separator..].trim_start_matches(|c: char| c.is_ascii_whitespace());
    Some((key, raw_value))
}

fn required_imeta_field(value: Option<String>, name: &str) -> Result<String> {
    value
        .filter(|value| {
            !value
                .trim_matches(|c: char| c.is_ascii_whitespace())
                .is_empty()
        })
        .ok_or_else(|| WhitenoiseError::InvalidInput(format!("imeta tag missing {name} field")))
}

fn canonical_media_type(value: String) -> Result<String> {
    let media_type = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_ascii_lowercase();
    if media_type.is_empty() || !media_type.contains('/') {
        return Err(WhitenoiseError::InvalidInput(
            "imeta media type must be a MIME type".to_string(),
        ));
    }

    Ok(match media_type.as_str() {
        "image/jpg" => "image/jpeg".to_string(),
        other => other.to_string(),
    })
}

fn parse_fixed_hex_field<const N: usize>(value: String, field_name: &str) -> Result<[u8; N]> {
    let bytes =
        hex::decode(value.trim_matches(|c: char| c.is_ascii_whitespace())).map_err(|error| {
            WhitenoiseError::InvalidInput(format!("imeta {field_name} field must be hex: {error}"))
        })?;
    let actual_len = bytes.len();
    bytes.try_into().map_err(|_| {
        WhitenoiseError::InvalidInput(format!(
            "imeta {field_name} field must be {N} bytes, got {actual_len}"
        ))
    })
}

fn parse_dimensions(value: String) -> Option<(u32, u32)> {
    let (width, height) = value.split_once('x')?;
    let width = width.parse::<u32>().ok()?;
    let height = height.parse::<u32>().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

fn parse_duration_ms(value: String) -> Option<u64> {
    let trimmed = value.trim_matches(|c: char| c.is_ascii_whitespace());
    let (seconds, millis) = match trimmed.split_once('.') {
        Some((seconds, fraction)) => {
            if fraction.is_empty() || fraction.len() > 3 {
                return None;
            }
            let mut padded = fraction.to_string();
            while padded.len() < 3 {
                padded.push('0');
            }
            (seconds, padded)
        }
        None => (trimmed, "000".to_string()),
    };

    let seconds = seconds.parse::<u64>().ok()?;
    let millis = millis.parse::<u64>().ok()?;
    seconds.checked_mul(1000)?.checked_add(millis)
}

fn parse_waveform(value: String) -> Option<Vec<u8>> {
    let samples: Vec<u8> = value
        .split_whitespace()
        .map(str::parse::<u8>)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    FileMetadata::is_valid_waveform(&samples).then_some(samples)
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

/// Intermediate type for media file storage operations
///
/// This type abstracts over different upload sources and provides a unified
/// interface for storing media files.
pub(crate) struct MediaFileUpload<'a> {
    /// The decrypted file data to store
    pub data: &'a [u8],
    /// SHA-256 hash of the original/decrypted content for the MIP-04 `x` field.
    /// None for group images, Some for chat media.
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
    /// Encryption nonce (hex-encoded) for chat media.
    /// None for group images (which use key/nonce encryption), Some for chat media
    pub nonce: Option<String>,
    /// Encryption scheme version (e.g., "mip04-v2") for chat media.
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
    #[cfg(test)]
    pub(crate) async fn find_file_with_prefix(&self, prefix: &str) -> Option<PathBuf> {
        self.storage.media_files.find_file_with_prefix(prefix).await
    }

    /// Parses MIP-04/NIP-92 `imeta` tags from a decrypted chat event.
    ///
    /// This is a synchronous app-level parser. Individual malformed `imeta`
    /// tags are logged and skipped so one bad attachment does not drop the
    /// enclosing chat message.
    pub(crate) fn parse_imeta_tags_from_event(
        &self,
        inner_event: &UnsignedEvent,
    ) -> Result<Vec<ParsedMediaReference>> {
        let mut parsed = Vec::new();

        for tag in inner_event.tags.iter() {
            if tag.kind() != TagKind::Custom("imeta".into()) {
                continue;
            }

            let reference = match Self::parse_imeta_tag(tag) {
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

            parsed.push(reference);
        }

        Ok(parsed)
    }

    fn parse_imeta_tag(tag: &Tag) -> Result<ParsedMediaReference> {
        let fields = ImetaFields::from_tag(tag);

        let url = required_imeta_field(fields.url, "url")?;
        let mime_type = canonical_media_type(required_imeta_field(fields.mime_type, "m")?)?;
        let filename = required_imeta_field(fields.filename, "filename")?
            .trim()
            .to_string();
        if filename.is_empty() {
            return Err(WhitenoiseError::InvalidInput(
                "imeta filename cannot be empty".to_string(),
            ));
        }

        let original_hash =
            parse_fixed_hex_field::<32>(required_imeta_field(fields.original_hash, "x")?, "x")?;
        let nonce = parse_fixed_hex_field::<12>(required_imeta_field(fields.nonce, "n")?, "n")?;
        let scheme_version = required_imeta_field(fields.scheme_version, "v")?;
        if scheme_version != "mip04-v2" {
            return Err(WhitenoiseError::InvalidInput(format!(
                "unsupported imeta media scheme version: {scheme_version}"
            )));
        }

        let encrypted_hash = extract_hash_from_blossom_url(&url)?;
        let display_metadata = Self::extract_display_metadata_from_tag(tag);

        Ok(ParsedMediaReference {
            url,
            original_hash,
            mime_type,
            filename,
            dimensions: fields.dimensions.and_then(parse_dimensions),
            duration_ms: fields.duration.and_then(parse_duration_ms),
            waveform: fields.waveform.and_then(parse_waveform),
            nonce,
            scheme_version,
            encrypted_hash,
            blurhash: display_metadata.blurhash,
            thumbhash: display_metadata.thumbhash,
        })
    }

    /// Extracts optional preview hashes from an imeta tag in a single pass.
    ///
    /// Required MIP-04 fields are parsed separately; preview hashes remain
    /// optional display metadata.
    fn extract_display_metadata_from_tag(tag: &Tag) -> ParsedDisplayMetadata {
        let tag_vec = tag.clone().to_vec();
        let mut metadata = ParsedDisplayMetadata::default();

        for value in tag_vec.iter().skip(1) {
            let Some((key, raw_value)) = split_imeta_field(value) else {
                continue;
            };
            match key {
                "blurhash" if metadata.blurhash.is_none() && !raw_value.is_empty() => {
                    metadata.blurhash = Some(raw_value.to_string());
                }
                "thumbhash" if metadata.thumbhash.is_none() && !raw_value.is_empty() => {
                    metadata.thumbhash = Some(raw_value.to_string());
                }
                _ => {}
            }
            if metadata.blurhash.is_some() && metadata.thumbhash.is_some() {
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
            let dimensions = parsed.dimensions.map(|(w, h)| format!("{}x{}", w, h));

            let file_metadata = Some(FileMetadata {
                original_filename: Some(parsed.filename.clone()),
                dimensions,
                blurhash: parsed.blurhash,
                thumbhash: parsed.thumbhash,
                duration_ms: parsed.duration_ms,
                waveform: parsed.waveform.clone(),
            });

            MediaFile::save(
                self.account_pool,
                self.shared_db,
                group_id,
                account_pubkey,
                MediaFileParams {
                    file_path: &PathBuf::from(""), // Empty until downloaded
                    original_file_hash: Some(&parsed.original_hash),
                    encrypted_file_hash: &parsed.encrypted_hash,
                    mime_type: &parsed.mime_type,
                    media_type: "chat_media",
                    blossom_url: Some(&parsed.url),
                    nostr_key: None,
                    file_metadata: file_metadata.as_ref(),
                    nonce: Some(&hex::encode(parsed.nonce)),
                    scheme_version: Some(&parsed.scheme_version),
                },
            )
            .await?;

            tracing::debug!(
                target: "whitenoise::store_media_references",
                "Stored media reference for account {}: original_hash={}, encrypted_hash={}, mime_type={}",
                account_pubkey.to_hex(),
                hex::encode(parsed.original_hash),
                hex::encode(parsed.encrypted_hash),
                parsed.mime_type
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

    fn audio_imeta_tag(
        encrypted_hash: [u8; 32],
        original_hash: [u8; 32],
        nonce: [u8; 12],
        extra_fields: impl IntoIterator<Item = &'static str>,
    ) -> Tag {
        let mut tag_values = vec![
            format!(
                "url https://blossom.example.com/{}",
                hex::encode(encrypted_hash)
            ),
            "m audio/mpeg".to_string(),
            "filename voice.mp3".to_string(),
        ];
        tag_values.extend(extra_fields.into_iter().map(str::to_string));
        tag_values.extend([
            format!("x {}", hex::encode(original_hash)),
            format!("n {}", hex::encode(nonce)),
            "v mip04-v2".to_string(),
        ]);

        Tag::custom(TagKind::Custom("imeta".into()), tag_values)
    }

    fn event_with_tag(pubkey: PublicKey, tag: Tag) -> UnsignedEvent {
        let mut event = UnsignedEvent::new(
            pubkey,
            Timestamp::now(),
            Kind::Custom(9),
            vec![tag],
            String::new(),
        );
        event.ensure_id();
        event
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

    #[tokio::test]
    async fn test_parse_and_store_imeta_persists_audio_metadata_without_obsolete_mls_storage() {
        let t = setup().await;
        let media_files = MediaFiles::new(&t.storage, &t.shared, &t.account_pool);
        let group_id = GroupId::from_slice(&[7u8; 8]);
        let original_hash = [9u8; 32];
        let encrypted_hash = [10u8; 32];
        let nonce = [11u8; 12];
        let tag = audio_imeta_tag(
            encrypted_hash,
            original_hash,
            nonce,
            ["duration 12.345", "waveform 0 8 42 100"],
        );
        let event = event_with_tag(t.pubkey, tag);
        let parsed_references = media_files.parse_imeta_tags_from_event(&event).unwrap();

        assert_eq!(parsed_references.len(), 1);
        assert_eq!(parsed_references[0].duration_ms, Some(12_345));
        assert_eq!(parsed_references[0].waveform, Some(vec![0, 8, 42, 100]));

        media_files
            .store_parsed_media_references(&group_id, &t.pubkey, parsed_references)
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
        let tag = audio_imeta_tag(
            encrypted_hash,
            original_hash,
            nonce,
            ["duration not-a-number", "waveform 0 101", "thumbhash thumb"],
        );
        let event = event_with_tag(t.pubkey, tag);
        let parsed_references = media_files.parse_imeta_tags_from_event(&event).unwrap();

        assert_eq!(parsed_references.len(), 1);
        assert_eq!(parsed_references[0].thumbhash, Some("thumb".to_string()));
        assert!(parsed_references[0].duration_ms.is_none());
        assert!(parsed_references[0].waveform.is_none());

        media_files
            .store_parsed_media_references(&group_id, &t.pubkey, parsed_references)
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
        let tag_values = tag.clone().to_vec();

        assert!(tag_values.contains(&format!("x {}", hex::encode(original_hash))));
        assert!(tag_values.contains(&"duration 12.345".to_string()));
        assert!(tag_values.contains(&"waveform 0 8 42 100".to_string()));
        assert!(tag_values.contains(&"v mip04-v2".to_string()));

        let parsed = MediaFiles::parse_imeta_tag(&tag).unwrap();
        assert_eq!(parsed.duration_ms, Some(12_345));
        assert_eq!(parsed.waveform, Some(vec![0, 8, 42, 100]));
    }

    #[test]
    fn test_parse_imeta_canonicalizes_mime_type_without_obsolete_mls_storage() {
        let original_hash = [9u8; 32];
        let encrypted_hash = [10u8; 32];
        let nonce = [11u8; 12];
        let tag = Tag::custom(
            TagKind::Custom("imeta".into()),
            [
                format!(
                    "url https://blossom.example.com/{}",
                    hex::encode(encrypted_hash)
                ),
                "m Image/JPG ; charset=utf-8".to_string(),
                "filename photo.jpg".to_string(),
                format!("x {}", hex::encode(original_hash)),
                format!("n {}", hex::encode(nonce)),
                "v mip04-v2".to_string(),
            ],
        );

        let parsed = MediaFiles::parse_imeta_tag(&tag).unwrap();

        assert_eq!(parsed.mime_type, "image/jpeg");
    }

    #[test]
    fn test_build_chat_media_imeta_tag_omits_invalid_optional_audio_metadata() {
        let original_hash = [9u8; 32];
        let encrypted_hash = [10u8; 32];
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
            file_metadata: Some(FileMetadata {
                original_filename: Some("voice.mp3".to_string()),
                dimensions: None,
                blurhash: None,
                thumbhash: None,
                duration_ms: Some(0),
                waveform: Some(vec![0, 101]),
            }),
            nonce: Some("0102030405060708090a0b0c".to_string()),
            scheme_version: Some("mip04-v2".to_string()),
            created_at: chrono::Utc::now(),
        };

        let tag = build_chat_media_imeta_tag(&media_file, None).unwrap();
        let tag_values = tag.to_vec();

        assert!(
            !tag_values
                .iter()
                .any(|value| value.starts_with("duration "))
        );
        assert!(
            !tag_values
                .iter()
                .any(|value| value.starts_with("waveform "))
        );
        assert!(tag_values.contains(&format!("x {}", hex::encode(original_hash))));
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
