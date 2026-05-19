use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use mdk_core::GroupId;
use nostr_sdk::PublicKey;
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use sqlx::{Row, SqlitePool};

use super::{Database, DatabaseError};
use crate::perf_instrument;
use crate::whitenoise::error::WhitenoiseError;

const MAX_WAVEFORM_SAMPLES: usize = 16_384;

/// Optional metadata for media files stored as JSONB
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct FileMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_filename: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub blurhash: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbhash: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub waveform: Option<Vec<u8>>,
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

    pub fn with_thumbhash(mut self, thumbhash: String) -> Self {
        self.thumbhash = Some(thumbhash);
        self
    }

    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    pub fn with_waveform(mut self, waveform: Vec<u8>) -> Self {
        self.waveform = Self::is_valid_waveform(&waveform).then_some(waveform);
        self
    }

    pub fn is_valid_waveform(waveform: &[u8]) -> bool {
        !waveform.is_empty()
            && waveform.len() <= MAX_WAVEFORM_SAMPLES
            && waveform.iter().all(|sample| *sample <= 100)
    }

    pub(crate) fn sanitized_for_storage(&self) -> Self {
        let mut sanitized = self.clone();
        if sanitized
            .waveform
            .as_deref()
            .is_some_and(|waveform| !Self::is_valid_waveform(waveform))
        {
            sanitized.waveform = None;
        }
        sanitized
    }

    pub fn is_empty(&self) -> bool {
        self.original_filename.is_none()
            && self.dimensions.is_none()
            && self.blurhash.is_none()
            && self.thumbhash.is_none()
            && self.duration_ms.is_none()
            && self.waveform.is_none()
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
    pub nonce: Option<&'a str>, // Encryption nonce (hex-encoded, for chat_media)
    pub scheme_version: Option<&'a str>, // Encryption version (e.g., "mip04-v2", for chat_media)
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
    pub nonce: Option<String>, // Encryption nonce (hex-encoded, for chat_media)
    pub scheme_version: Option<String>, // Encryption version (e.g., "mip04-v2", for chat_media)
    pub created_at: DateTime<Utc>,
}

/// Data from a `media_references` row (per-account DB).
struct ReferenceRow {
    id: i64,
    mls_group_id: Vec<u8>,
    encrypted_file_hash: String,
    original_file_hash: Option<String>,
    media_type: String,
    nostr_key: Option<String>,
    file_metadata: Option<Vec<u8>>,
    nonce: Option<String>,
    scheme_version: Option<String>,
    created_at: i64,
}

/// Data from a `media_blobs` row (shared DB).
#[derive(Clone)]
struct BlobRow {
    file_path: String,
    mime_type: String,
    blossom_url: Option<String>,
}

impl MediaFile {
    /// Assemble a `MediaFile` from a reference row (account DB) and a blob
    /// row (shared DB). The `account_pubkey` is the owning account — it is
    /// implicit in the per-account database file (no column in the table).
    fn assemble(
        r: ReferenceRow,
        b: BlobRow,
        account_pubkey: PublicKey,
    ) -> std::result::Result<Self, WhitenoiseError> {
        let mls_group_id = GroupId::from_slice(&r.mls_group_id);
        let encrypted_file_hash = hex::decode(&r.encrypted_file_hash).map_err(|e| {
            DatabaseError::Sqlx(sqlx::Error::ColumnDecode {
                index: "encrypted_file_hash".to_string(),
                source: Box::new(e),
            })
        })?;
        let original_file_hash = r
            .original_file_hash
            .and_then(|hex_str| hex::decode(&hex_str).ok());
        let file_metadata: Option<FileMetadata> = r
            .file_metadata
            .and_then(|s| serde_json::from_slice(&s).ok());
        let created_at = match chrono::DateTime::from_timestamp_millis(r.created_at) {
            Some(ts) => ts,
            None => {
                tracing::warn!(
                    target: "whitenoise::database::media_files",
                    encrypted_file_hash = %r.encrypted_file_hash,
                    raw_millis = r.created_at,
                    "media_references.created_at out of range, falling back to MIN_UTC",
                );
                chrono::DateTime::<Utc>::MIN_UTC
            }
        };

        Ok(Self {
            id: Some(r.id),
            mls_group_id,
            account_pubkey,
            file_path: PathBuf::from(b.file_path),
            original_file_hash,
            encrypted_file_hash,
            mime_type: b.mime_type,
            media_type: r.media_type,
            blossom_url: b.blossom_url,
            nostr_key: r.nostr_key,
            file_metadata,
            nonce: r.nonce,
            scheme_version: r.scheme_version,
            created_at,
        })
    }

    /// Read a `ReferenceRow` from a SQLite row.
    fn ref_from_row(
        row: &sqlx::sqlite::SqliteRow,
    ) -> std::result::Result<ReferenceRow, sqlx::Error> {
        Ok(ReferenceRow {
            id: row.try_get("id")?,
            mls_group_id: row.try_get("mls_group_id")?,
            encrypted_file_hash: row.try_get("encrypted_file_hash")?,
            original_file_hash: row.try_get("original_file_hash")?,
            media_type: row.try_get("media_type")?,
            nostr_key: row.try_get("nostr_key")?,
            file_metadata: row.try_get("file_metadata")?,
            nonce: row.try_get("nonce")?,
            scheme_version: row.try_get("scheme_version")?,
            created_at: row.try_get("created_at")?,
        })
    }

    /// Read a `BlobRow` from a SQLite row.
    fn blob_from_row(row: &sqlx::sqlite::SqliteRow) -> std::result::Result<BlobRow, sqlx::Error> {
        Ok(BlobRow {
            file_path: row.try_get("file_path")?,
            mime_type: row.try_get("mime_type")?,
            blossom_url: row.try_get("blossom_url")?,
        })
    }

