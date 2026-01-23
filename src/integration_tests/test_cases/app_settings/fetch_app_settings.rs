use crate::integration_tests::core::*;
use crate::whitenoise::app_settings::Language;
use crate::{ThemeMode, WhitenoiseError};
use async_trait::async_trait;

pub struct FetchAppSettingsTestCase {
    expected_theme: Option<ThemeMode>,
    expected_language: Option<Language>,
}

impl FetchAppSettingsTestCase {
    pub fn basic() -> Self {
        Self {
            expected_theme: None,
            expected_language: None,
        }
    }

    pub fn expect_theme(mut self, theme: ThemeMode) -> Self {
        self.expected_theme = Some(theme);
        self
    }

    pub fn expect_language(mut self, language: Language) -> Self {
        self.expected_language = Some(language);
        self
    }
}

#[async_trait]
impl TestCase for FetchAppSettingsTestCase {
    async fn run(&self, context: &mut ScenarioContext) -> Result<(), WhitenoiseError> {
        tracing::info!("Fetching app settings...");
        let settings = context.whitenoise.app_settings().await?;

        if let Some(expected_theme) = &self.expected_theme {
            assert_eq!(&settings.theme_mode, expected_theme, "Theme mode mismatch");
            tracing::info!("✓ Theme mode verified: {:?}", settings.theme_mode);
        }

        if let Some(expected_language) = &self.expected_language {
            assert_eq!(&settings.language, expected_language, "Language mismatch");
            tracing::info!("✓ Language verified: {:?}", settings.language);
        }

        tracing::info!("✓ App settings fetched successfully");
        Ok(())
    }
}
