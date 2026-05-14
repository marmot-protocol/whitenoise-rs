use chrono::Utc;

use super::Database;
use super::utils::parse_timestamp;
use crate::perf_span;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::product_analytics::ProductAnalyticsSettings;

impl<'r, R> sqlx::FromRow<'r, R> for ProductAnalyticsSettings
where
    R: sqlx::Row,
    &'r str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    fn from_row(row: &'r R) -> std::result::Result<Self, sqlx::Error> {
        let enabled_int: i64 = row.try_get("enabled")?;
        let consent_version: String = row.try_get("consent_version")?;
        let updated_at = parse_timestamp(row, "updated_at")?;

        Ok(Self {
            enabled: enabled_int == 1,
            updated_at,
            consent_version,
        })
    }
}

pub(crate) async fn find_or_create_settings(
    database: &Database,
) -> Result<ProductAnalyticsSettings, WhitenoiseError> {
    let _span = perf_span!("db::product_analytics_find_or_create");
    match sqlx::query_as::<_, ProductAnalyticsSettings>(
        "SELECT enabled, consent_version, updated_at FROM product_analytics_settings WHERE id = 1",
    )
    .fetch_one(&database.pool)
    .await
    {
        Ok(settings) => Ok(settings),
        Err(e) => match e {
            sqlx::Error::RowNotFound => {
                let settings = ProductAnalyticsSettings::default();
                save_settings(&settings, database).await?;
                Ok(settings)
            }
            _ => Err(WhitenoiseError::SqlxError(e)),
        },
    }
}

pub(crate) async fn save_settings(
    settings: &ProductAnalyticsSettings,
    database: &Database,
) -> Result<(), WhitenoiseError> {
    let _span = perf_span!("db::product_analytics_save");
    let now = Utc::now().timestamp_millis();
    sqlx::query(
        "INSERT INTO product_analytics_settings \
         (id, enabled, consent_version, created_at, updated_at) VALUES (1, ?, ?, ?, ?) \
         ON CONFLICT(id) DO UPDATE SET \
         enabled = excluded.enabled, \
         consent_version = excluded.consent_version, \
         updated_at = excluded.updated_at",
    )
    .bind(i64::from(settings.enabled))
    .bind(&settings.consent_version)
    .bind(now)
    .bind(settings.updated_at.timestamp_millis())
    .execute(&database.pool)
    .await
    .map_err(|e| WhitenoiseError::Database(e.into()))?;

    Ok(())
}
