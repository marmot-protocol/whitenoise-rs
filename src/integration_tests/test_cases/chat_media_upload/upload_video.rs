use std::io::Write;

use async_trait::async_trait;
use nostr_sdk::Url;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;

/// Test case for uploading video files (MP4)
pub struct UploadVideoTestCase {
    account_name: String,
    group_name: String,
}

impl UploadVideoTestCase {
    pub fn new(account_name: &str, group_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
        }
    }

    /// Create a temporary MP4 video file with valid magic bytes
    fn create_test_video(&self) -> Result<tempfile::NamedTempFile, WhitenoiseError> {
        let mut temp_file = tempfile::NamedTempFile::new()?;

        // MP4 magic bytes (ftyp box signature)
        // This is a minimal valid MP4 file structure
        let mp4_header: &[u8] = &[
            0x00, 0x00, 0x00, 0x20, // box size (32 bytes)
            0x66, 0x74, 0x79, 0x70, // 'ftyp'
            0x69, 0x73, 0x6F, 0x6D, // 'isom' - major brand
            0x00, 0x00, 0x02, 0x00, // minor version
            0x69, 0x73, 0x6F, 0x6D, // 'isom' - compatible brand
            0x69, 0x73, 0x6F, 0x32, // 'iso2' - compatible brand
            0x6D, 0x70, 0x34, 0x31, // 'mp41' - compatible brand
            0x00, 0x00, 0x00, 0x00, // padding
        ];

        temp_file.write_all(mp4_header)?;
        temp_file.flush()?;

        Ok(temp_file)
    }
}

#[async_trait]
impl TestCase for UploadVideoTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Uploading MP4 video for group {} using account: {}",
            self.group_name,
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;

        let temp_file = self.create_test_video()?;
        let temp_path = temp_file
            .path()
            .to_str()
            .ok_or_else(|| WhitenoiseError::Internal("Invalid temp path".to_string()))?;

        // Read the file data and compute expected hash
        let test_video_data = tokio::fs::read(temp_path).await?;
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&test_video_data);
        let expected_original_hash: [u8; 32] = hasher.finalize().into();

        let blossom_url = if cfg!(debug_assertions) {
            Some(Url::parse("http://localhost:3000").unwrap())
        } else {
            None
        };

        let media_file = context
            .whitenoise
            .upload_chat_media(account, &group.mls_group_id, temp_path, blossom_url, None)
            .await?;

        drop(temp_file);

        tracing::info!(
            "✓ Video uploaded successfully: encrypted_hash={}, original_hash={}",
            hex::encode(&media_file.encrypted_file_hash),
            media_file
                .original_file_hash
                .as_ref()
                .map(hex::encode)
                .unwrap_or_else(|| "none".to_string())
        );

        // Validate video upload
        assert!(!media_file.encrypted_file_hash.is_empty());
        assert!(
            media_file.original_file_hash.is_some(),
            "Chat media should have original_file_hash (MIP-04)"
        );

        // Verify original_file_hash matches the SHA-256 of the uploaded file
        assert_eq!(
            media_file.original_file_hash.as_ref().unwrap().as_slice(),
            expected_original_hash,
            "Original file hash should match SHA-256 of uploaded file content"
        );

        assert!(media_file.blossom_url.is_some());
        assert_eq!(media_file.mime_type, "video/mp4");
        assert_eq!(media_file.media_type, "chat_media");
        assert!(media_file.file_path.exists());

        tracing::info!("✓ Video upload validations passed");

        Ok(())
    }
}
