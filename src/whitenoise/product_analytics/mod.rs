use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mod aptabase;
mod client;
mod events;
mod worker;

pub use events::{
    PRODUCT_ANALYTICS_SCHEMA_VERSION, ProductAnalyticsEvent, ProductAnalyticsEventName,
    ProductAnalyticsNumberProp, ProductAnalyticsStringProp,
};
pub(crate) use worker::ProductAnalytics;

use crate::whitenoise::database::Database;
use crate::whitenoise::error::WhitenoiseError;
use crate::whitenoise::{Result, WhitenoiseConfig};
use crate::{Whitenoise, perf_instrument};

pub const PRODUCT_ANALYTICS_CONSENT_VERSION: &str = "product-analytics-v1";
pub(crate) const PRODUCT_ANALYTICS_MAX_BATCH_SIZE: usize = 25;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductAnalyticsSettings {
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub consent_version: String,
}

impl Default for ProductAnalyticsSettings {
    fn default() -> Self {
        let now = utc_now_millis();
        Self {
            enabled: false,
            created_at: now,
            updated_at: now,
            consent_version: PRODUCT_ANALYTICS_CONSENT_VERSION.to_string(),
        }
    }
}

pub(crate) fn utc_now_millis() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(Utc::now().timestamp_millis())
        .expect("current timestamp must be representable")
}

impl ProductAnalyticsSettings {
    pub(crate) async fn find_or_create_default(
        database: &Database,
    ) -> Result<ProductAnalyticsSettings> {
        crate::whitenoise::database::product_analytics::find_or_create_settings(database).await
    }

