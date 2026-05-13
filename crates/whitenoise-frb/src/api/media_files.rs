use crate::api::wn_session;
use crate::api::{error::ApiError, group_id_from_string, group_id_to_string};
use chrono::{DateTime, Utc};
use flutter_rust_bridge::frb;
use nostr_sdk::prelude::*;
use std::path::PathBuf;
use whitenoise::{FileMetadata as WhitenoiseFileMetadata, MediaFile as WhitenoiseMediaFile};

#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub original_filename: Option<String>,
    pub dimensions: Option<String>,
    pub blurhash: Option<String>,
    pub thumbhash: Option<String>,
}

impl From<WhitenoiseFileMetadata> for FileMetadata {
    fn from(metadata: WhitenoiseFileMetadata) -> Self {
        Self {
            original_filename: metadata.original_filename,
            dimensions: metadata.dimensions,
            blurhash: metadata.blurhash,
            thumbhash: metadata.thumbhash,
        }
    }
}

impl From<FileMetadata> for WhitenoiseFileMetadata {
    fn from(metadata: FileMetadata) -> Self {
        Self {
            original_filename: metadata.original_filename,
            dimensions: metadata.dimensions,
            blurhash: metadata.blurhash,
            thumbhash: metadata.thumbhash,
        }
    }
}
#[frb(non_opaque)]
#[derive(Debug, Clone)]
pub struct MediaFile {
    pub id: String,
    pub mls_group_id: String,
    pub account_pubkey: String,
    pub file_path: String,
    pub original_file_hash: Option<String>,
    pub encrypted_file_hash: String,
    pub mime_type: String,
    pub media_type: String,
    pub blossom_url: String,
    pub nostr_key: String,
    pub file_metadata: Option<FileMetadata>,
    pub created_at: DateTime<Utc>,
    pub nonce: Option<String>,
    pub scheme_version: Option<String>,
}

impl From<WhitenoiseMediaFile> for MediaFile {
    fn from(media_file: WhitenoiseMediaFile) -> Self {
        Self {
            id: media_file.id.unwrap_or_default().to_string(),
            account_pubkey: media_file.account_pubkey.to_string(),
            mls_group_id: group_id_to_string(&media_file.mls_group_id),
            file_path: media_file.file_path.to_string_lossy().to_string(),
            original_file_hash: media_file.original_file_hash.map(hex::encode),
            encrypted_file_hash: hex::encode(media_file.encrypted_file_hash),
            mime_type: media_file.mime_type.to_string(),
            media_type: media_file.media_type.to_string(),
            blossom_url: media_file.blossom_url.unwrap_or_default(),
            nostr_key: media_file.nostr_key.unwrap_or_default(),
            file_metadata: media_file.file_metadata.map(|metadata| metadata.into()),
            created_at: media_file.created_at,
            nonce: media_file.nonce,
            scheme_version: media_file.scheme_version,
        }
    }
}

impl TryFrom<MediaFile> for WhitenoiseMediaFile {
    type Error = ApiError;

    fn try_from(mf: MediaFile) -> Result<Self, Self::Error> {
        let id = mf.id.parse::<i64>().ok().filter(|&v| v > 0);
        let original_file_hash = mf
            .original_file_hash
            .map(|h| hex::decode(&h).map_err(ApiError::from))
            .transpose()?;

        Ok(Self {
            id,
            account_pubkey: PublicKey::parse(&mf.account_pubkey)?,
            mls_group_id: group_id_from_string(&mf.mls_group_id)?,
            file_path: PathBuf::from(mf.file_path),
            original_file_hash,
            encrypted_file_hash: hex::decode(&mf.encrypted_file_hash)?,
            mime_type: mf.mime_type,
            media_type: mf.media_type,
            // Not exposed in the bridge type; the canonical values live
            // in the media_files table and are unaffected by this snapshot.
            nonce: None,
            scheme_version: None,
            blossom_url: if mf.blossom_url.is_empty() {
                None
            } else {
                Some(mf.blossom_url)
            },
            nostr_key: if mf.nostr_key.is_empty() {
                None
            } else {
                Some(mf.nostr_key)
            },
            file_metadata: mf.file_metadata.map(|fm| fm.into()),
            created_at: mf.created_at,
        })
    }
}

#[frb]
pub async fn upload_chat_media(
    account_pubkey: String,
    group_id: String,
    file_path: String,
) -> Result<MediaFile, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;

    let media_file = session
        .groups()
        .media()
        .upload_chat_media(&group_id, &file_path, None, None)
        .await?;

    Ok(media_file.into())
}

#[frb]
pub async fn download_chat_media(
    account_pubkey: String,
    group_id: String,
    original_file_hash: String,
) -> Result<MediaFile, ApiError> {
    let pubkey = PublicKey::parse(&account_pubkey)?;
    let session = wn_session(&pubkey)?;
    let group_id = group_id_from_string(&group_id)?;
    let original_file_hash_bytes = ::hex::decode(&original_file_hash)?;
    let hash_array: [u8; 32] =
        original_file_hash_bytes
            .try_into()
            .map_err(|_| ApiError::NostrHex {
                message: "Invalid original_file_hash length; must be 32 bytes.".to_string(),
            })?;

    let media_file = session
        .groups()
        .media()
        .download_chat_media(&group_id, &hash_array)
        .await?;

    Ok(media_file.into())
}
