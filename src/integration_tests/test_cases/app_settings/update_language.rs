use crate::WhitenoiseError;
use crate::integration_tests::core::*;
use crate::whitenoise::app_settings::Language;
use async_trait::async_trait;

pub struct UpdateLanguageTestCase {
    language: Language,
}

impl UpdateLanguageTestCase {
    pub fn new(language: Language) -> Self {
        Self { language }
    }
}

#[async_trait]
impl TestCase for UpdateLanguageTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Updating language to: {:?}", self.language);
        context
            .whitenoise
            .update_language(self.language.clone())
            .await?;

        // Verify the update worked
        let settings = context.whitenoise.app_settings().await?;
        assert_eq!(
            settings.language, self.language,
            "Language was not updated correctly"
        );

        tracing::info!("✓ Language updated and verified: {:?}", self.language);
        Ok(())
    }
}
