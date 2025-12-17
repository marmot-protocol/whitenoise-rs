use crate::integration_tests::{
    core::*,
    test_cases::{chat_media_upload::*, shared::*},
};
use crate::{Whitenoise, WhitenoiseError};
use async_trait::async_trait;

pub struct ChatMediaUploadScenario {
    context: ScenarioContext,
}

impl ChatMediaUploadScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for ChatMediaUploadScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        // Create test accounts
        CreateAccountsTestCase::with_names(vec!["media_uploader", "media_member"])
            .execute(&mut self.context)
            .await?;

        // Create a test group for media uploads
        CreateGroupTestCase::basic()
            .with_name("media_upload_test_group")
            .with_members("media_uploader", vec!["media_member"])
            .execute(&mut self.context)
            .await?;

        // Note: MLS membership is auto-finalized when welcome is received,
        // so members can participate immediately without explicit accept

        // Upload image with default options (includes blurhash generation)
        UploadChatImageTestCase::basic()
            .with_account("media_uploader")
            .with_group("media_upload_test_group")
            .execute(&mut self.context)
            .await?;

        // Send a message that references the uploaded media and verify aggregation links it
        SendMessageWithMediaTestCase::new("media_uploader", "media_upload_test_group")
            .execute(&mut self.context)
            .await?;

        // Test receiving messages with media and verify MediaFile records are created
        ReceiveMessageWithMediaTestCase::new(
            "media_uploader",
            "media_member",
            "media_upload_test_group",
        )
        .execute(&mut self.context)
        .await?;

        // Test video upload (MP4)
        UploadVideoTestCase::new("media_uploader", "media_upload_test_group")
            .execute(&mut self.context)
            .await?;

        // Test audio upload (MP3)
        UploadAudioTestCase::new("media_uploader", "media_upload_test_group")
            .execute(&mut self.context)
            .await?;

        // Test PDF document upload
        UploadPdfTestCase::new("media_uploader", "media_upload_test_group")
            .execute(&mut self.context)
            .await?;

        // Test download_chat_media API (download → decrypt → cache)
        DownloadChatMediaTestCase::new("media_uploader", "media_upload_test_group")
            .execute(&mut self.context)
            .await?;

        // Test unsupported format rejection (BMP)
        UnsupportedFormatTestCase::new("media_uploader", "media_upload_test_group")
            .execute(&mut self.context)
            .await?;

        tracing::info!("✓ Chat media upload scenario completed with:");
        tracing::info!("  • Image upload with default processing options");
        tracing::info!("  • Blurhash generation verification");
        tracing::info!("  • Metadata extraction and storage");
        tracing::info!("  • Message with media reference sent");
        tracing::info!("  • Message aggregation verified media linking");
        tracing::info!("  • Received message with media creates MediaFile records");
        tracing::info!("  • Video (MP4) upload verified");
        tracing::info!("  • Audio (MP3) upload verified");
        tracing::info!("  • Document (PDF) upload verified");
        tracing::info!("  • Chat media download API verified (MIP-04 compliant)");
        tracing::info!("  • Unsupported format (BMP) rejection verified");

        Ok(())
    }
}