    /// Fetch the blob for a given encrypted_file_hash from the shared DB.
    async fn fetch_blob(
        shared_pool: &SqlitePool,
        encrypted_file_hash_hex: &str,
    ) -> std::result::Result<Option<BlobRow>, DatabaseError> {
        let row = sqlx::query(
            "SELECT file_path, mime_type, blossom_url
             FROM media_blobs
             WHERE encrypted_file_hash = ?",
        )
        .bind(encrypted_file_hash_hex)
        .fetch_optional(shared_pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        match row {
            Some(r) => Ok(Some(Self::blob_from_row(&r).map_err(DatabaseError::Sqlx)?)),
            None => Ok(None),
        }
    }

    /// Batch-fetch blob rows for a set of encrypted file hashes in one query.
    async fn fetch_blobs_batch(
        shared_pool: &SqlitePool,
        hashes: &[&str],
    ) -> std::result::Result<HashMap<String, BlobRow>, DatabaseError> {
        if hashes.is_empty() {
            return Ok(HashMap::new());
        }

        // Deduplicate hashes — multiple references can point to the same blob.
        let unique: std::collections::HashSet<&str> = hashes.iter().copied().collect();

        // Use QueryBuilder for a single IN-clause query with proper binds.
        let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT encrypted_file_hash, file_path, mime_type, blossom_url \
             FROM media_blobs \
             WHERE encrypted_file_hash IN (",
        );
        let mut separated = qb.separated(", ");
        for hash in &unique {
            separated.push_bind(*hash);
        }
        separated.push_unseparated(")");

        let rows = qb
            .build()
            .fetch_all(shared_pool)
            .await
            .map_err(DatabaseError::Sqlx)?;

        let mut map = HashMap::with_capacity(rows.len());
        for row in &rows {
            let hash: String = row
                .try_get("encrypted_file_hash")
                .map_err(DatabaseError::Sqlx)?;
            let blob = BlobRow {
                file_path: row.try_get("file_path").map_err(DatabaseError::Sqlx)?,
                mime_type: row.try_get("mime_type").map_err(DatabaseError::Sqlx)?,
                blossom_url: row.try_get("blossom_url").map_err(DatabaseError::Sqlx)?,
            };
            map.insert(hash, blob);
        }

        Ok(map)
    }
}

impl MediaFile {
    /// Finds a media file by its encrypted file hash.
    ///
    /// Reads the reference from the **account** pool and the blob from the
    /// **shared** pool, combining them in Rust.
    ///
    /// # Arguments
    /// * `account_pool` - Per-account database pool (holds `media_references`)
    /// * `shared_db` - Shared database (holds `media_blobs`)
    /// * `account_pubkey` - The owning account (implicit in the per-account DB)
    /// * `encrypted_file_hash` - The SHA-256 hash of the encrypted file
    #[perf_instrument("db::media_files")]
    pub(crate) async fn find_by_hash(
        account_pool: &SqlitePool,
        shared_db: &Database,
        account_pubkey: &PublicKey,
        encrypted_file_hash: &[u8; 32],
    ) -> Result<Option<Self>, WhitenoiseError> {
        let hash_hex = hex::encode(encrypted_file_hash);

        let ref_row = sqlx::query(
            "SELECT id, mls_group_id, encrypted_file_hash, original_file_hash,
                    media_type, nostr_key, file_metadata, nonce, scheme_version, created_at
             FROM media_references
             WHERE encrypted_file_hash = ?
             ORDER BY created_at ASC, id ASC
             LIMIT 1",
        )
        .bind(&hash_hex)
        .fetch_optional(account_pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        let ref_row = match ref_row {
            Some(r) => Self::ref_from_row(&r).map_err(DatabaseError::Sqlx)?,
            None => return Ok(None),
        };

        let blob = match Self::fetch_blob(&shared_db.pool, &hash_hex).await? {
            Some(b) => b,
            None => return Ok(None),
        };

        Ok(Some(Self::assemble(ref_row, blob, *account_pubkey)?))
    }

    /// Saves a cached media file to the database.
    ///
    /// Upserts the blob on the **shared** pool and inserts the reference on the
    /// **account** pool. Returns the assembled `MediaFile`.
    ///
    /// # Arguments
    /// * `account_pool` - Per-account database pool (holds `media_references`)
    /// * `shared_db` - Shared database (holds `media_blobs`)
    /// * `mls_group_id` - The MLS group ID
    /// * `account_pubkey` - The account public key accessing this media
    /// * `params` - Media file parameters (path, hashes, mime type, etc.)
    #[perf_instrument("db::media_files")]
    pub(crate) async fn save(
        account_pool: &SqlitePool,
        shared_db: &Database,
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

        let file_metadata_json = params
            .file_metadata
            .map(FileMetadata::sanitized_for_storage)
            .filter(|m| !m.is_empty())
            .map(Json);

        // Upsert blob on shared DB.
        sqlx::query(
            "INSERT INTO media_blobs
                (encrypted_file_hash, file_path, mime_type, blossom_url, created_at)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT (encrypted_file_hash) DO UPDATE SET
                blossom_url = COALESCE(excluded.blossom_url, media_blobs.blossom_url),
                file_path = CASE
                    WHEN excluded.file_path != '' THEN excluded.file_path
                    ELSE media_blobs.file_path
                END",
        )
        .bind(&encrypted_file_hash_hex)
        .bind(file_path_str)
        .bind(params.mime_type)
        .bind(params.blossom_url)
        .bind(now_ms)
        .execute(&shared_db.pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        // Upsert reference on account DB (no account_pubkey column).
        sqlx::query(
            "INSERT INTO media_references (
                mls_group_id, encrypted_file_hash,
                media_type, nostr_key, file_metadata,
                original_file_hash, nonce, scheme_version, created_at
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (mls_group_id, encrypted_file_hash)
            DO NOTHING",
        )
        .bind(mls_group_id.as_slice())
        .bind(&encrypted_file_hash_hex)
        .bind(params.media_type)
        .bind(params.nostr_key)
        .bind(file_metadata_json)
        .bind(original_file_hash_hex.as_ref())
        .bind(params.nonce)
        .bind(params.scheme_version)
        .bind(now_ms)
        .execute(account_pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        // Read back the reference from the account DB.
        let ref_row = sqlx::query(
            "SELECT id, mls_group_id, encrypted_file_hash, original_file_hash,
                    media_type, nostr_key, file_metadata, nonce, scheme_version, created_at
             FROM media_references
             WHERE mls_group_id = ? AND encrypted_file_hash = ?
             LIMIT 1",
        )
        .bind(mls_group_id.as_slice())
        .bind(&encrypted_file_hash_hex)
        .fetch_one(account_pool)
        .await
        .map_err(DatabaseError::Sqlx)?;
        let ref_row = Self::ref_from_row(&ref_row).map_err(DatabaseError::Sqlx)?;

        // Read back the blob from the shared DB.
        let blob = Self::fetch_blob(&shared_db.pool, &encrypted_file_hash_hex)
            .await?
            .ok_or_else(|| {
                WhitenoiseError::MediaCache(format!(
                    "missing media_blobs row for hash {encrypted_file_hash_hex}"
                ))
            })?;

        Self::assemble(ref_row, blob, *account_pubkey)
    }

    /// Finds all media files for a specific MLS group.
    ///
    /// Reads references from the **account** pool and blobs from the **shared**
    /// pool, assembling the results in Rust.
    #[perf_instrument("db::media_files")]
    pub(crate) async fn find_by_group(
        account_pool: &SqlitePool,
        shared_db: &Database,
        account_pubkey: &PublicKey,
        group_id: &GroupId,
    ) -> Result<Vec<Self>, WhitenoiseError> {
        let ref_rows = sqlx::query(
            "SELECT id, mls_group_id, encrypted_file_hash, original_file_hash,
                    media_type, nostr_key, file_metadata, nonce, scheme_version, created_at
             FROM media_references
             WHERE mls_group_id = ?",
        )
        .bind(group_id.as_slice())
        .fetch_all(account_pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        let mut refs: Vec<ReferenceRow> = Vec::with_capacity(ref_rows.len());
        for row in &ref_rows {
            refs.push(Self::ref_from_row(row).map_err(DatabaseError::Sqlx)?);
        }

        // Batch-fetch all blobs in one query instead of N round-trips.
        let hashes: Vec<&str> = refs
            .iter()
            .map(|r| r.encrypted_file_hash.as_str())
            .collect();
        let blob_map = Self::fetch_blobs_batch(&shared_db.pool, &hashes).await?;

        let mut result = Vec::with_capacity(refs.len());
        for r in refs {
            let blob = match blob_map.get(&r.encrypted_file_hash) {
                Some(b) => b.clone(),
                None => {
                    tracing::warn!(
                        target: "whitenoise::database::media_files",
                        encrypted_file_hash = %r.encrypted_file_hash,
                        "media_blobs row missing for referenced hash; skipping entry",
                    );
                    continue;
                }
            };
            result.push(Self::assemble(r, blob, *account_pubkey)?);
        }

        Ok(result)
    }

    /// Finds a media file by original hash and group ID (MIP-04 compliant lookup).
    ///
    /// The account scope is implicit in the per-account database file.
    ///
    /// # Arguments
    /// * `account_pool` - Per-account database pool (holds `media_references`)
    /// * `shared_db` - Shared database (holds `media_blobs`)
    /// * `account_pubkey` - The owning account
    /// * `original_file_hash` - The SHA-256 hash of the decrypted file content
    /// * `group_id` - The MLS group ID to scope the search to
    #[perf_instrument("db::media_files")]
    pub(crate) async fn find_by_original_hash_and_group(
        account_pool: &SqlitePool,
        shared_db: &Database,
        account_pubkey: &PublicKey,
        original_file_hash: &[u8; 32],
        group_id: &GroupId,
    ) -> Result<Option<Self>, WhitenoiseError> {
        let hash_hex = hex::encode(original_file_hash);

        let ref_row = sqlx::query(
            "SELECT id, mls_group_id, encrypted_file_hash, original_file_hash,
                    media_type, nostr_key, file_metadata, nonce, scheme_version, created_at
             FROM media_references
             WHERE original_file_hash = ? AND mls_group_id = ?
             LIMIT 1",
        )
        .bind(&hash_hex)
        .bind(group_id.as_slice())
        .fetch_optional(account_pool)
        .await
        .map_err(DatabaseError::Sqlx)?;

        let ref_row = match ref_row {
            Some(r) => Self::ref_from_row(&r).map_err(DatabaseError::Sqlx)?,
            None => return Ok(None),
        };

        let blob = match Self::fetch_blob(&shared_db.pool, &ref_row.encrypted_file_hash).await? {
            Some(b) => b,
            None => return Ok(None),
        };

        Ok(Some(Self::assemble(ref_row, blob, *account_pubkey)?))
    }

    /// Updates the file_path for an existing media file record.
    ///
    /// Reads the `encrypted_file_hash` from the **account** pool's reference
    /// row, then updates the **shared** blob's `file_path`.
    ///
    /// NOTE: This mutates the shared `media_blobs` row, which affects every
    /// account referencing the same `encrypted_file_hash`.
    #[perf_instrument("db::media_files")]
    pub(crate) async fn update_file_path(
        account_pool: &SqlitePool,
        shared_db: &Database,
        account_pubkey: &PublicKey,
        id: i64,
        new_path: &Path,
    ) -> Result<Self, WhitenoiseError> {
        let path_str = new_path
            .to_str()
            .ok_or_else(|| WhitenoiseError::MediaCache("Invalid file path".to_string()))?;

        // Look up the encrypted_file_hash from the account-DB reference row.
        let hash: String =
            sqlx::query_scalar("SELECT encrypted_file_hash FROM media_references WHERE id = ?")
                .bind(id)
                .fetch_optional(account_pool)
                .await
                .map_err(DatabaseError::Sqlx)?
                .ok_or_else(|| {
                    WhitenoiseError::MediaCache(format!("MediaFile with id {} not found", id))
                })?;

        // Update the shared blob's file_path.
        sqlx::query("UPDATE media_blobs SET file_path = ? WHERE encrypted_file_hash = ?")
            .bind(path_str)
            .bind(&hash)
            .execute(&shared_db.pool)
            .await
            .map_err(DatabaseError::Sqlx)?;

        // Read back the reference.
        let ref_row = sqlx::query(
            "SELECT id, mls_group_id, encrypted_file_hash, original_file_hash,
                    media_type, nostr_key, file_metadata, nonce, scheme_version, created_at
             FROM media_references
             WHERE id = ?",
        )
        .bind(id)
        .fetch_one(account_pool)
        .await
        .map_err(DatabaseError::Sqlx)?;
        let ref_row = Self::ref_from_row(&ref_row).map_err(DatabaseError::Sqlx)?;

        let blob = Self::fetch_blob(&shared_db.pool, &hash)
            .await?
            .ok_or_else(|| {
                WhitenoiseError::MediaCache(format!("missing media_blobs row for hash {hash}"))
            })?;

        Self::assemble(ref_row, blob, *account_pubkey)
    }

    /// Returns all distinct non-empty file paths referenced by media_blobs records.
    ///
    /// Reads from the **shared** database only (blobs are shared across accounts).
    #[perf_instrument("db::media_files")]
    pub(crate) async fn all_referenced_file_paths(
        shared_db: &Database,
    ) -> Result<Vec<String>, WhitenoiseError> {
        let paths: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT file_path FROM media_blobs WHERE file_path != ''")
                .fetch_all(&shared_db.pool)
                .await
                .map_err(DatabaseError::Sqlx)?;
        Ok(paths)
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

    /// Deletes all per-account `media_references` rows for a given group.
    pub(crate) async fn delete_references_for_group(
        account_pool: &SqlitePool,
        group_id: &GroupId,
    ) -> Result<(), WhitenoiseError> {
        sqlx::query("DELETE FROM media_references WHERE mls_group_id = ?")
            .bind(group_id.as_slice())
            .execute(account_pool)
            .await
            .map_err(DatabaseError::Sqlx)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::database::account_db::AccountDatabase;
    use tempfile::TempDir;

    /// Test fixture: shared DB (media_blobs) + account DB (media_references).
    struct TestDbs {
        shared: Database,
        account_pool: SqlitePool,
        account_pubkey: PublicKey,
        _dir: TempDir,
    }

    async fn setup_test_dbs() -> TestDbs {
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

        // Create user+account in shared DB for FK constraints
        create_test_account(&shared, &pubkey).await;

        TestDbs {
            shared,
            account_pool: account_db.inner.pool,
            account_pubkey: pubkey,
            _dir: dir,
        }
    }

    /// Set up a second account against the same shared DB.
    async fn add_account(shared: &Database, dir: &TempDir, pubkey: &PublicKey) -> SqlitePool {
        let account_db =
            AccountDatabase::new(*pubkey, dir.path().join(format!("{}.db", pubkey.to_hex())))
                .await
                .unwrap();
        account_db
            .run_account_migrations(&shared.pool)
            .await
            .unwrap();
        create_test_account(shared, pubkey).await;
        account_db.inner.pool
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
        let t = setup_test_dbs().await;
        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let encrypted_file_hash = [3u8; 32];
        let file_path = t._dir.path().join("test.jpg");

        let media_file = MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id,
            &t.account_pubkey,
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

        assert!(media_file.id.is_some());
        assert!(media_file.id.unwrap() > 0);
        assert_eq!(media_file.encrypted_file_hash, encrypted_file_hash.to_vec());
        assert_eq!(media_file.original_file_hash, None);
        assert_eq!(media_file.mime_type, "image/jpeg");
        assert_eq!(media_file.media_type, "group_image");
        assert_eq!(media_file.mls_group_id, group_id);
        assert_eq!(media_file.account_pubkey, t.account_pubkey);
    }

    #[tokio::test]
    async fn test_upsert_on_conflict() {
        let t = setup_test_dbs().await;

        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let encrypted_file_hash = [3u8; 32];
        let file_path = t._dir.path().join("test.jpg");

        // Save media first time
        let first_save = MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id,
            &t.account_pubkey,
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
            &t.account_pool,
            &t.shared,
            &group_id,
            &t.account_pubkey,
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
        // The blob upsert uses COALESCE — a non-null blossom_url in the
        // second save replaces the existing one.
        assert_eq!(
            second_save.blossom_url,
            Some("https://example.com/blob2".to_string())
        );
    }

    #[tokio::test]
    async fn test_find_by_hash_returns_first_match() {
        let t = setup_test_dbs().await;
        let pubkey2 = PublicKey::from_slice(&[20u8; 32]).unwrap();
        let pool2 = add_account(&t.shared, &t._dir, &pubkey2).await;

        // Test with two accounts having the same encrypted hash in different groups
        let group_id1 = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let group_id2 = mdk_core::GroupId::from_slice(&[2u8; 8]);
        let encrypted_file_hash = [42u8; 32];
        let file_path1 = t._dir.path().join("test1.jpg");
        let file_path2 = t._dir.path().join("test2.jpg");

        // Create metadata for first record
        let metadata = FileMetadata::new()
            .with_filename("original.jpg".to_string())
            .with_dimensions("1920x1080".to_string())
            .with_blurhash("LEHV6nWB2yk8pyo0adR*.7kCMdnj".to_string());

        // Save first record (account 1) with metadata
        let first_save = MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id1,
            &t.account_pubkey,
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

        // Save second record (account 2) with same encrypted hash but different group
        MediaFile::save(
            &pool2,
            &t.shared,
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

        // find_by_hash for account 1 returns account 1's record
        let found = MediaFile::find_by_hash(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &encrypted_file_hash,
        )
        .await
        .unwrap();

        assert!(found.is_some());
        let media_file = found.unwrap();

        assert_eq!(media_file.id, first_save.id);
        assert_eq!(media_file.encrypted_file_hash, encrypted_file_hash.to_vec());
        assert_eq!(media_file.mls_group_id, group_id1);
        assert_eq!(media_file.account_pubkey, t.account_pubkey);
        assert_eq!(media_file.mime_type, "image/jpeg");
        assert_eq!(media_file.media_type, "group_image");
        // Account 2's save upserted the shared blob with its URL, so the
        // latest non-null blossom_url wins (COALESCE in the upsert).
        assert_eq!(
            media_file.blossom_url,
            Some("https://another-server.com/hash42".to_string())
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
        let t = setup_test_dbs().await;

        let nonexistent_hash = [99u8; 32];

        // Try to find a hash that doesn't exist
        let found = MediaFile::find_by_hash(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &nonexistent_hash,
        )
        .await
        .unwrap();

        assert!(found.is_none());
    }

    #[tokio::test]
    async fn test_find_by_group_empty_result() {
        let t = setup_test_dbs().await;

        let nonexistent_group_id = mdk_core::GroupId::from_slice(&[99u8; 8]);

        // Try to find media for a group that doesn't exist
        let media_files = MediaFile::find_by_group(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &nonexistent_group_id,
        )
        .await
        .unwrap();

        assert!(media_files.is_empty());
    }

    #[tokio::test]
    async fn test_find_by_group_multiple_files_and_group_isolation() {
        let t = setup_test_dbs().await;
        let pubkey2 = PublicKey::from_slice(&[20u8; 32]).unwrap();
        let pool2 = add_account(&t.shared, &t._dir, &pubkey2).await;

        let group_id1 = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let group_id2 = mdk_core::GroupId::from_slice(&[2u8; 8]);

        // Create metadata for one file
        let metadata = FileMetadata::new()
            .with_filename("image1.jpg".to_string())
            .with_dimensions("1920x1080".to_string());

        // Save one media file for group 1 under account 1
        let original_hash1a = [11u8; 32];
        let encrypted_hash1a = [111u8; 32];
        let file_path1a = t._dir.path().join("group1_file1.jpg");

        MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id1,
            &t.account_pubkey,
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

        // Save one media file for group 1 under account 2
        let original_hash1b = [12u8; 32];
        let encrypted_hash1b = [121u8; 32];
        let file_path1b = t._dir.path().join("group1_file2.png");

        MediaFile::save(
            &pool2,
            &t.shared,
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

        // Save one media file for group 2 under account 1
        let original_hash2 = [22u8; 32];
        let encrypted_hash2 = [222u8; 32];
        let file_path2 = t._dir.path().join("group2_file.jpg");

        MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id2,
            &t.account_pubkey,
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

        // Each account's find_by_group only returns that account's references.
        // Account 1 sees: group1 file1 and group2 file
        let acct1_group1 =
            MediaFile::find_by_group(&t.account_pool, &t.shared, &t.account_pubkey, &group_id1)
                .await
                .unwrap();
        assert_eq!(acct1_group1.len(), 1);
        assert_eq!(
            acct1_group1[0].encrypted_file_hash,
            encrypted_hash1a.to_vec()
        );
        assert_eq!(acct1_group1[0].mls_group_id, group_id1);
        assert!(acct1_group1[0].original_file_hash.is_some());

        // Verify metadata is preserved for account 1's file
        assert!(acct1_group1[0].file_metadata.is_some());
        assert_eq!(
            acct1_group1[0]
                .file_metadata
                .as_ref()
                .unwrap()
                .original_filename,
            Some("image1.jpg".to_string())
        );

        // Account 2 sees: group1 file2 only
        let acct2_group1 = MediaFile::find_by_group(&pool2, &t.shared, &pubkey2, &group_id1)
            .await
            .unwrap();
        assert_eq!(acct2_group1.len(), 1);
        assert_eq!(
            acct2_group1[0].encrypted_file_hash,
            encrypted_hash1b.to_vec()
        );
        assert_eq!(acct2_group1[0].mls_group_id, group_id1);
        assert!(acct2_group1[0].original_file_hash.is_some());

        // Account 1 sees group 2 file
        let acct1_group2 =
            MediaFile::find_by_group(&t.account_pool, &t.shared, &t.account_pubkey, &group_id2)
                .await
                .unwrap();
        assert_eq!(acct1_group2.len(), 1);
        assert_eq!(
            acct1_group2[0].encrypted_file_hash,
            encrypted_hash2.to_vec()
        );
        assert_eq!(acct1_group2[0].mls_group_id, group_id2);
        assert!(acct1_group2[0].original_file_hash.is_some());

        // Account 2 sees nothing in group 2
        let acct2_group2 = MediaFile::find_by_group(&pool2, &t.shared, &pubkey2, &group_id2)
            .await
            .unwrap();
        assert!(acct2_group2.is_empty());

        // Cross-group isolation: account 1's group 1 results don't include group 2 hashes
        let encrypted_hashes_acct1_group1: Vec<Vec<u8>> = acct1_group1
            .iter()
            .map(|mf| mf.encrypted_file_hash.clone())
            .collect();
        assert!(!encrypted_hashes_acct1_group1.contains(&encrypted_hash2.to_vec()));
        assert!(!encrypted_hashes_acct1_group1.contains(&encrypted_hash1b.to_vec()));
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
        let t = setup_test_dbs().await;

        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let original_hash = [11u8; 32];
        let encrypted_hash = [111u8; 32];
        let initial_path = t._dir.path().join("initial.jpg");
        let new_path = t._dir.path().join("updated.jpg");

        // Create metadata to verify it's preserved
        let metadata = FileMetadata::new()
            .with_filename("test.jpg".to_string())
            .with_dimensions("800x600".to_string());

        // Create initial media file record
        let media_file = MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id,
            &t.account_pubkey,
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
        let updated = MediaFile::update_file_path(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            original_id,
            &new_path,
        )
        .await
        .unwrap();

        // Verify path was updated
        assert_eq!(updated.file_path, new_path);

        // Verify ID is preserved
        assert_eq!(updated.id, Some(original_id));

        // Verify all other fields remain unchanged
        assert_eq!(updated.mls_group_id, group_id);
        assert_eq!(updated.account_pubkey, t.account_pubkey);
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
        let fetched = MediaFile::find_by_original_hash_and_group(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &original_hash,
            &group_id,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(fetched.file_path, new_path);
        assert_eq!(fetched.id, Some(original_id));
    }

    #[tokio::test]
    async fn test_update_file_path_not_found() {
        let t = setup_test_dbs().await;

        let nonexistent_id = 99999;
        let new_path = t._dir.path().join("new.jpg");

        // Try to update a nonexistent record
        let result = MediaFile::update_file_path(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            nonexistent_id,
            &new_path,
        )
        .await;

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
        let t = setup_test_dbs().await;

        let group_id1 = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let group_id2 = mdk_core::GroupId::from_slice(&[2u8; 8]);
        let original_hash1 = [11u8; 32];
        let original_hash2 = [12u8; 32];
        let encrypted_hash1 = [111u8; 32];
        let encrypted_hash2 = [121u8; 32];
        let file_path1 = t._dir.path().join("chat_media1.jpg");
        let file_path2 = t._dir.path().join("chat_media2.png");

        // Create metadata for first file
        let metadata = FileMetadata::new()
            .with_filename("test_image.jpg".to_string())
            .with_dimensions("1920x1080".to_string());

        // Save first chat media file in group 1 with original hash
        let saved_file1 = MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id1,
            &t.account_pubkey,
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
            &t.account_pool,
            &t.shared,
            &group_id2,
            &t.account_pubkey,
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
        let file_path_group_image = t._dir.path().join("group_image.jpg");
        MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id1,
            &t.account_pubkey,
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
        let found = MediaFile::find_by_original_hash_and_group(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &original_hash1,
            &group_id1,
        )
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
        let not_found = MediaFile::find_by_original_hash_and_group(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &original_hash1,
            &group_id2,
        )
        .await
        .unwrap();

        assert!(
            not_found.is_none(),
            "Should not find file with correct hash and account but wrong group"
        );

        // Test 3: Should not find file with wrong hash but correct group and account
        let wrong_hash = [255u8; 32];
        let not_found = MediaFile::find_by_original_hash_and_group(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &wrong_hash,
            &group_id1,
        )
        .await
        .unwrap();

        assert!(not_found.is_none(), "Should not find file with wrong hash");

        // Test 4: Find second file in different group with same account
        let found = MediaFile::find_by_original_hash_and_group(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &original_hash2,
            &group_id2,
        )
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
        // The group image has no original_file_hash, so it should not be found by a random hash
        let nonexistent_hash = [100u8; 32];
        let not_found = MediaFile::find_by_original_hash_and_group(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &nonexistent_hash,
            &group_id1,
        )
        .await
        .unwrap();

        assert!(
            not_found.is_none(),
            "Should not find group image when searching by original_file_hash"
        );
    }

    #[tokio::test]
    async fn test_find_by_original_hash_and_group_multi_account() {
        let t = setup_test_dbs().await;
        let account2_pubkey = PublicKey::from_slice(&[20u8; 32]).unwrap();
        let pool2 = add_account(&t.shared, &t._dir, &account2_pubkey).await;

        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let original_hash = [11u8; 32]; // Same media file
        let encrypted_hash = [111u8; 32]; // Same encrypted hash
        let file_path1 = t._dir.path().join("account1_media.jpg");
        let file_path2 = t._dir.path().join("account2_media.jpg");

        // Account 1 saves media file (e.g., after uploading)
        let saved_file1 = MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id,
            &t.account_pubkey,
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
            &pool2,
            &t.shared,
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
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &original_hash,
            &group_id,
        )
        .await
        .unwrap()
        .expect("Should find account1's record");

        assert_eq!(found1.id, saved_file1.id);
        assert_eq!(found1.account_pubkey, t.account_pubkey);
        // file_path comes from the shared blob; account 2's save overwrote it
        // (last non-empty writer wins in the blob upsert).
        assert_eq!(found1.file_path, file_path2);

        // Verify that querying with account2 returns account2's record (not account1's!)
        let found2 = MediaFile::find_by_original_hash_and_group(
            &pool2,
            &t.shared,
            &account2_pubkey,
            &original_hash,
            &group_id,
        )
        .await
        .unwrap()
        .expect("Should find account2's record");

        assert_eq!(found2.id, saved_file2.id);
        assert_eq!(found2.account_pubkey, account2_pubkey);
        assert_eq!(found2.file_path, file_path2);

        // Verify the two references belong to different accounts but share the
        // same blob (IDs can collide since they're in separate per-account DBs).
        assert_ne!(
            found1.account_pubkey, found2.account_pubkey,
            "Different accounts should have different account_pubkey"
        );
        assert_eq!(
            found1.file_path, found2.file_path,
            "Both accounts share the same blob, so file_path should match"
        );

        // Verify a third account (with its own pool) cannot find records for the other accounts
        let account3_pubkey = PublicKey::from_slice(&[30u8; 32]).unwrap();
        let pool3 = add_account(&t.shared, &t._dir, &account3_pubkey).await;
        let not_found = MediaFile::find_by_original_hash_and_group(
            &pool3,
            &t.shared,
            &account3_pubkey,
            &original_hash,
            &group_id,
        )
        .await
        .unwrap();

        assert!(
            not_found.is_none(),
            "Account 3 should not find records belonging to other accounts"
        );
    }

    #[tokio::test]
    async fn test_file_metadata_with_thumbhash() {
        let t = setup_test_dbs().await;

        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let encrypted_hash = [42u8; 32];
        let file_path = t._dir.path().join("test.jpg");

        // Save with both blurhash and thumbhash
        let metadata = FileMetadata::new()
            .with_filename("photo.jpg".to_string())
            .with_dimensions("800x600".to_string())
            .with_blurhash("LEHV6nWB2yk8pyo0adR*.7kCMdnj".to_string())
            .with_thumbhash("3OcRJYB4d3h/iIeHeEh3eIhw+j2w".to_string());

        MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id,
            &t.account_pubkey,
            MediaFileParams {
                file_path: &file_path,
                original_file_hash: None,
                encrypted_file_hash: &encrypted_hash,
                mime_type: "image/jpeg",
                media_type: "chat_media",
                blossom_url: Some("https://blossom.example.com/hash42"),
                nostr_key: None,
                file_metadata: Some(&metadata),
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Retrieve and verify both hashes are persisted
        let found = MediaFile::find_by_hash(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &encrypted_hash,
        )
        .await
        .unwrap()
        .expect("Should find the saved media file");

        let retrieved = found.file_metadata.expect("Should have file_metadata");
        assert_eq!(
            retrieved.blurhash,
            Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj".to_string())
        );
        assert_eq!(
            retrieved.thumbhash,
            Some("3OcRJYB4d3h/iIeHeEh3eIhw+j2w".to_string())
        );
        assert_eq!(retrieved.original_filename, Some("photo.jpg".to_string()));
        assert_eq!(retrieved.dimensions, Some("800x600".to_string()));
    }

    #[test]
    fn test_file_metadata_audio_fields_serde_roundtrip() {
        let metadata = FileMetadata::new()
            .with_filename("voice.mp3".to_string())
            .with_duration_ms(12_345)
            .with_waveform(vec![0, 8, 42, 100]);

        let json = serde_json::to_string(&metadata).unwrap();
        assert!(json.contains("\"duration_ms\":12345"));
        assert!(json.contains("\"waveform\":[0,8,42,100]"));

        let decoded: FileMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.duration_ms, Some(12_345));
        assert_eq!(decoded.waveform, Some(vec![0, 8, 42, 100]));
        assert_eq!(decoded.original_filename, Some("voice.mp3".to_string()));
    }

    #[test]
    fn test_file_metadata_skips_absent_audio_fields() {
        let metadata = FileMetadata::new().with_filename("photo.jpg".to_string());

        let json = serde_json::to_string(&metadata).unwrap();

        assert!(!json.contains("duration_ms"));
        assert!(!json.contains("waveform"));
    }

    #[test]
    fn test_file_metadata_invalid_waveform_is_omitted() {
        let metadata = FileMetadata::new()
            .with_filename("voice.mp3".to_string())
            .with_waveform(vec![0, 101]);

        assert!(metadata.waveform.is_none());

        let oversized = FileMetadata::new().with_waveform(vec![50; MAX_WAVEFORM_SAMPLES + 1]);
        assert!(oversized.waveform.is_none());
    }

    #[tokio::test]
    async fn test_shared_blobs_survive_reference_deletion() {
        let t = setup_test_dbs().await;
        let account2 = PublicKey::from_slice(&[20u8; 32]).unwrap();
        let pool2 = add_account(&t.shared, &t._dir, &account2).await;

        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let encrypted_hash = [42u8; 32];
        let file_path = t._dir.path().join("shared.jpg");

        // Account 1 saves a reference to the blob.
        MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id,
            &t.account_pubkey,
            MediaFileParams {
                file_path: &file_path,
                original_file_hash: None,
                encrypted_file_hash: &encrypted_hash,
                mime_type: "image/jpeg",
                media_type: "chat_media",
                blossom_url: None,
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Account 2 saves a reference to the same blob.
        MediaFile::save(
            &pool2,
            &t.shared,
            &group_id,
            &account2,
            MediaFileParams {
                file_path: &file_path,
                original_file_hash: None,
                encrypted_file_hash: &encrypted_hash,
                mime_type: "image/jpeg",
                media_type: "chat_media",
                blossom_url: None,
                nostr_key: None,
                file_metadata: None,
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Delete account1's reference from its own per-account pool.
        sqlx::query("DELETE FROM media_references WHERE mls_group_id = ?")
            .bind(group_id.as_slice())
            .execute(&t.account_pool)
            .await
            .unwrap();

        // Blob must still exist in the shared DB.
        let blob_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM media_blobs WHERE encrypted_file_hash = ?")
                .bind(hex::encode(encrypted_hash))
                .fetch_one(&t.shared.pool)
                .await
                .unwrap();
        assert_eq!(
            blob_count.0, 1,
            "Shared blob must survive reference deletion"
        );

        // Account 2 can still find the media via its own pool.
        let found = MediaFile::find_by_hash(&pool2, &t.shared, &account2, &encrypted_hash)
            .await
            .unwrap();
        assert!(found.is_some(), "Account2 should still see the media file");
    }

    #[tokio::test]
    async fn test_file_metadata_without_thumbhash_backwards_compat() {
        let t = setup_test_dbs().await;

        let group_id = mdk_core::GroupId::from_slice(&[1u8; 8]);
        let encrypted_hash = [42u8; 32];
        let file_path = t._dir.path().join("test.jpg");

        // Save with only blurhash (simulating old client behavior)
        let metadata = FileMetadata::new()
            .with_filename("old_photo.jpg".to_string())
            .with_blurhash("LEHV6nWB2yk8pyo0adR*.7kCMdnj".to_string());

        MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id,
            &t.account_pubkey,
            MediaFileParams {
                file_path: &file_path,
                original_file_hash: None,
                encrypted_file_hash: &encrypted_hash,
                mime_type: "image/jpeg",
                media_type: "chat_media",
                blossom_url: Some("https://blossom.example.com/hash42"),
                nostr_key: None,
                file_metadata: Some(&metadata),
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Retrieve and verify thumbhash is None while blurhash is present
        let found = MediaFile::find_by_hash(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &encrypted_hash,
        )
        .await
        .unwrap()
        .expect("Should find the saved media file");

        let retrieved = found.file_metadata.expect("Should have file_metadata");
        assert_eq!(
            retrieved.blurhash,
            Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj".to_string())
        );
        assert!(
            retrieved.thumbhash.is_none(),
            "Old records without thumbhash should deserialize with None"
        );
    }

    /// Regression: a non-null `file_metadata` written through the production
    /// path must round-trip cleanly back into `MediaFile.file_metadata`.
    ///
    /// This exercises the read side of `media_references.file_metadata`,
    /// which is declared `BLOB` in the schema. The internal `ReferenceRow`
    /// must accept whatever SQLite stores there (TEXT or BLOB) without a
    /// sqlx column-decode error.
    #[tokio::test]
    async fn test_file_metadata_blob_column_roundtrip() {
        let t = setup_test_dbs().await;

        let group_id = mdk_core::GroupId::from_slice(&[7u8; 8]);
        let encrypted_hash = [77u8; 32];
        let file_path = t._dir.path().join("blob_roundtrip.jpg");

        let metadata = FileMetadata::new()
            .with_filename("photo.jpg".to_string())
            .with_dimensions("4032x3024".to_string())
            .with_blurhash("L9AB*A%MfQ%M~qfQfQfQfQfQfQfQ".to_string())
            .with_thumbhash("3OcRJYB4d3h_iIeHeYh3eIhw+j3A".to_string());

        MediaFile::save(
            &t.account_pool,
            &t.shared,
            &group_id,
            &t.account_pubkey,
            MediaFileParams {
                file_path: &file_path,
                original_file_hash: None,
                encrypted_file_hash: &encrypted_hash,
                mime_type: "image/jpeg",
                media_type: "chat_media",
                blossom_url: Some("https://blossom.example.com/77"),
                nostr_key: None,
                file_metadata: Some(&metadata),
                nonce: None,
                scheme_version: None,
            },
        )
        .await
        .unwrap();

        // Simulate the on-disk state reported in production: a row whose
        // `file_metadata` cell carries SQLite's BLOB storage class, not TEXT.
        // sqlx's `Json<T>` for SQLite encodes as TEXT, so the production
        // BLOB-typed cells must have come from a path that wrote raw bytes —
        // overwrite directly via `CAST(... AS BLOB)` to reproduce that state.
        let metadata_bytes = serde_json::to_vec(&metadata).unwrap();
        sqlx::query("UPDATE media_references SET file_metadata = CAST(? AS BLOB) WHERE encrypted_file_hash = ?")
            .bind(&metadata_bytes)
            .bind(hex::encode(encrypted_hash))
            .execute(&t.account_pool)
            .await
            .unwrap();

        // Sanity-check: the stored value's runtime storage class is now BLOB.
        let storage_class: String = sqlx::query_scalar(
            "SELECT typeof(file_metadata) FROM media_references WHERE encrypted_file_hash = ?",
        )
        .bind(hex::encode(encrypted_hash))
        .fetch_one(&t.account_pool)
        .await
        .unwrap();
        assert_eq!(
            storage_class, "blob",
            "test setup must reproduce the production storage class"
        );

        // Read back via the production path. Before the fix this fails with:
        //   mismatched types; Rust type `core::option::Option<alloc::string::String>`
        //   (as SQL type `TEXT`) is not compatible with SQL type `BLOB`
        let found = MediaFile::find_by_hash(
            &t.account_pool,
            &t.shared,
            &t.account_pubkey,
            &encrypted_hash,
        )
        .await
        .expect("decoding a BLOB-typed file_metadata cell must not error")
        .expect("row must still be present");

        let retrieved = found
            .file_metadata
            .expect("file_metadata must round-trip from BLOB storage");
        assert_eq!(retrieved.original_filename, Some("photo.jpg".to_string()));
        assert_eq!(retrieved.dimensions, Some("4032x3024".to_string()));
        assert_eq!(
            retrieved.blurhash,
            Some("L9AB*A%MfQ%M~qfQfQfQfQfQfQfQ".to_string())
        );
        assert_eq!(
            retrieved.thumbhash,
            Some("3OcRJYB4d3h_iIeHeYh3eIhw+j3A".to_string())
        );
    }
}
