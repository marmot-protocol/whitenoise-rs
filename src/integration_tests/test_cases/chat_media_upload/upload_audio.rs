use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use nostr_sdk::Url;

/// Test case for uploading audio files (MP3)
pub struct UploadAudioTestCase {
    account_name: String,
    group_name: String,
}

impl UploadAudioTestCase {
    pub fn new(account_name: &str, group_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
        }
    }

    /// Create a temporary MP3 audio file with valid magic bytes
    fn create_test_audio(&self) -> Result<tempfile::NamedTempFile, WhitenoiseError> {
        use std::io::Write;

        let mut temp_file = tempfile::NamedTempFile::new()
            .map_err(|e| WhitenoiseError::Internal(format!("Failed to create temp file: {}", e)))?;

        // MP3 magic bytes (ID3v2 header)
        let mp3_header: &[u8] = &[
            0x49, 0x44, 0x33, // 'ID3'
            0x03, 0x00, // version 2.3.0
            0x00, // flags
            0x00, 0x00, 0x00, 0x00, // size (syncsafe integer)
        ];

        temp_file
            .write_all(mp3_header)
            .map_err(|e| WhitenoiseError::Internal(format!("Failed to write MP3 data: {}", e)))?;

        temp_file
            .flush()
            .map_err(|e| WhitenoiseError::Internal(format!("Failed to flush temp file: {}", e)))?;

        Ok(temp_file)
    }
}

#[async_trait]
impl TestCase for UploadAudioTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Uploading MP3 audio for group {} using account: {}",
            self.group_name,
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;

        let temp_file = self.create_test_audio()?;
        let temp_path = temp_file
            .path()
            .to_str()
            .ok_or_else(|| WhitenoiseError::Internal("Invalid temp path".to_string()))?;

        // Read the file data and compute expected hash
        let test_audio_data = tokio::fs::read(temp_path).await?;
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&test_audio_data);
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
            "✓ Audio uploaded successfully: encrypted_hash={}, original_hash={}",
            hex::encode(&media_file.encrypted_file_hash),
            media_file
                .original_file_hash
                .as_ref()
                .map(hex::encode)
                .unwrap_or_else(|| "none".to_string())
        );

        // Validate audio upload
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
        assert_eq!(media_file.mime_type, "audio/mpeg");
        assert_eq!(media_file.media_type, "chat_media");
        assert!(media_file.file_path.exists());

        tracing::info!("✓ Audio upload validations passed");

        Ok(())
    }
}