    pub(crate) async fn save(&self, database: &Database) -> Result<()> {
        crate::whitenoise::database::product_analytics::save_settings(self, database).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct ProductAnalyticsConfig {
    pub backend: ProductAnalyticsBackend,
    pub app_version: String,
    pub bundle_identifier: String,
    pub device_class: ProductAnalyticsDeviceClass,
    pub os_name: String,
    pub locale: String,
    pub is_debug: bool,
}

impl ProductAnalyticsConfig {
    pub fn validate(&self) -> Result<()> {
        validate_bounded_ascii("app_version", &self.app_version, 64, false)?;
        validate_bounded_ascii("os_name", &self.os_name, 32, false)?;
        validate_bounded_ascii("locale", &self.locale, 35, false)?;
        validate_bundle_identifier(&self.bundle_identifier)?;

        match &self.backend {
            ProductAnalyticsBackend::Disabled => Ok(()),
            ProductAnalyticsBackend::Aptabase(config) => config.validate(),
        }
    }

    pub(crate) fn system_props(&self) -> worker::SystemProps {
        worker::SystemProps {
            locale: self.locale.clone(),
            os_name: self.os_name.clone(),
            is_debug: self.is_debug,
            bundle_identifier: self.bundle_identifier.clone(),
            device_class: self.device_class.as_str().to_string(),
            app_version: self.app_version.clone(),
            sdk_version: format!("whitenoise-rs@{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProductAnalyticsBackend {
    Disabled,
    Aptabase(AptabaseAnalyticsConfig),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct AptabaseAnalyticsConfig {
    pub app_key: String,
    pub host: String,
}

impl AptabaseAnalyticsConfig {
    pub fn validate(&self) -> Result<()> {
        validate_bounded_ascii("aptabase app key", &self.app_key, 128, false)?;
        aptabase::validate_host(&self.host)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProductAnalyticsDeviceClass {
    Phone,
    Tablet,
    Desktop,
    Unknown,
}

impl ProductAnalyticsDeviceClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Phone => "phone",
            Self::Tablet => "tablet",
            Self::Desktop => "desktop",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProductAnalyticsTrackStatus {
    Queued,
    IgnoredDisabled,
    IgnoredUnconfigured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProductAnalyticsFlushStatus {
    /// Pending events were accepted by the configured backend, or dropped after a backend failure.
    Flushed,
    /// Analytics is enabled and configured, but no in-memory events were waiting.
    NothingToFlush,
    /// The device-local consent setting is disabled.
    Disabled,
    /// Consent is enabled, but no analytics backend is configured.
    Unconfigured,
    /// Flush did not complete before the bounded timeout.
    TimedOut,
}

impl WhitenoiseConfig {
    pub(crate) fn validate_product_analytics_config(&self) -> Result<()> {
        match &self.product_analytics_config {
            Some(config) => config.validate(),
            None => Ok(()),
        }
    }
}

impl Whitenoise {
    #[perf_instrument("whitenoise")]
    pub async fn product_analytics_settings(&self) -> Result<ProductAnalyticsSettings> {
        ProductAnalyticsSettings::find_or_create_default(&self.shared.database).await
    }

    #[perf_instrument("whitenoise")]
    pub async fn set_product_analytics_enabled(
        &self,
        enabled: bool,
        consent_version: String,
    ) -> Result<ProductAnalyticsSettings> {
        self.shared
            .product_analytics
            .set_enabled(&self.shared.database, enabled, consent_version)
            .await
    }

    #[perf_instrument("whitenoise")]
    pub async fn track_product_analytics_event(
        &self,
        event: ProductAnalyticsEvent,
    ) -> Result<ProductAnalyticsTrackStatus> {
        self.shared
            .product_analytics
            .track(&self.shared.database, event)
            .await
    }

    #[perf_instrument("whitenoise")]
    pub async fn flush_product_analytics(&self) -> Result<ProductAnalyticsFlushStatus> {
        self.shared
            .product_analytics
            .flush(&self.shared.database)
            .await
    }
}

fn validate_bounded_ascii(
    field: &'static str,
    value: &str,
    max_len: usize,
    allow_empty: bool,
) -> Result<()> {
    if !allow_empty && value.trim().is_empty() {
        return Err(WhitenoiseError::ProductAnalytics(format!(
            "{field} must not be empty"
        )));
    }
    if value.len() > max_len {
        return Err(WhitenoiseError::ProductAnalytics(format!(
            "{field} must be at most {max_len} bytes"
        )));
    }
    if !value.is_ascii() || value.contains("://") || value.contains('\\') {
        return Err(WhitenoiseError::ProductAnalytics(format!(
            "{field} contains unsupported characters"
        )));
    }
    Ok(())
}

fn validate_bundle_identifier(bundle_identifier: &str) -> Result<()> {
    validate_bounded_ascii("bundle identifier", bundle_identifier, 128, false)?;
    if !bundle_identifier.contains('.') {
        return Err(WhitenoiseError::ProductAnalytics(
            "bundle identifier must use reverse-DNS style".to_string(),
        ));
    }
    let valid = bundle_identifier.split('.').all(|segment| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    });
    if !valid {
        return Err(WhitenoiseError::ProductAnalytics(
            "bundle identifier contains an invalid segment".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whitenoise::test_utils::*;

    #[tokio::test]
    async fn product_analytics_settings_default_to_disabled() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let settings = whitenoise.product_analytics_settings().await.unwrap();

        assert!(!settings.enabled);
        assert_eq!(settings.consent_version, PRODUCT_ANALYTICS_CONSENT_VERSION);
    }

    #[tokio::test]
    async fn set_product_analytics_enabled_persists_consent() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let initial = whitenoise.product_analytics_settings().await.unwrap();
        let updated = whitenoise
            .set_product_analytics_enabled(true, PRODUCT_ANALYTICS_CONSENT_VERSION.to_string())
            .await
            .unwrap();
        let loaded = whitenoise.product_analytics_settings().await.unwrap();

        assert!(updated.enabled);
        assert_eq!(loaded.enabled, updated.enabled);
        assert_eq!(loaded.consent_version, PRODUCT_ANALYTICS_CONSENT_VERSION);
        assert_eq!(loaded.created_at, initial.created_at);
    }

    #[tokio::test]
    async fn enabled_but_unconfigured_tracking_is_ignored() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        whitenoise
            .set_product_analytics_enabled(true, PRODUCT_ANALYTICS_CONSENT_VERSION.to_string())
            .await
            .unwrap();

        let status = whitenoise
            .track_product_analytics_event(ProductAnalyticsEvent::new(
                ProductAnalyticsEventName::AppStarted,
            ))
            .await
            .unwrap();

        assert_eq!(status, ProductAnalyticsTrackStatus::IgnoredUnconfigured);
    }

    #[tokio::test]
    async fn flush_reports_disabled_by_default() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

        let status = whitenoise.flush_product_analytics().await.unwrap();

        assert_eq!(status, ProductAnalyticsFlushStatus::Disabled);
    }

    #[test]
    fn analytics_config_rejects_bad_bundle_identifier() {
        let config = ProductAnalyticsConfig {
            backend: ProductAnalyticsBackend::Disabled,
            app_version: "1.0.0".to_string(),
            bundle_identifier: "not a bundle".to_string(),
            device_class: ProductAnalyticsDeviceClass::Phone,
            os_name: "iOS".to_string(),
            locale: "en-US".to_string(),
            is_debug: false,
        };

        assert!(config.validate().is_err());
    }

    #[test]
    fn analytics_config_accepts_expected_os_name_with_slash() {
        let config = ProductAnalyticsConfig {
            backend: ProductAnalyticsBackend::Disabled,
            app_version: "1.0.0".to_string(),
            bundle_identifier: "dev.ipf.whitenoise.staging".to_string(),
            device_class: ProductAnalyticsDeviceClass::Desktop,
            os_name: "Linux/x86_64".to_string(),
            locale: "en-US".to_string(),
            is_debug: false,
        };

        config.validate().unwrap();
    }

    #[test]
    fn analytics_config_rejects_whitespace_only_metadata() {
        let config = ProductAnalyticsConfig {
            backend: ProductAnalyticsBackend::Disabled,
            app_version: "   ".to_string(),
            bundle_identifier: "dev.ipf.whitenoise.staging".to_string(),
            device_class: ProductAnalyticsDeviceClass::Phone,
            os_name: "iOS".to_string(),
            locale: "en-US".to_string(),
            is_debug: false,
        };

        assert!(config.validate().is_err());
    }

    #[test]
    fn analytics_config_accepts_aptabase_backend_and_builds_system_props() {
        let config = ProductAnalyticsConfig {
            backend: ProductAnalyticsBackend::Aptabase(AptabaseAnalyticsConfig {
                app_key: "A-TEST".to_string(),
                host: "http://127.0.0.1:12345".to_string(),
            }),
            app_version: "1.2.3".to_string(),
            bundle_identifier: "dev.ipf.whitenoise.staging".to_string(),
            device_class: ProductAnalyticsDeviceClass::Tablet,
            os_name: "Android 14".to_string(),
            locale: "en-US".to_string(),
            is_debug: true,
        };

        config.validate().unwrap();

        let props = config.system_props();
        assert_eq!(props.bundle_identifier, "dev.ipf.whitenoise.staging");
        assert_eq!(props.device_class, "tablet");
        assert_eq!(props.app_version, "1.2.3");
        assert_eq!(props.os_name, "Android 14");
        assert_eq!(props.locale, "en-US");
        assert!(props.is_debug);
        assert!(props.sdk_version.starts_with("whitenoise-rs@"));
    }

    #[test]
    fn analytics_config_rejects_invalid_aptabase_and_bundle_metadata() {
        let bad_app_key = ProductAnalyticsConfig {
            backend: ProductAnalyticsBackend::Aptabase(AptabaseAnalyticsConfig {
                app_key: " ".to_string(),
                host: "https://analytics.example.com".to_string(),
            }),
            app_version: "1.0.0".to_string(),
            bundle_identifier: "dev.ipf.whitenoise.staging".to_string(),
            device_class: ProductAnalyticsDeviceClass::Phone,
            os_name: "iOS".to_string(),
            locale: "en-US".to_string(),
            is_debug: false,
        };
        assert!(bad_app_key.validate().is_err());

        let bad_bundle = ProductAnalyticsConfig {
            backend: ProductAnalyticsBackend::Disabled,
            app_version: "1.0.0".to_string(),
            bundle_identifier: "dev..whitenoise".to_string(),
            device_class: ProductAnalyticsDeviceClass::Phone,
            os_name: "iOS".to_string(),
            locale: "en-US".to_string(),
            is_debug: false,
        };
        assert!(bad_bundle.validate().is_err());
    }

    #[test]
    fn analytics_config_rejects_length_and_url_like_metadata() {
        let too_long_version = ProductAnalyticsConfig {
            backend: ProductAnalyticsBackend::Disabled,
            app_version: "1".repeat(65),
            bundle_identifier: "dev.ipf.whitenoise.staging".to_string(),
            device_class: ProductAnalyticsDeviceClass::Phone,
            os_name: "iOS".to_string(),
            locale: "en-US".to_string(),
            is_debug: false,
        };
        assert!(too_long_version.validate().is_err());

        let url_like_locale = ProductAnalyticsConfig {
            backend: ProductAnalyticsBackend::Disabled,
            app_version: "1.0.0".to_string(),
            bundle_identifier: "dev.ipf.whitenoise.staging".to_string(),
            device_class: ProductAnalyticsDeviceClass::Phone,
            os_name: "iOS".to_string(),
            locale: "https://locale.example".to_string(),
            is_debug: false,
        };
        assert!(url_like_locale.validate().is_err());

        let path_like_bundle = ProductAnalyticsConfig {
            backend: ProductAnalyticsBackend::Disabled,
            app_version: "1.0.0".to_string(),
            bundle_identifier: "dev.ipf\\whitenoise".to_string(),
            device_class: ProductAnalyticsDeviceClass::Phone,
            os_name: "iOS".to_string(),
            locale: "en-US".to_string(),
            is_debug: false,
        };
        assert!(path_like_bundle.validate().is_err());
    }

    #[test]
    fn whitenoise_config_validates_optional_product_analytics_config() {
        let (mut config, _data_temp, _logs_temp) = create_test_config();
        assert!(config.validate_product_analytics_config().is_ok());

        config.product_analytics_config = Some(ProductAnalyticsConfig {
            backend: ProductAnalyticsBackend::Disabled,
            app_version: "1.0.0".to_string(),
            bundle_identifier: "dev.ipf.whitenoise.staging".to_string(),
            device_class: ProductAnalyticsDeviceClass::Desktop,
            os_name: "macOS".to_string(),
            locale: "en-US".to_string(),
            is_debug: true,
        });
        assert!(config.validate_product_analytics_config().is_ok());

        config.product_analytics_config = Some(ProductAnalyticsConfig {
            backend: ProductAnalyticsBackend::Disabled,
            app_version: "1.0.0".to_string(),
            bundle_identifier: "invalid".to_string(),
            device_class: ProductAnalyticsDeviceClass::Desktop,
            os_name: "macOS".to_string(),
            locale: "en-US".to_string(),
            is_debug: true,
        });
        assert!(config.validate_product_analytics_config().is_err());
    }

    #[test]
    fn device_class_strings_are_stable() {
        assert_eq!(ProductAnalyticsDeviceClass::Phone.as_str(), "phone");
        assert_eq!(ProductAnalyticsDeviceClass::Tablet.as_str(), "tablet");
        assert_eq!(ProductAnalyticsDeviceClass::Desktop.as_str(), "desktop");
        assert_eq!(ProductAnalyticsDeviceClass::Unknown.as_str(), "unknown");
    }
}
