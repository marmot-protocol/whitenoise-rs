use std::str::FromStr;

use chrono::{DateTime, Utc};

use super::{Database, utils::parse_timestamp};
use crate::whitenoise::{
    app_settings::{AppSettings, Language, ThemeMode},
    error::WhitenoiseError,
};

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
struct AppSettingsRow {
    id: i64,
    theme_mode: String,
    language: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl<'r, R> sqlx::FromRow<'r, R> for AppSettingsRow
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let id = row.try_get("id")?;
        let theme_mode = row.try_get("theme_mode")?;
        let language = row.try_get("language")?;
        let created_at = parse_timestamp(row, "created_at")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(AppSettingsRow {
            id,
            theme_mode,
            language,
            created_at,
            updated_at,
        })
    }
}

impl AppSettingsRow {
    /// Converts an AppSettingsRow to AppSettings
    fn into_app_settings(self) -> Result<AppSettings, WhitenoiseError> {
        let theme_mode = ThemeMode::from_str(&self.theme_mode)
            .map_err(|e| WhitenoiseError::Configuration(format!("Invalid theme mode: {}", e)))?;

        let language = Language::from_str(&self.language)
            .map_err(|e| WhitenoiseError::Configuration(format!("Invalid language: {}", e)))?;

        Ok(AppSettings {
            id: self.id,
            theme_mode,
            language,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

impl AppSettings {
    pub(crate) async fn find_or_create_default(
        database: &Database,
    ) -> Result<AppSettings, WhitenoiseError> {
        match sqlx::query_as::<_, AppSettingsRow>("SELECT * FROM app_settings WHERE id = 1")
            .fetch_one(&database.pool)
            .await
        {
            Ok(settings_row) => Ok(settings_row.into_app_settings()?),
            Err(e) => match e {
                sqlx::Error::RowNotFound => {
                    let settings = AppSettings::default();
                    settings.save(database).await?;
                    Ok(settings)
                }
                _ => Err(WhitenoiseError::SqlxError(e)),
            },
        }
    }

    /// Saves or updates the app settings in the database.
    ///
    /// # Arguments
    ///
    /// * `settings` - A reference to the `AppSettings` to save
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database operation fails.
    pub(crate) async fn save(&self, database: &Database) -> Result<(), WhitenoiseError> {
        sqlx::query(
            "INSERT INTO app_settings (id, theme_mode, language, created_at, updated_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET theme_mode = excluded.theme_mode, language = excluded.language, updated_at = ?"
        )
        .bind(self.id)
        .bind(self.theme_mode.to_string())
        .bind(self.language.to_string())
        .bind(self.created_at.timestamp_millis())
        .bind(self.updated_at.timestamp_millis())
        .bind(Utc::now().timestamp_millis())
        .execute(&database.pool)
        .await
        .map_err(|e| WhitenoiseError::Database(e.into()))?;

        Ok(())
    }

    /// Updates just the theme mode in the app settings.
    ///
    /// # Arguments
    ///
    /// * `theme_mode` - The new `ThemeMode` to set
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database operation fails.
    pub(crate) async fn update_theme_mode(
        theme_mode: ThemeMode,
        database: &Database,
    ) -> Result<(), WhitenoiseError> {
        sqlx::query("UPDATE app_settings SET theme_mode = ?, updated_at = ? WHERE id = 1")
            .bind(theme_mode.to_string())
            .bind(Utc::now().timestamp_millis())
            .execute(&database.pool)
            .await
            .map_err(|e| WhitenoiseError::Database(e.into()))?;

        Ok(())
    }

    /// Updates just the language in the app settings.
    ///
    /// # Arguments
    ///
    /// * `language` - The new language to set
    /// * `database` - A reference to the `Database` instance for database operations
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on success.
    ///
    /// # Errors
    ///
    /// Returns a [`WhitenoiseError`] if the database operation fails.
    pub(crate) async fn update_language(
        language: Language,
        database: &Database,
    ) -> Result<(), WhitenoiseError> {
        sqlx::query("UPDATE app_settings SET language = ?, updated_at = ? WHERE id = 1")
            .bind(language.to_string())
            .bind(Utc::now().timestamp_millis())
            .execute(&database.pool)
            .await
            .map_err(|e| WhitenoiseError::Database(e.into()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteRow;
    use sqlx::{FromRow, SqlitePool};

    async fn setup_test_db() -> SqlitePool {
        let pool = SqlitePool::connect(":memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE app_settings (
                id INTEGER PRIMARY KEY,
                theme_mode TEXT NOT NULL,
                language TEXT NOT NULL DEFAULT 'en',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn test_app_settings_row_from_row() {
        let pool = setup_test_db().await;
        let timestamp = chrono::Utc::now().timestamp_millis();

        sqlx::query(
            "INSERT INTO app_settings (id, theme_mode, language, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(1i64)
        .bind("dark")
        .bind("en")
        .bind(timestamp)
        .bind(timestamp)
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM app_settings WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let app_settings_row = AppSettingsRow::from_row(&row).unwrap();
        assert_eq!(app_settings_row.id, 1);
        assert_eq!(app_settings_row.theme_mode, "dark");
        assert_eq!(app_settings_row.language, "en");
        assert_eq!(app_settings_row.created_at.timestamp_millis(), timestamp);
        assert_eq!(app_settings_row.updated_at.timestamp_millis(), timestamp);
    }

    #[tokio::test]
    async fn test_theme_mode_conversion() {
        let pool = setup_test_db().await;
        let timestamp = chrono::Utc::now().timestamp_millis();

        let test_cases = [
            ("light", ThemeMode::Light),
            ("dark", ThemeMode::Dark),
            ("system", ThemeMode::System),
        ];

        for (theme_str, expected_theme) in test_cases {
            sqlx::query("DELETE FROM app_settings")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO app_settings (id, theme_mode, language, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(1i64)
            .bind(theme_str)
            .bind("en")
            .bind(timestamp)
            .bind(timestamp)
            .execute(&pool)
            .await
            .unwrap();

            let row: SqliteRow = sqlx::query("SELECT * FROM app_settings WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();

            let app_settings = AppSettingsRow::from_row(&row)
                .unwrap()
                .into_app_settings()
                .unwrap();
            assert_eq!(app_settings.theme_mode, expected_theme);
        }
    }

    #[tokio::test]
    async fn test_invalid_timestamp_decode_error() {
        let pool = setup_test_db().await;

        sqlx::query(
            "INSERT INTO app_settings (id, theme_mode, language, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(1i64)
        .bind("light")
        .bind("en")
        .bind(i64::MIN)
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&pool)
        .await
        .unwrap();

        let row: SqliteRow = sqlx::query("SELECT * FROM app_settings WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();

        let result = AppSettingsRow::from_row(&row);
        assert!(matches!(result, Err(sqlx::Error::ColumnDecode { .. })));
    }

    #[test]
    fn test_invalid_theme_mode_error() {
        let timestamp = chrono::Utc::now();
        let app_settings_row = AppSettingsRow {
            id: 1,
            theme_mode: "invalid_theme".to_string(),
            language: "en".to_string(),
            created_at: timestamp,
            updated_at: timestamp,
        };

        let result = app_settings_row.into_app_settings();
        assert!(
            matches!(result, Err(WhitenoiseError::Configuration(msg)) if msg.contains("Invalid theme mode"))
        );
    }

    #[test]
    fn test_invalid_language_error() {
        let timestamp = chrono::Utc::now();
        let app_settings_row = AppSettingsRow {
            id: 1,
            theme_mode: "light".to_string(),
            language: "invalid_lang".to_string(),
            created_at: timestamp,
            updated_at: timestamp,
        };

        let result = app_settings_row.into_app_settings();
        assert!(
            matches!(result, Err(WhitenoiseError::Configuration(msg)) if msg.contains("Invalid language"))
        );
    }

    #[tokio::test]
    async fn test_save_persists_settings() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let settings = AppSettings::new(ThemeMode::System, None);
        settings.save(&whitenoise.database).await.unwrap();

        let loaded = AppSettings::find_or_create_default(&whitenoise.database)
            .await
            .unwrap();
        assert_eq!(loaded.id, settings.id);
        assert_eq!(loaded.theme_mode, settings.theme_mode);
        assert_eq!(loaded.language, settings.language);
    }

    #[tokio::test]
    async fn test_update_theme_mode_changes_value() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Ensure default settings exist
        AppSettings::find_or_create_default(&whitenoise.database)
            .await
            .unwrap();

        AppSettings::update_theme_mode(ThemeMode::Dark, &whitenoise.database)
            .await
            .unwrap();

        let loaded = AppSettings::find_or_create_default(&whitenoise.database)
            .await
            .unwrap();
        assert_eq!(loaded.theme_mode, ThemeMode::Dark);
    }

    #[tokio::test]
    async fn test_update_language_changes_value() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        // Ensure default settings exist
        AppSettings::find_or_create_default(&whitenoise.database)
            .await
            .unwrap();

        AppSettings::update_language(Language::Spanish, &whitenoise.database)
            .await
            .unwrap();

        let loaded = AppSettings::find_or_create_default(&whitenoise.database)
            .await
            .unwrap();
        assert_eq!(loaded.language, Language::Spanish);
    }

    #[tokio::test]
    async fn test_find_or_create_default_handles_row_not_found() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let settings = AppSettings::find_or_create_default(&whitenoise.database)
            .await
            .unwrap();
        assert_eq!(settings.id, 1);
        assert!(matches!(
            settings.theme_mode,
            ThemeMode::Light | ThemeMode::Dark | ThemeMode::System
        ));
    }

    #[tokio::test]
    async fn test_find_or_create_default_propagates_decode_errors() {
        use crate::whitenoise::test_utils::create_mock_whitenoise;

        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        sqlx::query("DELETE FROM app_settings WHERE id = 1")
            .execute(&whitenoise.database.pool)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO app_settings (id, theme_mode, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(1i64)
        .bind("light")
        .bind(i64::MAX)
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(&whitenoise.database.pool)
        .await
        .unwrap();

        let result = AppSettings::find_or_create_default(&whitenoise.database).await;
        assert!(matches!(result, Err(WhitenoiseError::SqlxError(_))));
    }
}
