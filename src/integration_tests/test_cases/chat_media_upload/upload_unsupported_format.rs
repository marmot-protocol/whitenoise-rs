use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use async_trait::async_trait;
use nostr_sdk::Url;

/// Test case for verifying unsupported format rejection
pub struct UnsupportedFormatTestCase {
    account_name: String,
    group_name: String,
}

impl UnsupportedFormatTestCase {
    pub fn new(account_name: &str, group_name: &str) -> Self {
        Self {
            account_name: account_name.to_string(),
            group_name: group_name.to_string(),
        }
    }

    /// Create a temporary BMP file (detectable but not whitelisted)
    fn create_test_bmp(&self) -> Result<tempfile::NamedTempFile, WhitenoiseError> {
        use std::io::Write;

        let mut temp_file = tempfile::NamedTempFile::new()
            .map_err(|e| WhitenoiseError::Internal(format!("Failed to create temp file: {}", e)))?;

        // BMP magic bytes
        let bmp_header: &[u8] = &[0x42, 0x4D]; // 'BM'

        temp_file
            .write_all(bmp_header)
            .map_err(|e| WhitenoiseError::Internal(format!("Failed to write BMP data: {}", e)))?;

        temp_file
            .flush()
            .map_err(|e| WhitenoiseError::Internal(format!("Failed to flush temp file: {}", e)))?;

        Ok(temp_file)
    }
}

#[async_trait]
impl TestCase for UnsupportedFormatTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!(
            "Testing unsupported format rejection for group {} using account: {}",
            self.group_name,
            self.account_name
        );

        let account = context.get_account(&self.account_name)?;
        let group = context.get_group(&self.group_name)?;

        let temp_file = self.create_test_bmp()?;
        let temp_path = temp_file
            .path()
            .to_str()
            .ok_or_else(|| WhitenoiseError::Internal("Invalid temp path".to_string()))?;

        let blossom_url = if cfg!(debug_assertions) {
            Some(Url::parse("http://localhost:3000").unwrap())
        } else {
            None
        };

        let result = context
            .whitenoise
            .upload_chat_media(account, &group.mls_group_id, temp_path, blossom_url, None)
            .await;

        drop(temp_file);

        // Should fail with UnsupportedMediaFormat error
        match result {
            Err(WhitenoiseError::UnsupportedMediaFormat(msg)) => {
                tracing::info!("✓ Unsupported format correctly rejected: {}", msg);
                assert!(
                    msg.contains("Unsupported media format") || msg.contains("image/bmp"),
                    "Error message should indicate unsupported format"
                );
            }
            Err(e) => {
                return Err(WhitenoiseError::Internal(format!(
                    "Expected UnsupportedMediaFormat error, got: {:?}",
                    e
                )));
            }
            Ok(_) => {
                return Err(WhitenoiseError::Internal(
                    "Expected upload to fail for unsupported format".to_string(),
                ));
            }
        }

        Ok(())
    }
}
