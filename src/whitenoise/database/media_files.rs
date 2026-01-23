use chrono::{DateTime, Utc};
use mdk_core::GroupId;
use nostr_sdk::PublicKey;
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use std::path::{Path, PathBuf};

use super::{Database, DatabaseError, utils::parse_timestamp};
use crate::whitenoise::error::WhitenoiseError;

/// Optional metadata for media files stored as JSONB
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct FileMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_filename: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub blurhash: Option<String>,
}

impl FileMetadata {
    /// Creates a new FileMetadata with all fields set to None
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_filename(mut self, original_filename: String) -> Self {
        self.original_filename = Some(original_filename);
        self
    }

    pub fn with_dimensions(mut self, dimensions: String) -> Self {
        self.dimensions = Some(dimensions);
        self
    }

    pub fn with_blurhash(mut self, blurhash: String) -> Self {
        self.blurhash = Some(blurhash);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.original_filename.is_none() && self.dimensions.is_none() && self.blurhash.is_none()
    }
}

/// Internal database row representation for media_files table
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct MediaFileRow {
    pub id: i64,
    pub mls_group_id: GroupId,
    pub account_pubkey: PublicKey,
    pub file_path: PathBuf,
    pub original_file_hash: Option<Vec<u8>>, // SHA-256 of decrypted content (MIP-04 x field, MDK key derivation)
    pub encrypted_file_hash: Vec<u8>,        // SHA-256 of encrypted blob (Blossom verification)
    pub mime_type: String,
    pub media_type: String,
    pub blossom_url: Option<String>,
    pub nostr_key: Option<String>,
    pub file_metadata: Option<FileMetadata>,
    pub nonce: Option<String>,               // Encryption nonce (hex-encoded, for chat_media)
    pub scheme_version: Option<String>,      // Encryption version (e.g., "mip04-v2", for chat_media)
    pub created_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for MediaFileRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Vec<u8>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let id: i64 = row.try_get("id")?;
        let mls_group_id_bytes: Vec<u8> = row.try_get("mls_group_id")?;
        let account_pubkey_str: String = row.try_get("account_pubkey")?;
        let file_path_str: String = row.try_get("file_path")?;

        // Parse MLS group ID
        let mls_group_id = GroupId::from_slice(&mls_group_id_bytes);

        // Parse account pubkey
        let account_pubkey =
            PublicKey::parse(&account_pubkey_str).map_err(|e| sqlx::Error::ColumnDecode {
                index: "account_pubkey".to_string(),
                source: Box::new(e),
            })?;

        // Parse file path
        let file_path = PathBuf::from(file_path_str);

        // Parse encrypted_file_hash from hex (required)
        let encrypted_file_hash_hex: String = row.try_get("encrypted_file_hash")?;
        let encrypted_file_hash =
            hex::decode(encrypted_file_hash_hex).map_err(|e| sqlx::Error::ColumnDecode {
                index: "encrypted_file_hash".to_string(),
                source: Box::new(e),
            })?;

        // Parse original_file_hash from hex (optional - NULL for old records and group images)
        let original_file_hash: Option<Vec<u8>> = row
            .try_get::<Option<String>, _>("original_file_hash")?
            .and_then(|hex| hex::decode(&hex).ok());

        let mime_type: String = row.try_get("mime_type")?;
        let media_type: String = row.try_get("media_type")?;
        let blossom_url: Option<String> = row.try_get("blossom_url")?;
        let nostr_key: Option<String> = row.try_get("nostr_key")?;

        // Deserialize file_metadata from JSON stored as TEXT/BLOB
        // We can't use Json<T> directly here because our generic FromRow implementation
        // doesn't constrain the database type, so we deserialize manually
        let file_metadata: Option<FileMetadata> = row
            .try_get::<Option<String>, _>("file_metadata")?
            .and_then(|json_str| serde_json::from_str(&json_str).ok());

        let nonce: Option<String> = row.try_get("nonce")?;
        let scheme_version: Option<String> = row.try_get("scheme_version")?;

        let created_at = parse_timestamp(row, "created_at")?;

        Ok(Self {
            id,
            mls_group_id,
            account_pubkey,
            file_path,
            original_file_hash,
            encrypted_file_hash,
            mime_type,
            media_type,
            blossom_url,
            nostr_key,
            file_metadata,
            nonce,
            scheme_version,
            created_at,
        })
    }
}

/// Parameters for saving a media file
#[derive(Debug, Clone)]
pub struct MediaFileParams<'a> {
    pub file_path: &'a Path,
    pub original_file_hash: Option<&'a [u8; 32]>, // SHA-256 of decrypted content (for chat_media with MDK)
    pub encrypted_file_hash: &'a [u8; 32],        // SHA-256 of encrypted blob (for Blossom)
    pub mime_type: &'a str,
    pub media_type: &'a str,
    pub blossom_url: Option<&'a str>,
    pub nostr_key: Option<&'a str>,
    pub file_metadata: Option<&'a FileMetadata>,
    pub nonce: Option<&'a str>,                   // Encryption nonce (hex-encoded, for chat_media)
    pub scheme_version: Option<&'a str>,          // Encryption version (e.g., "mip04-v2", for chat_media)
}

/// Represents a cached media file
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaFile {
    pub id: Option<i64>,
    pub mls_group_id: GroupId,
    pub account_pubkey: PublicKey,
    pub file_path: PathBuf,
    pub original_file_hash: Option<Vec<u8>>, // SHA-256 of decrypted content (MIP-04 x field, MDK key derivation)
    pub encrypted_file_hash: Vec<u8>,        // SHA-256 of encrypted blob (Blossom verification)
    pub mime_type: String,
    pub media_type: String,
    pub blossom_url: Option<String>,
    pub nostr_key: Option<String>,
    pub file_metadata: Option<FileMetadata>,
    pub nonce: Option<String>,               // Encryption nonce (hex-encoded, for chat_media)
    pub scheme_version: Option<String>,      // Encryption version (e.g., "mip04-v2", for chat_media)
    pub created_at: DateTime<Utc>,
}

