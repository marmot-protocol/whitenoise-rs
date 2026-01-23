use crate::integration_tests::{core::*, test_cases::app_settings::*};
use crate::whitenoise::app_settings::Language;
use crate::{ThemeMode, Whitenoise, WhitenoiseError};
use async_trait::async_trait;

pub struct AppSettingsScenario {
    context: ScenarioContext,
}

impl AppSettingsScenario {
    pub fn new(whitenoise: &'static Whitenoise) -> Self {
        Self {
            context: ScenarioContext::new(whitenoise),
        }
    }
}

#[async_trait]
impl Scenario for AppSettingsScenario {
    fn context(&self) -> &ScenarioContext {
        &self.context
    }

    async fn run_scenario(&mut self) -> Result<(), WhitenoiseError> {
        // Test fetching default settings
        FetchAppSettingsTestCase::basic()
            .expect_theme(ThemeMode::System)
            .expect_language(Language::English)
            .execute(&mut self.context)
            .await?;

        // Test updating to dark mode
        UpdateThemeModeTestCase::to_dark()
            .execute(&mut self.context)
            .await?;

        // Test updating to light mode
        UpdateThemeModeTestCase::to_light()
            .execute(&mut self.context)
            .await?;

        // Test updating back to system mode
        UpdateThemeModeTestCase::to_system()
            .execute(&mut self.context)
            .await?;

        // Test updating language to Spanish
        UpdateLanguageTestCase::new(Language::Spanish)
            .execute(&mut self.context)
            .await?;

        // Test updating language back to English
        UpdateLanguageTestCase::new(Language::English)
            .execute(&mut self.context)
            .await?;

        Ok(())
    }
}
