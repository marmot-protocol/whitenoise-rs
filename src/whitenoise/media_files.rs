use std::path::{Path, PathBuf};

use mdk_core::{
    GroupId,
    encrypted_media::{manager::EncryptedMediaManager, types::MediaReference},
    prelude::MdkStorageProvider,
};
use nostr_sdk::prelude::*;

pub use crate::whitenoise::database::media_files::MediaFile;
use crate::whitenoise::{
    database::{
        Database,
        media_files::{FileMetadata, MediaFileParams},
    },
    error::{Result, WhitenoiseError},
    storage::Storage,
};

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
    database: &'a Database,
}

impl<'a> MediaFiles<'a> {
    /// Creates a new MediaFiles orchestrator
    ///
    /// # Arguments
    /// * `storage` - Reference to the storage layer
    /// * `database` - Reference to the database
    pub(crate) fn new(storage: &'a Storage, database: &'a Database) -> Self {
        Self { storage, database }
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
    pub(crate) async fn record_in_database(
        &self,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
        file_path: &Path,
        upload: MediaFileUpload<'_>,
    ) -> Result<MediaFile> {
        let media_file = MediaFile::save(
            self.database,
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
                nonce: None,            // Group images use key/nonce encryption, not MDK
                scheme_version: None,   // Group images use key/nonce encryption, not MDK
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

            // Extract blurhash (optional field that MDK doesn't parse)
            let blurhash = Self::extract_blurhash_from_tag(tag);

            parsed.push(ParsedMediaReference {
                reference,
                encrypted_hash: encrypted_file_hash,
                blurhash,
            });
        }

        Ok(parsed)
    }

    /// Extracts blurhash from an imeta tag
    ///
    /// MDK's parser doesn't extract blurhash, so we do it ourselves.
    /// Format: "blurhash <blurhash_string>"
    fn extract_blurhash_from_tag(tag: &Tag) -> Option<String> {
        let tag_vec = tag.clone().to_vec();
        for value in tag_vec.iter().skip(1) {
            if let Some(blur) = value.strip_prefix("blurhash ") {
                return Some(blur.to_string());
            }
        }
        None
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
            });

            // Create MediaFile record (without file yet - empty path until downloaded)
            // Store nonce and scheme_version for MDK decryption
            MediaFile::save(
                self.database,
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
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_store_and_record() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path).await.unwrap();
        let storage = Storage::new(temp_dir.path()).await.unwrap();

        let media_files = MediaFiles::new(&storage, &db);

        let group_id = GroupId::from_slice(&[1u8; 8]);
        let pubkey = PublicKey::from_slice(&[2u8; 32]).unwrap();
        let encrypted_file_hash = [3u8; 32];
        let test_data = b"test file content";

        // Create test account to satisfy foreign key constraint
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
            "INSERT INTO accounts (pubkey, user_id, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(pubkey.to_hex())
        .bind(user_id)
        .bind(chrono::Utc::now().timestamp())
        .bind(chrono::Utc::now().timestamp())
        .execute(&db.pool)
        .await
        .unwrap();

        // Store and record
        let upload = MediaFileUpload {
            data: test_data,
            original_file_hash: None,
            encrypted_file_hash,
            mime_type: "image/jpeg",
            media_type: "test_media",
            blossom_url: None,
            nostr_key: None,
            file_metadata: None,
        };
        let media_file = media_files
            .store_and_record(&pubkey, &group_id, "test.jpg", upload)
            .await
            .unwrap();

        // Verify file exists on disk
        assert!(media_file.file_path.exists());

        // Verify file content is correct
        let content = tokio::fs::read(&media_file.file_path).await.unwrap();
        assert_eq!(content, test_data);

        // Verify idempotency: calling store_and_record again should succeed
        let upload2 = MediaFileUpload {
            data: test_data,
            original_file_hash: None,
            encrypted_file_hash,
            mime_type: "image/jpeg",
            media_type: "test_media",
            blossom_url: None,
            nostr_key: None,
            file_metadata: None,
        };
        let media_file2 = media_files
            .store_and_record(&pubkey, &group_id, "test.jpg", upload2)
            .await
            .unwrap();

        // Should return the same path
        assert_eq!(media_file.file_path, media_file2.file_path);
    }

    #[tokio::test]
    async fn test_find_file_with_prefix() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path).await.unwrap();
        let storage = Storage::new(temp_dir.path()).await.unwrap();

        let media_files = MediaFiles::new(&storage, &db);

        // Store files directly via storage
        storage
            .media_files
            .store_file("abc123.jpg", b"jpeg data")
            .await
            .unwrap();

        // Find by prefix
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
}
