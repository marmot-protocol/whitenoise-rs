use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use mdk_core::media_processing::MediaProcessingOptions;
use nostr_sdk::Url;
use sha2::{Digest, Sha256};

pub struct DownloadChatMediaTestCase {
    account_name: String,
    group_name: String,
}

impl DownloadChatMediaTestCase {
    pub fn new(account_name: &str, group_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
        }
    }

    /// Create a temporary test image file
    fn create_test_image(&self) -> Result<tempfile::NamedTempFile, WhitenoiseError> {
        let temp_file = tempfile::NamedTempFile::new()
            .map_err(|e| WhitenoiseError::Internal(format!("Failed to create temp file: {}", e)))?;

        // Create a distinctive 100x100 blue image for testing
        let img = ::image::RgbaImage::from_pixel(100, 100, ::image::Rgba([0u8, 0, 255, 255]));

        img.save_with_format(temp_file.path(), ::image::ImageFormat::Png)
            .map_err(|e| WhitenoiseError::Internal(format!("Failed to save test image: {}", e)))?;

        Ok(temp_file)
    }
}

#[async_trait]
impl TestCase for DownloadChatMediaTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Testing download_chat_media for group {} using account: {}",
            self.group_name,
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;

        // Step 1: Upload media file (stores both hashes)
        let temp_file = self.create_test_image()?;
        let temp_path = temp_file
            .path()
            .to_str()
            .ok_or_else(|| WhitenoiseError::Internal("Invalid temp path".to_string()))?;

        // Read original file data for later comparison
        let original_file_data = tokio::fs::read(temp_path).await?;

        // Compute expected original_file_hash
        let mut hasher = Sha256::new();
        hasher.update(&original_file_data);
        let expected_original_hash: [u8; 32] = hasher.finalize().into();

        // Upload with test options (no blurhash for simpler test)
        let options = Some(MediaProcessingOptions {
            generate_blurhash: false,
            ..Default::default()
        });

        let blossom_url = if cfg!(debug_assertions) {
            Some(Url::parse("http://localhost:3000").unwrap())
        } else {
            None
        };

        let uploaded_media = context
            .whitenoise
            .upload_chat_media(
                account,
                &group.mls_group_id,
                temp_path,
                blossom_url.clone(),
                options,
            )
            .await?;

        drop(temp_file);

        tracing::info!(
            "✓ Uploaded test media: encrypted_hash={}, original_hash={}",
            hex::encode(&uploaded_media.encrypted_file_hash),
            hex::encode(uploaded_media.original_file_hash.as_ref().unwrap())
        );

        // Step 2: Verify original_file_hash is populated
        assert!(
            uploaded_media.original_file_hash.is_some(),
            "Original file hash should be populated"
        );
        assert_eq!(
            uploaded_media
                .original_file_hash
                .as_ref()
                .unwrap()
                .as_slice(),
            expected_original_hash,
            "Original hash should match file content"
        );

        // Step 3: Delete cached file to simulate not-downloaded state
        let cached_path = uploaded_media.file_path.clone();
        assert!(
            cached_path.exists(),
            "Cached file should exist before deletion"
        );

        tokio::fs::remove_file(&cached_path).await.map_err(|e| {
            WhitenoiseError::Internal(format!("Failed to delete cached file: {}", e))
        })?;

        tracing::info!(
            "✓ Deleted cached file to simulate not-downloaded state: {}",
            cached_path.display()
        );

        assert!(
            !cached_path.exists(),
            "Cached file should not exist after deletion"
        );

        // Step 4: Extract original_file_hash (simulates getting from imeta tag)
        let original_file_hash: [u8; 32] = uploaded_media
            .original_file_hash
            .as_ref()
            .unwrap()
            .as_slice()
            .try_into()
            .map_err(|_| WhitenoiseError::Internal("Invalid hash length".to_string()))?;

        // Step 5: Call download_chat_media with original hash
        tracing::info!(
            "Downloading media using original_file_hash: {}",
            hex::encode(original_file_hash)
        );

        let downloaded_media = context
            .whitenoise
            .download_chat_media(account, &group.mls_group_id, &original_file_hash)
            .await?;

        tracing::info!(
            "✓ Media downloaded and cached to: {}",
            downloaded_media.file_path.display()
        );

        // Step 6: Verify file is re-downloaded and cached
        assert!(
            downloaded_media.file_path.exists(),
            "Downloaded file should exist at: {}",
            downloaded_media.file_path.display()
        );

        assert_eq!(
            downloaded_media.encrypted_file_hash, uploaded_media.encrypted_file_hash,
            "Downloaded media should have same encrypted hash"
        );

        assert_eq!(
            downloaded_media.original_file_hash, uploaded_media.original_file_hash,
            "Downloaded media should have same original hash"
        );

        // Step 7: Verify file content matches original
        let downloaded_content = tokio::fs::read(&downloaded_media.file_path).await?;

        assert_eq!(
            downloaded_content, original_file_data,
            "Downloaded content should match original file content"
        );

        tracing::info!("✓ Downloaded content matches original file");

        // Step 8: Call again to verify idempotency (no re-download)
        tracing::info!("Testing idempotency - calling download_chat_media again...");

        let downloaded_again = context
            .whitenoise
            .download_chat_media(account, &group.mls_group_id, &original_file_hash)
            .await?;

        assert_eq!(
            downloaded_again.file_path, downloaded_media.file_path,
            "Second download should return same file path (idempotent)"
        );

        assert!(
            downloaded_again.file_path.exists(),
            "File should still exist after second download"
        );

        tracing::info!("✓ Idempotency verified - no re-download occurred");

        // Verify all metadata is preserved
        assert_eq!(
            downloaded_again.mime_type, "image/png",
            "MIME type should be preserved"
        );
        assert_eq!(
            downloaded_again.media_type, "chat_media",
            "Media type should be chat_media"
        );
        assert!(
            downloaded_again.file_metadata.is_some(),
            "File metadata should be present"
        );

        let metadata = downloaded_again.file_metadata.as_ref().unwrap();
        assert!(
            metadata.original_filename.is_some(),
            "Original filename should be preserved"
        );

        tracing::info!("✓ All download_chat_media validations passed");

        Ok(())
    }
}