impl From<MediaFileRow> for MediaFile {
    fn from(val: MediaFileRow) -> Self {
        Self {
            id: Some(val.id),
            mls_group_id: val.mls_group_id,
            account_pubkey: val.account_pubkey,
            file_path: val.file_path,
            original_file_hash: val.original_file_hash,
            encrypted_file_hash: val.encrypted_file_hash,
            mime_type: val.mime_type,
            media_type: val.media_type,
            blossom_url: val.blossom_url,
            nostr_key: val.nostr_key,
            file_metadata: val.file_metadata,
            nonce: val.nonce,
            scheme_version: val.scheme_version,
            created_at: val.created_at,
        }
    }
}

impl MediaFile {
    /// Finds a media file by its encrypted file hash
    ///
    /// Returns the first matching media file for any group/account combination.
    /// This is useful for retrieving stored metadata (like blossom_url) when you
    /// only have the encrypted hash.
    ///
    /// # Arguments
    /// * `database` - The database connection
    /// * `encrypted_file_hash` - The SHA-256 hash of the encrypted file
    ///
    /// # Returns
    /// The MediaFile if found, None otherwise
    pub(crate) async fn find_by_hash(
        database: &Database,
        encrypted_file_hash: &[u8; 32],
    ) -> Result<Option<Self>, WhitenoiseError> {
        let encrypted_file_hash_hex = hex::encode(encrypted_file_hash);

        let row_opt = sqlx::query_as::<_, MediaFileRow>(
            "SELECT id, mls_group_id, account_pubkey, file_path,
                    original_file_hash, encrypted_file_hash,
                    mime_type, media_type, blossom_url, nostr_key,
                    file_metadata, nonce, scheme_version, created_at
             FROM media_files
             WHERE encrypted_file_hash = ?
             LIMIT 1",
        )
        .bind(&encrypted_file_hash_hex)
        .fetch_optional(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(row_opt.map(Into::into))
    }

    /// Saves a cached media file to the database
    ///
    /// Inserts a new row or ignores if the record already exists
    /// (based on unique constraint on mls_group_id, encrypted_file_hash, account_pubkey)
    ///
    /// # Arguments
    /// * `database` - The database connection
    /// * `mls_group_id` - The MLS group ID
    /// * `account_pubkey` - The account public key accessing this media
    /// * `params` - Media file parameters (path, hashes, mime type, etc.)
    ///
    /// # Returns
    /// The MediaFile with the database-assigned ID
    ///
    /// # Errors
    /// Returns a [`WhitenoiseError`] if the database operation fails.
    pub(crate) async fn save(
        database: &Database,
        mls_group_id: &GroupId,
        account_pubkey: &PublicKey,
        params: MediaFileParams<'_>,
    ) -> Result<Self, WhitenoiseError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let encrypted_file_hash_hex = hex::encode(params.encrypted_file_hash);
        let original_file_hash_hex = params.original_file_hash.map(hex::encode);
        let file_path_str = params
            .file_path
            .to_str()
            .ok_or_else(|| WhitenoiseError::MediaCache("Invalid file path".to_string()))?;

        // Wrap file_metadata in Json for automatic serialization
        // Only store if not empty (optimization)
        let file_metadata_json = params.file_metadata.filter(|m| !m.is_empty()).map(Json);

        let account_pubkey_hex = account_pubkey.to_hex();

        let row_opt = sqlx::query_as::<_, MediaFileRow>(
            "INSERT INTO media_files (
                mls_group_id, account_pubkey, file_path,
                original_file_hash, encrypted_file_hash,
                mime_type, media_type, blossom_url, nostr_key,
                file_metadata, nonce, scheme_version, created_at
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (mls_group_id, encrypted_file_hash, account_pubkey)
            DO NOTHING
            RETURNING id, mls_group_id, account_pubkey, file_path,
                      original_file_hash, encrypted_file_hash,
                      mime_type, media_type, blossom_url, nostr_key,
                      file_metadata, nonce, scheme_version, created_at",
        )
        .bind(mls_group_id.as_slice())
        .bind(&account_pubkey_hex)
        .bind(file_path_str)
        .bind(original_file_hash_hex.as_ref())
        .bind(&encrypted_file_hash_hex)
        .bind(params.mime_type)
        .bind(params.media_type)
        .bind(params.blossom_url)
        .bind(params.nostr_key)
        .bind(file_metadata_json)
        .bind(params.nonce)
        .bind(params.scheme_version)
        .bind(now_ms)
        .fetch_optional(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        if let Some(row) = row_opt {
            return Ok(row.into());
        }

        // Conflict occurred - select existing row
        let existing = sqlx::query_as::<_, MediaFileRow>(
            "SELECT id, mls_group_id, account_pubkey, file_path,
                    original_file_hash, encrypted_file_hash,
                    mime_type, media_type, blossom_url, nostr_key,
                    file_metadata, nonce, scheme_version, created_at
             FROM media_files
             WHERE mls_group_id = ? AND encrypted_file_hash = ? AND account_pubkey = ?
             LIMIT 1",
        )
        .bind(mls_group_id.as_slice())
        .bind(&encrypted_file_hash_hex)
        .bind(&account_pubkey_hex)
        .fetch_one(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(existing.into())
    }

    /// Finds all media files for a specific MLS group
    ///
    /// Returns a Vec of MediaFile records for the group.
    /// This leverages the indexed mls_group_id column for efficient retrieval.
    ///
    /// # Arguments
    /// * `database` - Database connection
    /// * `group_id` - The MLS group ID to fetch media files for
    pub(crate) async fn find_by_group(
        database: &Database,
        group_id: &GroupId,
    ) -> Result<Vec<Self>, WhitenoiseError> {
        let rows = sqlx::query_as::<_, MediaFileRow>(
            "SELECT id, mls_group_id, account_pubkey, file_path,
                    original_file_hash, encrypted_file_hash,
                    mime_type, media_type, blossom_url, nostr_key,
                    file_metadata, nonce, scheme_version, created_at
             FROM media_files
             WHERE mls_group_id = ?",
        )
        .bind(group_id.as_slice())
        .fetch_all(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Finds a media file by original hash, group ID, and account (MIP-04 compliant lookup)
    ///
    /// This is the primary lookup method for media files referenced in imeta tags,
    /// as MIP-04 requires the 'x' field to contain the original content hash.
    ///
    /// The query is scoped to a specific group and account for security and correctness.
    /// In multi-account setups, the same file hash can exist in the same group but with
    /// separate records per account, so we must filter by account_pubkey to prevent
    /// cross-account cache corruption.
    ///
    /// # Arguments
    /// * `database` - The database connection
    /// * `original_file_hash` - The SHA-256 hash of the decrypted file content
    /// * `group_id` - The MLS group ID to scope the search to
    /// * `account_pubkey` - The account public key (ensures account-scoped lookup)
    ///
    /// # Returns
    /// * `Ok(Some(MediaFile))` - The matching media file for this account
    /// * `Ok(None)` - No matching media file found for this account
    /// * `Err(WhitenoiseError)` - Database error
    ///
    /// # Example
    /// ```ignore
    /// // Look up media file from imeta tag
    /// let original_hash = hex::decode(&imeta_x_field)?;
    /// if let Some(media_file) = MediaFile::find_by_original_hash_and_group(
    ///     &db, &original_hash, &group_id, &account_pubkey
    /// ).await? {
    ///     // Download and decrypt the file
    /// }
    /// ```
    pub(crate) async fn find_by_original_hash_and_group(
        database: &Database,
        original_file_hash: &[u8; 32],
        group_id: &GroupId,
        account_pubkey: &PublicKey,
    ) -> Result<Option<Self>, WhitenoiseError> {
        let hash_hex = hex::encode(original_file_hash);
        let account_hex = account_pubkey.to_hex();

        let row_opt = sqlx::query_as::<_, MediaFileRow>(
            "SELECT id, mls_group_id, account_pubkey, file_path,
                    original_file_hash, encrypted_file_hash,
                    mime_type, media_type, blossom_url, nostr_key,
                    file_metadata, nonce, scheme_version, created_at
             FROM media_files
             WHERE original_file_hash = ? AND mls_group_id = ? AND account_pubkey = ?
             LIMIT 1",
        )
        .bind(&hash_hex)
        .bind(group_id.as_slice())
        .bind(&account_hex)
        .fetch_optional(&database.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        Ok(row_opt.map(Into::into))
    }

    /// Updates the file_path for an existing media file record
    ///
    /// This method is called after downloading and caching a media file to update
    /// the database record with the local file path. It performs an atomic update
    /// and returns the complete updated record.
    ///
    /// # Arguments
    /// * `database` - The database connection
    /// * `id` - The media file ID to update
    /// * `new_path` - The new file path to set
    ///
    /// # Returns
    /// * `Ok(MediaFile)` - The updated media file record
    /// * `Err(WhitenoiseError)` - If the record doesn't exist or database error
    ///
    /// # Example
    /// ```ignore
    /// // After downloading and caching a file
    /// let cached_path = PathBuf::from("/cache/media/abc123.jpg");
    /// let updated = MediaFile::update_file_path(&db, media_file.id.unwrap(), &cached_path).await?;
    /// ```
    pub(crate) async fn update_file_path(
        database: &Database,
        id: i64,
        new_path: &Path,
    ) -> Result<Self, WhitenoiseError> {
        let path_str = new_path
            .to_str()
            .ok_or_else(|| WhitenoiseError::MediaCache("Invalid file path".to_string()))?;

        let row = sqlx::query_as::<_, MediaFileRow>(
            "UPDATE media_files
             SET file_path = ?
             WHERE id = ?
             RETURNING id, mls_group_id, account_pubkey, file_path,
                       original_file_hash, encrypted_file_hash,
                       mime_type, media_type, blossom_url, nostr_key,
                       file_metadata, created_at",
        )
        .bind(path_str)
        .bind(id)
        .fetch_one(&database.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => {
                WhitenoiseError::MediaCache(format!("MediaFile with id {} not found", id))
            }
            _ => DatabaseError::Sqlx(e).into(),
        })?;

        Ok(row.into())
    }

    /// Check if this media file is an image
    pub fn is_image(&self) -> bool {
        self.mime_type.starts_with("image/")
    }

    /// Check if this media file is a video
    pub fn is_video(&self) -> bool {
        self.mime_type.starts_with("video/")
    }

    /// Check if this media file is audio
    pub fn is_audio(&self) -> bool {
        self.mime_type.starts_with("audio/")
    }

    /// Check if this media file is a document
    pub fn is_document(&self) -> bool {
        self.mime_type == "application/pdf"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn create_test_account(db: &Database, pubkey: &PublicKey) {
        // Create test user and account to satisfy foreign key constraints
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
    }

    #[tokio::test]
    async fn test_save_media_file() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path).await.unwrap();

        // Create a test group ID
        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let pubkey = PublicKey::from_slice(&[2u8; 32]).unwrap();
        let encrypted_file_hash = [3u8; 32];
        let file_path = temp_dir.path().join("test.jpg");

        // Create test account to satisfy foreign key constraint
        create_test_account(&db, &pubkey).await;

        // Save media - the save method returns the persisted record
        // Group images don't have original_file_hash (they use key/nonce encryption)
        let media_file = MediaFile::save(
            &db,
            &group_id,
            &pubkey,
            MediaFileParams {
                file_path: &file_path,
                original_file_hash: None,
                encrypted_file_hash: &encrypted_file_hash,
                mime_type: "image/jpeg",
                media_type: "group_image",
                blossom_url: None,
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Verify the returned record has correct data
        assert!(media_file.id.is_some());
        assert!(media_file.id.unwrap() > 0);
        assert_eq!(media_file.encrypted_file_hash, encrypted_file_hash.to_vec());
        assert_eq!(media_file.original_file_hash, None);
        assert_eq!(media_file.mime_type, "image/jpeg");
        assert_eq!(media_file.media_type, "group_image");
        assert_eq!(media_file.mls_group_id, group_id);
        assert_eq!(media_file.account_pubkey, pubkey);
    }

    #[tokio::test]
    async fn test_upsert_on_conflict() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path).await.unwrap();

        // Create a test group ID
        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let pubkey = PublicKey::from_slice(&[2u8; 32]).unwrap();
        let encrypted_file_hash = [3u8; 32];
        let file_path = temp_dir.path().join("test.jpg");

        // Create test account to satisfy foreign key constraint
        create_test_account(&db, &pubkey).await;

        // Save media first time
        let first_save = MediaFile::save(
            &db,
            &group_id,
            &pubkey,
            MediaFileParams {
                file_path: &file_path,
                original_file_hash: None,
                encrypted_file_hash: &encrypted_file_hash,
                mime_type: "image/jpeg",
                media_type: "group_image",
                blossom_url: Some("https://example.com/blob1"),
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        assert!(first_save.id.is_some());
        let first_id = first_save.id.unwrap();

        // Save same media again (should trigger conflict and return existing row)
        let second_save = MediaFile::save(
            &db,
            &group_id,
            &pubkey,
            MediaFileParams {
                file_path: &file_path,
                original_file_hash: None,
                encrypted_file_hash: &encrypted_file_hash,
                mime_type: "image/jpeg",
                media_type: "group_image",
                blossom_url: Some("https://example.com/blob2"),
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        assert!(second_save.id.is_some());
        let second_id = second_save.id.unwrap();

        // Both saves should return the same ID (existing row)
        assert_eq!(first_id, second_id);
        // Original blossom_url should be preserved
        assert_eq!(
            second_save.blossom_url,
            Some("https://example.com/blob1".to_string())
        );
    }

    #[tokio::test]
    async fn test_find_by_hash_returns_first_match() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path).await.unwrap();

        // Test with multiple records having same encrypted hash (different groups/accounts)
        let group_id1 = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let group_id2 = mdk_core::GroupId::from_slice(&[2u8; 8]);
        let pubkey1 = PublicKey::from_slice(&[10u8; 32]).unwrap();
        let pubkey2 = PublicKey::from_slice(&[20u8; 32]).unwrap();
        let encrypted_file_hash = [42u8; 32];
        let file_path1 = temp_dir.path().join("test1.jpg");
        let file_path2 = temp_dir.path().join("test2.jpg");

        create_test_account(&db, &pubkey1).await;
        create_test_account(&db, &pubkey2).await;

        // Create metadata for first record
        let metadata = FileMetadata::new()
            .with_filename("original.jpg".to_string())
            .with_dimensions("1920x1080".to_string())
            .with_blurhash("LEHV6nWB2yk8pyo0adR*.7kCMdnj".to_string());

        // Save first record with metadata
        let first_save = MediaFile::save(
            &db,
            &group_id1,
            &pubkey1,
            MediaFileParams {
                file_path: &file_path1,
                original_file_hash: None,
                encrypted_file_hash: &encrypted_file_hash,
                mime_type: "image/jpeg",
                media_type: "group_image",
                blossom_url: Some("https://blossom.example.com/hash42"),
                nostr_key: None,
                file_metadata: Some(&metadata),
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Save second record with same encrypted hash but different details
        MediaFile::save(
            &db,
            &group_id2,
            &pubkey2,
            MediaFileParams {
                file_path: &file_path2,
                original_file_hash: None,
                encrypted_file_hash: &encrypted_file_hash,
                mime_type: "image/png",
                media_type: "group_image",
                blossom_url: Some("https://another-server.com/hash42"),
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Find by hash should return the first inserted record
        let found = MediaFile::find_by_hash(&db, &encrypted_file_hash)
            .await
            .unwrap();

        assert!(found.is_some());
        let media_file = found.unwrap();

        // Verify it returns the first record
        assert_eq!(media_file.id, first_save.id);
        assert_eq!(media_file.encrypted_file_hash, encrypted_file_hash.to_vec());
        assert_eq!(media_file.mls_group_id, group_id1);
        assert_eq!(media_file.account_pubkey, pubkey1);
        assert_eq!(media_file.mime_type, "image/jpeg");
        assert_eq!(media_file.media_type, "group_image");
        assert_eq!(
            media_file.blossom_url,
            Some("https://blossom.example.com/hash42".to_string())
        );

        // Verify metadata is preserved
        assert!(media_file.file_metadata.is_some());
        let retrieved_metadata = media_file.file_metadata.unwrap();
        assert_eq!(
            retrieved_metadata.original_filename,
            Some("original.jpg".to_string())
        );
        assert_eq!(retrieved_metadata.dimensions, Some("1920x1080".to_string()));
        assert_eq!(
            retrieved_metadata.blurhash,
            Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj".to_string())
        );
    }

    #[tokio::test]
    async fn test_find_by_hash_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path).await.unwrap();

        let nonexistent_hash = [99u8; 32];

        // Try to find a hash that doesn't exist
        let found = MediaFile::find_by_hash(&db, &nonexistent_hash)
            .await
            .unwrap();

        assert!(found.is_none());
    }

    #[tokio::test]
    async fn test_find_by_group_empty_result() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path).await.unwrap();

        let nonexistent_group_id = mdk_core::GroupId::from_slice(&[99u8; 8]);

        // Try to find media for a group that doesn't exist
        let media_files = MediaFile::find_by_group(&db, &nonexistent_group_id)
            .await
            .unwrap();

        assert!(media_files.is_empty());
    }

    #[tokio::test]
    async fn test_find_by_group_multiple_files_and_group_isolation() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path).await.unwrap();

        let group_id1 = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let group_id2 = mdk_core::GroupId::from_slice(&[2u8; 8]);
        let pubkey1 = PublicKey::from_slice(&[10u8; 32]).unwrap();
        let pubkey2 = PublicKey::from_slice(&[20u8; 32]).unwrap();

        create_test_account(&db, &pubkey1).await;
        create_test_account(&db, &pubkey2).await;

        // Create metadata for one file
        let metadata = FileMetadata::new()
            .with_filename("image1.jpg".to_string())
            .with_dimensions("1920x1080".to_string());

        // Save multiple media files for group 1 (different accounts)
        // For chat_media, we have both original and encrypted hashes
        let original_hash1a = [11u8; 32];
        let encrypted_hash1a = [111u8; 32];
        let original_hash1b = [12u8; 32];
        let encrypted_hash1b = [121u8; 32];
        let file_path1a = temp_dir.path().join("group1_file1.jpg");
        let file_path1b = temp_dir.path().join("group1_file2.png");

        MediaFile::save(
            &db,
            &group_id1,
            &pubkey1,
            MediaFileParams {
                file_path: &file_path1a,
                original_file_hash: Some(&original_hash1a),
                encrypted_file_hash: &encrypted_hash1a,
                mime_type: "image/jpeg",
                media_type: "chat_media",
                blossom_url: Some("https://example.com/blob1a"),
                nostr_key: Some("nostr_key_1a"),
                file_metadata: Some(&metadata),
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        MediaFile::save(
            &db,
            &group_id1,
            &pubkey2,
            MediaFileParams {
                file_path: &file_path1b,
                original_file_hash: Some(&original_hash1b),
                encrypted_file_hash: &encrypted_hash1b,
                mime_type: "image/png",
                media_type: "chat_media",
                blossom_url: Some("https://example.com/blob1b"),
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Save one media file for group 2
        let original_hash2 = [22u8; 32];
        let encrypted_hash2 = [222u8; 32];
        let file_path2 = temp_dir.path().join("group2_file.jpg");

        MediaFile::save(
            &db,
            &group_id2,
            &pubkey1,
            MediaFileParams {
                file_path: &file_path2,
                original_file_hash: Some(&original_hash2),
                encrypted_file_hash: &encrypted_hash2,
                mime_type: "image/jpeg",
                media_type: "chat_media",
                blossom_url: Some("https://example.com/blob2"),
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Test: Find all media files for group 1
        let media_files_group1 = MediaFile::find_by_group(&db, &group_id1).await.unwrap();

        // Should return both files from group 1 regardless of account
        assert_eq!(media_files_group1.len(), 2);

        // Verify we got both files from group 1 (checking encrypted_file_hash)
        let encrypted_hashes1: Vec<Vec<u8>> = media_files_group1
            .iter()
            .map(|mf| mf.encrypted_file_hash.clone())
            .collect();
        assert!(encrypted_hashes1.contains(&encrypted_hash1a.to_vec()));
        assert!(encrypted_hashes1.contains(&encrypted_hash1b.to_vec()));
        assert!(!encrypted_hashes1.contains(&encrypted_hash2.to_vec())); // Should not contain group 2 file

        // Verify all files have correct group_id
        assert!(
            media_files_group1
                .iter()
                .all(|mf| mf.mls_group_id == group_id1)
        );

        // Verify metadata is preserved for the file that has it
        let file_with_metadata = media_files_group1
            .iter()
            .find(|mf| mf.encrypted_file_hash == encrypted_hash1a.to_vec())
            .unwrap();
        assert!(file_with_metadata.file_metadata.is_some());
        assert_eq!(
            file_with_metadata
                .file_metadata
                .as_ref()
                .unwrap()
                .original_filename,
            Some("image1.jpg".to_string())
        );
        // Verify chat_media has both hashes
        assert!(file_with_metadata.original_file_hash.is_some());

        // Test: Find all media files for group 2
        let media_files_group2 = MediaFile::find_by_group(&db, &group_id2).await.unwrap();

        // Should return only one file from group 2
        assert_eq!(media_files_group2.len(), 1);
        assert_eq!(
            media_files_group2[0].encrypted_file_hash,
            encrypted_hash2.to_vec()
        );
        assert_eq!(media_files_group2[0].mls_group_id, group_id2);
        // Verify chat_media has original_file_hash
        assert!(media_files_group2[0].original_file_hash.is_some());

        // Verify groups are properly isolated
        assert_ne!(
            media_files_group1.len(),
            media_files_group2.len(),
            "Different groups should have different file counts"
        );
        let encrypted_hashes2: Vec<Vec<u8>> = media_files_group2
            .iter()
            .map(|mf| mf.encrypted_file_hash.clone())
            .collect();
        assert!(!encrypted_hashes2.contains(&encrypted_hash1a.to_vec()));
        assert!(!encrypted_hashes2.contains(&encrypted_hash1b.to_vec()));
    }

    #[test]
    fn test_is_image() {
        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let pubkey = PublicKey::from_slice(&[2u8; 32]).unwrap();
        let encrypted_file_hash = vec![3u8; 32];

        // Test various image MIME types
        let image_types = vec!["image/jpeg", "image/png", "image/gif", "image/webp"];
        for mime_type in image_types {
            let media_file = MediaFile {
                id: Some(1),
                mls_group_id: group_id.clone(),
                account_pubkey: pubkey,
                file_path: PathBuf::from("/test.jpg"),
                original_file_hash: None,
                encrypted_file_hash: encrypted_file_hash.clone(),
                mime_type: mime_type.to_string(),
                media_type: "test".to_string(),
                blossom_url: None,
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
                created_at: Utc::now(),
            };
            assert!(media_file.is_image(), "Failed for MIME type: {}", mime_type);
        }

        // Test non-image types
        let non_image_types = vec!["video/mp4", "audio/mpeg", "application/pdf"];
        for mime_type in non_image_types {
            let media_file = MediaFile {
                id: Some(1),
                mls_group_id: group_id.clone(),
                account_pubkey: pubkey,
                file_path: PathBuf::from("/test.file"),
                original_file_hash: None,
                encrypted_file_hash: encrypted_file_hash.clone(),
                mime_type: mime_type.to_string(),
                media_type: "test".to_string(),
                blossom_url: None,
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
                created_at: Utc::now(),
            };
            assert!(
                !media_file.is_image(),
                "Should fail for MIME type: {}",
                mime_type
            );
        }
    }

    #[test]
    fn test_is_video() {
        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let pubkey = PublicKey::from_slice(&[2u8; 32]).unwrap();
        let encrypted_file_hash = vec![3u8; 32];

        // Test various video MIME types
        let video_types = vec!["video/mp4", "video/webm", "video/quicktime"];
        for mime_type in video_types {
            let media_file = MediaFile {
                id: Some(1),
                mls_group_id: group_id.clone(),
                account_pubkey: pubkey,
                file_path: PathBuf::from("/test.mp4"),
                original_file_hash: None,
                encrypted_file_hash: encrypted_file_hash.clone(),
                mime_type: mime_type.to_string(),
                media_type: "test".to_string(),
                blossom_url: None,
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
                created_at: Utc::now(),
            };
            assert!(media_file.is_video(), "Failed for MIME type: {}", mime_type);
        }

        // Test non-video types
        let non_video_types = vec!["image/jpeg", "audio/mpeg", "application/pdf"];
        for mime_type in non_video_types {
            let media_file = MediaFile {
                id: Some(1),
                mls_group_id: group_id.clone(),
                account_pubkey: pubkey,
                file_path: PathBuf::from("/test.file"),
                original_file_hash: None,
                encrypted_file_hash: encrypted_file_hash.clone(),
                mime_type: mime_type.to_string(),
                media_type: "test".to_string(),
                blossom_url: None,
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
                created_at: Utc::now(),
            };
            assert!(
                !media_file.is_video(),
                "Should fail for MIME type: {}",
                mime_type
            );
        }
    }

    #[test]
    fn test_is_audio() {
        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let pubkey = PublicKey::from_slice(&[2u8; 32]).unwrap();
        let encrypted_file_hash = vec![3u8; 32];

        // Test various audio MIME types
        let audio_types = vec![
            "audio/mpeg",
            "audio/ogg",
            "audio/mp4",
            "audio/m4a",
            "audio/wav",
            "audio/x-wav",
        ];
        for mime_type in audio_types {
            let media_file = MediaFile {
                id: Some(1),
                mls_group_id: group_id.clone(),
                account_pubkey: pubkey,
                file_path: PathBuf::from("/test.mp3"),
                original_file_hash: None,
                encrypted_file_hash: encrypted_file_hash.clone(),
                mime_type: mime_type.to_string(),
                media_type: "test".to_string(),
                blossom_url: None,
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
                created_at: Utc::now(),
            };
            assert!(media_file.is_audio(), "Failed for MIME type: {}", mime_type);
        }

        // Test non-audio types
        let non_audio_types = vec!["image/jpeg", "video/mp4", "application/pdf"];
        for mime_type in non_audio_types {
            let media_file = MediaFile {
                id: Some(1),
                mls_group_id: group_id.clone(),
                account_pubkey: pubkey,
                file_path: PathBuf::from("/test.file"),
                original_file_hash: None,
                encrypted_file_hash: encrypted_file_hash.clone(),
                mime_type: mime_type.to_string(),
                media_type: "test".to_string(),
                blossom_url: None,
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
                created_at: Utc::now(),
            };
            assert!(
                !media_file.is_audio(),
                "Should fail for MIME type: {}",
                mime_type
            );
        }
    }

    #[test]
    fn test_is_document() {
        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let pubkey = PublicKey::from_slice(&[2u8; 32]).unwrap();
        let encrypted_file_hash = vec![3u8; 32];

        // Test PDF
        let media_file = MediaFile {
            id: Some(1),
            mls_group_id: group_id.clone(),
            account_pubkey: pubkey,
            file_path: PathBuf::from("/test.pdf"),
            original_file_hash: None,
            encrypted_file_hash: encrypted_file_hash.clone(),
            mime_type: "application/pdf".to_string(),
            media_type: "test".to_string(),
            blossom_url: None,
            nostr_key: None,
            file_metadata: None,
            nonce: None,
            scheme_version: None,
            created_at: Utc::now(),
        };
        assert!(media_file.is_document());

        // Test non-document types
        let non_document_types = vec!["image/jpeg", "video/mp4", "audio/mpeg", "application/json"];
        for mime_type in non_document_types {
            let media_file = MediaFile {
                id: Some(1),
                mls_group_id: group_id.clone(),
                account_pubkey: pubkey,
                file_path: PathBuf::from("/test.file"),
                original_file_hash: None,
                encrypted_file_hash: encrypted_file_hash.clone(),
                mime_type: mime_type.to_string(),
                media_type: "test".to_string(),
                blossom_url: None,
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
                created_at: Utc::now(),
            };
            assert!(
                !media_file.is_document(),
                "Should fail for MIME type: {}",
                mime_type
            );
        }
    }

    #[test]
    fn test_media_type_edge_cases() {
        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let pubkey = PublicKey::from_slice(&[2u8; 32]).unwrap();
        let encrypted_file_hash = vec![3u8; 32];

        // Test empty MIME type
        let media_file = MediaFile {
            id: Some(1),
            mls_group_id: group_id.clone(),
            account_pubkey: pubkey,
            file_path: PathBuf::from("/test.file"),
            original_file_hash: None,
            encrypted_file_hash: encrypted_file_hash.clone(),
            mime_type: "".to_string(),
            media_type: "test".to_string(),
            blossom_url: None,
            nostr_key: None,
            file_metadata: None,
            nonce: None,
            scheme_version: None,
            created_at: Utc::now(),
        };
        assert!(!media_file.is_image());
        assert!(!media_file.is_video());
        assert!(!media_file.is_audio());
        assert!(!media_file.is_document());

        // Test malformed MIME type
        let media_file = MediaFile {
            id: Some(1),
            mls_group_id: group_id.clone(),
            account_pubkey: pubkey,
            file_path: PathBuf::from("/test.file"),
            original_file_hash: None,
            encrypted_file_hash: encrypted_file_hash.clone(),
            mime_type: "notamimetype".to_string(),
            media_type: "test".to_string(),
            blossom_url: None,
            nostr_key: None,
            file_metadata: None,
            nonce: None,
            scheme_version: None,
            created_at: Utc::now(),
        };
        assert!(!media_file.is_image());
        assert!(!media_file.is_video());
        assert!(!media_file.is_audio());
        assert!(!media_file.is_document());
    }

    #[tokio::test]
    async fn test_update_file_path() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path).await.unwrap();

        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let pubkey = PublicKey::from_slice(&[10u8; 32]).unwrap();
        let original_hash = [11u8; 32];
        let encrypted_hash = [111u8; 32];
        let initial_path = temp_dir.path().join("initial.jpg");
        let new_path = temp_dir.path().join("updated.jpg");

        create_test_account(&db, &pubkey).await;

        // Create metadata to verify it's preserved
        let metadata = FileMetadata::new()
            .with_filename("test.jpg".to_string())
            .with_dimensions("800x600".to_string());

        // Create initial media file record
        let media_file = MediaFile::save(
            &db,
            &group_id,
            &pubkey,
            MediaFileParams {
                file_path: &initial_path,
                original_file_hash: Some(&original_hash),
                encrypted_file_hash: &encrypted_hash,
                mime_type: "image/jpeg",
                media_type: "chat_media",
                blossom_url: Some("https://example.com/blob"),
                nostr_key: Some("test_key"),
                file_metadata: Some(&metadata),
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        let original_id = media_file.id.unwrap();

        // Verify initial state
        assert_eq!(media_file.file_path, initial_path);
        assert_eq!(media_file.original_file_hash, Some(original_hash.to_vec()));
        assert_eq!(media_file.encrypted_file_hash, encrypted_hash.to_vec());

        // Update the file path
        let updated = MediaFile::update_file_path(&db, original_id, &new_path)
            .await
            .unwrap();

        // Verify path was updated
        assert_eq!(updated.file_path, new_path);

        // Verify ID is preserved
        assert_eq!(updated.id, Some(original_id));

        // Verify all other fields remain unchanged
        assert_eq!(updated.mls_group_id, group_id);
        assert_eq!(updated.account_pubkey, pubkey);
        assert_eq!(updated.original_file_hash, Some(original_hash.to_vec()));
        assert_eq!(updated.encrypted_file_hash, encrypted_hash.to_vec());
        assert_eq!(updated.mime_type, "image/jpeg");
        assert_eq!(updated.media_type, "chat_media");
        assert_eq!(
            updated.blossom_url,
            Some("https://example.com/blob".to_string())
        );
        assert_eq!(updated.nostr_key, Some("test_key".to_string()));
        assert!(updated.file_metadata.is_some());
        assert_eq!(
            updated.file_metadata.as_ref().unwrap().original_filename,
            Some("test.jpg".to_string())
        );

        // Verify the update persisted by fetching again
        let fetched =
            MediaFile::find_by_original_hash_and_group(&db, &original_hash, &group_id, &pubkey)
                .await
                .unwrap()
                .unwrap();

        assert_eq!(fetched.file_path, new_path);
        assert_eq!(fetched.id, Some(original_id));
    }

    #[tokio::test]
    async fn test_update_file_path_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path).await.unwrap();

        let nonexistent_id = 99999;
        let new_path = temp_dir.path().join("new.jpg");

        // Try to update a nonexistent record
        let result = MediaFile::update_file_path(&db, nonexistent_id, &new_path).await;

        assert!(result.is_err());
        match result {
            Err(WhitenoiseError::MediaCache(msg)) => {
                assert!(msg.contains("not found"));
            }
            _ => panic!("Expected MediaCache error for nonexistent record"),
        }
    }

    #[tokio::test]
    async fn test_find_by_original_hash_and_group() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path).await.unwrap();

        // Create test data
        let group_id1 = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let group_id2 = mdk_core::GroupId::from_slice(&[2u8; 8]);
        let pubkey = PublicKey::from_slice(&[10u8; 32]).unwrap();
        let original_hash1 = [11u8; 32];
        let original_hash2 = [12u8; 32];
        let encrypted_hash1 = [111u8; 32];
        let encrypted_hash2 = [121u8; 32];
        let file_path1 = temp_dir.path().join("chat_media1.jpg");
        let file_path2 = temp_dir.path().join("chat_media2.png");

        create_test_account(&db, &pubkey).await;

        // Create metadata for first file
        let metadata = FileMetadata::new()
            .with_filename("test_image.jpg".to_string())
            .with_dimensions("1920x1080".to_string());

        // Save first chat media file in group 1 with original hash
        let saved_file1 = MediaFile::save(
            &db,
            &group_id1,
            &pubkey,
            MediaFileParams {
                file_path: &file_path1,
                original_file_hash: Some(&original_hash1),
                encrypted_file_hash: &encrypted_hash1,
                mime_type: "image/jpeg",
                media_type: "chat_media",
                blossom_url: Some("https://example.com/blob1"),
                nostr_key: Some("test_key_1"),
                file_metadata: Some(&metadata),
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Save second chat media file in group 2 with different original hash
        MediaFile::save(
            &db,
            &group_id2,
            &pubkey,
            MediaFileParams {
                file_path: &file_path2,
                original_file_hash: Some(&original_hash2),
                encrypted_file_hash: &encrypted_hash2,
                mime_type: "image/png",
                media_type: "chat_media",
                blossom_url: Some("https://example.com/blob2"),
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Save a group image (without original_file_hash) in group 1
        let encrypted_hash_group_image = [99u8; 32];
        let file_path_group_image = temp_dir.path().join("group_image.jpg");
        MediaFile::save(
            &db,
            &group_id1,
            &pubkey,
            MediaFileParams {
                file_path: &file_path_group_image,
                original_file_hash: None, // Group images don't have original_file_hash
                encrypted_file_hash: &encrypted_hash_group_image,
                mime_type: "image/jpeg",
                media_type: "group_image",
                blossom_url: Some("https://example.com/group_img"),
                nostr_key: Some("group_key"),
                file_metadata: None,
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Test 1: Find file with correct original hash, group, and account
        let found =
            MediaFile::find_by_original_hash_and_group(&db, &original_hash1, &group_id1, &pubkey)
                .await
                .unwrap();

        assert!(
            found.is_some(),
            "Should find file with correct hash, group, and account"
        );
        let media_file = found.unwrap();
        assert_eq!(media_file.id, saved_file1.id);
        assert_eq!(media_file.original_file_hash, Some(original_hash1.to_vec()));
        assert_eq!(media_file.encrypted_file_hash, encrypted_hash1.to_vec());
        assert_eq!(media_file.mls_group_id, group_id1);
        assert_eq!(media_file.mime_type, "image/jpeg");
        assert_eq!(media_file.media_type, "chat_media");
        assert!(media_file.file_metadata.is_some());

        // Test 2: Should not find file with correct hash and account but wrong group
        let not_found =
            MediaFile::find_by_original_hash_and_group(&db, &original_hash1, &group_id2, &pubkey)
                .await
                .unwrap();

        assert!(
            not_found.is_none(),
            "Should not find file with correct hash and account but wrong group"
        );

        // Test 3: Should not find file with wrong hash but correct group and account
        let wrong_hash = [255u8; 32];
        let not_found =
            MediaFile::find_by_original_hash_and_group(&db, &wrong_hash, &group_id1, &pubkey)
                .await
                .unwrap();

        assert!(not_found.is_none(), "Should not find file with wrong hash");

        // Test 4: Find second file in different group with same account
        let found =
            MediaFile::find_by_original_hash_and_group(&db, &original_hash2, &group_id2, &pubkey)
                .await
                .unwrap();

        assert!(found.is_some(), "Should find second file in group 2");
        let media_file2 = found.unwrap();
        assert_eq!(
            media_file2.original_file_hash,
            Some(original_hash2.to_vec())
        );
        assert_eq!(media_file2.mls_group_id, group_id2);
        assert_eq!(media_file2.mime_type, "image/png");

        // Test 5: Verify this method is MIP-04 specific (uses original_file_hash)
        // The group image has no original_file_hash, so it should not be found
        let nonexistent_hash = [100u8; 32];
        let not_found =
            MediaFile::find_by_original_hash_and_group(&db, &nonexistent_hash, &group_id1, &pubkey)
                .await
                .unwrap();

        assert!(
            not_found.is_none(),
            "Should not find group image when searching by original_file_hash"
        );
    }

    #[tokio::test]
    async fn test_find_by_original_hash_and_group_multi_account() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Database::new(db_path).await.unwrap();

        // Create test data for multi-account scenario
        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let account1_pubkey = PublicKey::from_slice(&[10u8; 32]).unwrap();
        let account2_pubkey = PublicKey::from_slice(&[20u8; 32]).unwrap();
        let original_hash = [11u8; 32]; // Same media file
        let encrypted_hash = [111u8; 32]; // Same encrypted hash
        let file_path1 = temp_dir.path().join("account1_media.jpg");
        let file_path2 = temp_dir.path().join("account2_media.jpg");

        create_test_account(&db, &account1_pubkey).await;
        create_test_account(&db, &account2_pubkey).await;

        // Account 1 saves media file (e.g., after uploading)
        let saved_file1 = MediaFile::save(
            &db,
            &group_id,
            &account1_pubkey,
            MediaFileParams {
                file_path: &file_path1,
                original_file_hash: Some(&original_hash),
                encrypted_file_hash: &encrypted_hash,
                mime_type: "image/jpeg",
                media_type: "chat_media",
                blossom_url: Some("https://example.com/blob"),
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Account 2 saves reference to same media file (e.g., after receiving message)
        let saved_file2 = MediaFile::save(
            &db,
            &group_id,
            &account2_pubkey,
            MediaFileParams {
                file_path: &file_path2, // Different file path for account 2
                original_file_hash: Some(&original_hash),
                encrypted_file_hash: &encrypted_hash,
                mime_type: "image/jpeg",
                media_type: "chat_media",
                blossom_url: Some("https://example.com/blob"),
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Verify that querying with account1 returns account1's record
        let found1 = MediaFile::find_by_original_hash_and_group(
            &db,
            &original_hash,
            &group_id,
            &account1_pubkey,
        )
        .await
        .unwrap()
        .expect("Should find account1's record");

        assert_eq!(found1.id, saved_file1.id);
        assert_eq!(found1.account_pubkey, account1_pubkey);
        assert_eq!(found1.file_path, file_path1);

        // Verify that querying with account2 returns account2's record (not account1's!)
        let found2 = MediaFile::find_by_original_hash_and_group(
            &db,
            &original_hash,
            &group_id,
            &account2_pubkey,
        )
        .await
        .unwrap()
        .expect("Should find account2's record");

        assert_eq!(found2.id, saved_file2.id);
        assert_eq!(found2.account_pubkey, account2_pubkey);
        assert_eq!(found2.file_path, file_path2);

        // Verify the two records are different
        assert_ne!(
            found1.id, found2.id,
            "Different accounts should have different records"
        );
        assert_ne!(
            found1.file_path, found2.file_path,
            "Each account should have their own file_path"
        );

        // Verify a third account cannot find records for the other accounts
        let account3_pubkey = PublicKey::from_slice(&[30u8; 32]).unwrap();
        create_test_account(&db, &account3_pubkey).await;
        let not_found = MediaFile::find_by_original_hash_and_group(
            &db,
            &original_hash,
            &group_id,
            &account3_pubkey,
        )
        .await
        .unwrap();

        assert!(
            not_found.is_none(),
            "Account 3 should not find records belonging to other accounts"
        );
    }
}
