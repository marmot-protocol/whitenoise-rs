use std::io::Write;

use async_trait::async_trait;
use nostr_sdk::Url;

use crate::WhitenoiseError;
use crate::integration_tests::core::*;

/// Test case for uploading PDF documents
pub struct UploadPdfTestCase {
    account_name: String,
    group_name: String,
}

impl UploadPdfTestCase {
    pub fn new(account_name: &str, group_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
        }
    }

    /// Create a temporary PDF file with valid magic bytes
    fn create_test_pdf(&self) -> Result<tempfile::NamedTempFile, WhitenoiseError> {
        let mut temp_file = tempfile::NamedTempFile::new()?;

        // Minimal valid PDF header
        let pdf_header: &[u8] = b"%PDF-1.4\n";

        temp_file.write_all(pdf_header)?;
        temp_file.flush()?;

        Ok(temp_file)
    }
}

#[async_trait]
impl TestCase for UploadPdfTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Uploading PDF document for group {} using account: {}",
            self.group_name,
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;

        let temp_file = self.create_test_pdf()?;
        let temp_path = temp_file
            .path()
            .to_str()
            .ok_or_else(|| WhitenoiseError::Internal("Invalid temp path".to_string()))?;

        // Read the file data and compute expected hash
        let test_pdf_data = tokio::fs::read(temp_path).await?;
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&test_pdf_data);
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
            "✓ PDF uploaded successfully: encrypted_hash={}, original_hash={}",
            hex::encode(&media_file.encrypted_file_hash),
            media_file
                .original_file_hash
                .as_ref()
                .map(hex::encode)
                .unwrap_or_else(|| "none".to_string())
        );

        // Validate PDF upload
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
        assert_eq!(media_file.mime_type, "application/pdf");
        assert_eq!(media_file.media_type, "chat_media");
        assert!(media_file.file_path.exists());

        tracing::info!("✓ PDF upload validations passed");

        Ok(())
    }
}
