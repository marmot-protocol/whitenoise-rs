use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use mdk_core::media_processing::MediaProcessingOptions;
use nostr_sdk::Url;

pub struct UploadChatImageTestCase {
    account_name: String,
    group_name: String,
}

impl UploadChatImageTestCase {
    pub fn basic() -> Self {
        Self {
            account_name: String::new(),
            group_name: String::new(),
        }
    }

    pub fn with_account(mut self, account_name: &str) -> Self {
        self.account_name = account_name.to_string();
        self
    }

    pub fn with_group(mut self, group_name: &str) -> Self {
        self.group_name = group_name.to_string();
        self
    }

    /// Create a temporary test image file using NamedTempFile
    fn create_test_image(&self) -> Result<tempfile::NamedTempFile, WhitenoiseError> {
        let temp_file = tempfile::NamedTempFile::new()
            .map_err(|e| WhitenoiseError::Internal(format!("Failed to create temp file: {}", e)))?;

        let img = ::image::RgbaImage::from_pixel(100, 100, ::image::Rgba([0u8, 255, 0, 255]));

        img.save_with_format(temp_file.path(), ::image::ImageFormat::Png)
            .map_err(|e| WhitenoiseError::Internal(format!("Failed to save test image: {}", e)))?;

        Ok(temp_file)
    }
}

#[async_trait]
impl TestCase for UploadChatImageTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Uploading chat image for group {} using account: {}",
            self.group_name,
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;

        // Create temporary test image
        let temp_file = self.create_test_image()?;
        let temp_path = temp_file
            .path()
            .to_str()
            .ok_or_else(|| WhitenoiseError::Internal("Invalid temp path".to_string()))?;

        // Read the file data and compute expected hash
        let test_image_data = tokio::fs::read(temp_path).await?;
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&test_image_data);
        let expected_original_hash: [u8; 32] = hasher.finalize().into();

        // Use default options (which includes blurhash generation)
        let options = Some(MediaProcessingOptions::default());

        // Upload the media
        let blossom_url = if cfg!(debug_assertions) {
            Some(Url::parse("http://localhost:3000").unwrap())
        } else {
            None // Use default
        };

        let media_file = context
            .whitenoise
            .upload_chat_media(
                account,
                &group.mls_group_id,
                temp_path,
                blossom_url,
                options,
            )
            .await?;

        // Keep temp_file alive until after upload completes
        drop(temp_file);

        tracing::info!(
            "✓ Chat image uploaded successfully: encrypted_hash={}, original_hash={}",
            hex::encode(&media_file.encrypted_file_hash),
            media_file
                .original_file_hash
                .as_ref()
                .map(hex::encode)
                .unwrap_or_else(|| "none".to_string())
        );

        // Validate upload results
        assert!(
            !media_file.encrypted_file_hash.is_empty(),
            "Encrypted file hash should not be empty"
        );
        assert!(
            media_file.original_file_hash.is_some(),
            "Original file hash should be populated for chat media (MIP-04)"
        );

        // Verify original_file_hash matches the SHA-256 of the uploaded file
        assert_eq!(
            media_file.original_file_hash.as_ref().unwrap().as_slice(),
            expected_original_hash,
            "Original file hash should match SHA-256 of uploaded file content"
        );

        assert!(
            media_file.blossom_url.is_some(),
            "Blossom URL should be present"
        );
        assert!(
            media_file.nostr_key.is_some(),
            "Nostr key should be stored for chat media"
        );
        assert_eq!(media_file.mime_type, "image/png", "MIME type should match");
        assert_eq!(
            media_file.media_type, "chat_media",
            "Media type should be chat_media"
        );
        assert!(
            media_file.file_path.exists(),
            "Cached file should exist at: {}",
            media_file.file_path.display()
        );

        // Verify file metadata is present and contains blurhash
        assert!(
            media_file.file_metadata.is_some(),
            "File metadata should be present"
        );

        let metadata = media_file.file_metadata.as_ref().unwrap();
        assert!(
            metadata.original_filename.is_some(),
            "Original filename should be stored in metadata"
        );
        assert!(
            metadata.dimensions.is_some(),
            "Image dimensions should be detected and stored"
        );
        assert!(
            metadata.blurhash.is_some(),
            "Blurhash should be generated with default options"
        );

        tracing::info!("✓ All image file validations passed");

        // Store the media file in context for subsequent tests
        context.add_media_file("uploaded_chat_media", media_file);

        Ok(())
    }
}
