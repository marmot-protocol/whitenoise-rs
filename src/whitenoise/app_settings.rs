use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Whitenoise, whitenoise::Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum ThemeMode {
    Light,
    Dark,
    System,
}

impl Default for ThemeMode {
    fn default() -> Self {
        Self::System
    }
}

impl fmt::Display for ThemeMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThemeMode::Light => write!(f, "light"),
            ThemeMode::Dark => write!(f, "dark"),
            ThemeMode::System => write!(f, "system"),
        }
    }
}

impl FromStr for ThemeMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "light" => Ok(ThemeMode::Light),
            "dark" => Ok(ThemeMode::Dark),
            "system" => Ok(ThemeMode::System),
            _ => Err(format!("Invalid theme mode: {}", s)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum Language {
    English,
    Spanish,
    French,
    German,
    Italian,
    Portuguese,
    Russian,
    Turkish,
}

impl Default for Language {
    fn default() -> Self {
        Self::English
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Language::English => write!(f, "en"),
            Language::Spanish => write!(f, "es"),
            Language::French => write!(f, "fr"),
            Language::German => write!(f, "de"),
            Language::Italian => write!(f, "it"),
            Language::Portuguese => write!(f, "pt"),
            Language::Russian => write!(f, "ru"),
            Language::Turkish => write!(f, "tr"),
        }
    }
}

impl FromStr for Language {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "en" | "english" => Ok(Language::English),
            "es" | "spanish" => Ok(Language::Spanish),
            "fr" | "french" => Ok(Language::French),
            "de" | "german" => Ok(Language::German),
            "it" | "italian" => Ok(Language::Italian),
            "pt" | "portuguese" => Ok(Language::Portuguese),
            "ru" | "russian" => Ok(Language::Russian),
            "tr" | "turkish" => Ok(Language::Turkish),
            _ => Err(format!("Unsupported language: {}", s)),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AppSettings {
    pub id: i64,
    pub theme_mode: ThemeMode,
    pub language: Language,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            id: 1,
            theme_mode: ThemeMode::System,
            language: Language::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
}

impl AppSettings {
    pub fn new(theme_mode: ThemeMode, language: Option<Language>) -> Self {
        Self {
            id: 1, // Always use id=1 since we only allow one row
            theme_mode,
            language: language.unwrap_or_default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
}

impl Whitenoise {
    /// Loads the current application settings from the database.
    ///
    /// This method retrieves the global application settings, which includes
    /// theme preferences and other UI configuration. If no settings exist
    /// in the database, default settings will be created and saved.
    pub async fn app_settings(&self) -> Result<AppSettings> {
        AppSettings::find_or_create_default(&self.database).await
    }

    /// Updates only the theme mode in the application settings.
    ///
    /// This is a convenience method that loads the current settings,
    /// updates only the theme mode, and saves the settings back to the
    /// database. This is more efficient than manually loading, modifying,
    /// and saving when you only need to change the theme.
    ///
    /// # Arguments
    ///
    /// * `theme_mode` - The new [`ThemeMode`] to set (Light, Dark, or System)
    pub async fn update_theme_mode(&self, theme_mode: ThemeMode) -> Result<()> {
        AppSettings::update_theme_mode(theme_mode, &self.database).await
    }

    /// Updates only the language in the application settings.
    ///
    /// # Arguments
    ///
    /// * `language` - The new [`Language`] to set
    pub async fn update_language(&self, language: Language) -> Result<()> {
        AppSettings::update_language(language, &self.database).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn theme_mode_display_round_trips_via_from_str() {
        for variant in [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System] {
            let round_trip = ThemeMode::from_str(&variant.to_string()).unwrap();
            assert_eq!(round_trip, variant);
        }
    }

    #[test]
    fn theme_mode_from_str_rejects_unknown_value() {
        assert!(ThemeMode::from_str("neon").is_err());
    }

    #[test]
    fn app_settings_new_sets_id_and_theme() {
        let settings = AppSettings::new(ThemeMode::Dark, None);
        assert_eq!(settings.id, 1);
        assert_eq!(settings.theme_mode, ThemeMode::Dark);
        assert_eq!(settings.language, Language::English);
    }

    #[test]
    fn app_settings_new_sets_language() {
        let settings = AppSettings::new(ThemeMode::System, Some(Language::Spanish));
        assert_eq!(settings.language, Language::Spanish);
    }

    #[test]
    fn language_default_is_english() {
        assert_eq!(Language::default(), Language::English);
    }

    #[test]
    fn language_display_round_trips_via_from_str() {
        for variant in [
            Language::English,
            Language::Spanish,
            Language::French,
            Language::German,
            Language::Italian,
            Language::Portuguese,
            Language::Russian,
            Language::Turkish,
        ] {
            let round_trip = Language::from_str(&variant.to_string()).unwrap();
            assert_eq!(round_trip, variant);
        }
    }

    #[test]
    fn language_from_str_accepts_full_names() {
        assert_eq!(Language::from_str("english").unwrap(), Language::English);
        assert_eq!(Language::from_str("spanish").unwrap(), Language::Spanish);
        assert_eq!(Language::from_str("french").unwrap(), Language::French);
        assert_eq!(Language::from_str("german").unwrap(), Language::German);
        assert_eq!(Language::from_str("italian").unwrap(), Language::Italian);
        assert_eq!(Language::from_str("portuguese").unwrap(), Language::Portuguese);
        assert_eq!(Language::from_str("russian").unwrap(), Language::Russian);
        assert_eq!(Language::from_str("turkish").unwrap(), Language::Turkish);
    }

    #[test]
    fn language_from_str_is_case_insensitive() {
        assert_eq!(Language::from_str("EN").unwrap(), Language::English);
        assert_eq!(Language::from_str("SPANISH").unwrap(), Language::Spanish);
        assert_eq!(Language::from_str("Fr").unwrap(), Language::French);
    }

    #[test]
    fn language_from_str_rejects_unknown_value() {
        assert!(Language::from_str("klingon").is_err());
        assert!(Language::from_str("").is_err());
        assert!(Language::from_str("xyz").is_err());
    }
}
