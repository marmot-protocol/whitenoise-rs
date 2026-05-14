use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use crate::whitenoise::Result;
use crate::whitenoise::error::WhitenoiseError;

pub const PRODUCT_ANALYTICS_SCHEMA_VERSION: u32 = 1;
const MAX_PROP_KEY_LEN: usize = 48;
const MAX_PROP_VALUE_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProductAnalyticsEvent {
    pub name: ProductAnalyticsEventName,
    pub string_props: Vec<ProductAnalyticsStringProp>,
    pub number_props: Vec<ProductAnalyticsNumberProp>,
}

impl ProductAnalyticsEvent {
    pub fn new(name: ProductAnalyticsEventName) -> Self {
        Self {
            name,
            string_props: Vec::new(),
            number_props: vec![ProductAnalyticsNumberProp {
                key: "schema_version".to_string(),
                value: f64::from(PRODUCT_ANALYTICS_SCHEMA_VERSION),
            }],
        }
    }

    pub fn with_string_prop(mut self, key: &str, value: &str) -> Self {
        self.string_props.push(ProductAnalyticsStringProp {
            key: key.to_string(),
            value: value.to_string(),
        });
        self
    }

    pub fn with_number_prop(mut self, key: &str, value: f64) -> Self {
        self.number_props.push(ProductAnalyticsNumberProp {
            key: key.to_string(),
            value,
        });
        self
    }

    pub(crate) fn validate(&self) -> Result<()> {
        let event_name = self.name.as_str();
        if event_name.is_empty() {
            return Err(analytics_error("event name must not be empty"));
        }

        let mut keys = HashSet::new();
        for prop in &self.string_props {
            validate_prop_key(self.name, &prop.key)?;
            validate_string_prop_value(&prop.key, &prop.value)?;
            if !keys.insert(prop.key.as_str()) {
                return Err(analytics_error("duplicate analytics prop key"));
            }
        }

        for prop in &self.number_props {
            validate_prop_key(self.name, &prop.key)?;
            if !prop.value.is_finite() {
                return Err(analytics_error("analytics number prop must be finite"));
            }
            if !keys.insert(prop.key.as_str()) {
                return Err(analytics_error("duplicate analytics prop key"));
            }
        }

        Ok(())
    }

    pub(crate) fn validated_props(&self) -> Result<Map<String, Value>> {
        self.validate()?;

        let mut props = Map::with_capacity(self.string_props.len() + self.number_props.len());
        for prop in &self.string_props {
            props.insert(prop.key.clone(), Value::String(prop.value.clone()));
        }
        for prop in &self.number_props {
            let Some(number) = Number::from_f64(prop.value) else {
                return Err(analytics_error("analytics number prop must be finite"));
            };
            props.insert(prop.key.clone(), Value::Number(number));
        }
        Ok(props)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProductAnalyticsEventName {
    AnalyticsEnabled,
    AppStarted,
    AppForegrounded,
    AppBackgrounded,
    OnboardingStarted,
    OnboardingCompleted,
    IdentityCreated,
    LoginStarted,
    LoginCompleted,
    LoginFailed,
    MessageSendStarted,
    MessageSendCompleted,
    MessageSendFailed,
    GroupCreateStarted,
    GroupCreateCompleted,
    GroupCreateFailed,
    MembersAdded,
    MembersRemoved,
    GroupDataUpdated,
    MediaUploadStarted,
    MediaUploadCompleted,
    MediaUploadFailed,
    PushRegistrationCompleted,
    PushRegistrationFailed,
    SettingChanged,
}

impl ProductAnalyticsEventName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnalyticsEnabled => "analytics_enabled",
            Self::AppStarted => "app_started",
            Self::AppForegrounded => "app_foregrounded",
            Self::AppBackgrounded => "app_backgrounded",
            Self::OnboardingStarted => "onboarding_started",
            Self::OnboardingCompleted => "onboarding_completed",
            Self::IdentityCreated => "identity_created",
            Self::LoginStarted => "login_started",
            Self::LoginCompleted => "login_completed",
            Self::LoginFailed => "login_failed",
            Self::MessageSendStarted => "message_send_started",
            Self::MessageSendCompleted => "message_send_completed",
            Self::MessageSendFailed => "message_send_failed",
            Self::GroupCreateStarted => "group_create_started",
            Self::GroupCreateCompleted => "group_create_completed",
            Self::GroupCreateFailed => "group_create_failed",
            Self::MembersAdded => "members_added",
            Self::MembersRemoved => "members_removed",
            Self::GroupDataUpdated => "group_data_updated",
            Self::MediaUploadStarted => "media_upload_started",
            Self::MediaUploadCompleted => "media_upload_completed",
            Self::MediaUploadFailed => "media_upload_failed",
            Self::PushRegistrationCompleted => "push_registration_completed",
            Self::PushRegistrationFailed => "push_registration_failed",
            Self::SettingChanged => "setting_changed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct ProductAnalyticsStringProp {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProductAnalyticsNumberProp {
    pub key: String,
    pub value: f64,
}

fn validate_prop_key(event_name: ProductAnalyticsEventName, key: &str) -> Result<()> {
    if key.is_empty() || key.len() > MAX_PROP_KEY_LEN || !key.is_ascii() {
        return Err(analytics_error("analytics prop key is invalid"));
    }
    if !event_allows_prop(event_name, key) {
        return Err(analytics_error("analytics prop is not allowed for event"));
    }
    Ok(())
}

fn validate_string_prop_value(key: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_PROP_VALUE_LEN || !value.is_ascii() {
        return Err(analytics_error("analytics string prop value is invalid"));
    }
    if contains_sensitive_pattern(value) {
        return Err(analytics_error(
            "analytics string prop value looks like sensitive data",
        ));
    }
    if !global_prop_value_allowed(key, value) {
        return Err(analytics_error(
            "analytics string prop value is not allowed",
        ));
    }
    Ok(())
}

fn event_allows_prop(event_name: ProductAnalyticsEventName, key: &str) -> bool {
    if key == "schema_version" {
        return true;
    }

    match event_name {
        ProductAnalyticsEventName::AnalyticsEnabled => false,
        ProductAnalyticsEventName::AppStarted
        | ProductAnalyticsEventName::AppForegrounded
        | ProductAnalyticsEventName::AppBackgrounded
        | ProductAnalyticsEventName::PushRegistrationCompleted => key == "platform",
        ProductAnalyticsEventName::OnboardingStarted
        | ProductAnalyticsEventName::OnboardingCompleted
        | ProductAnalyticsEventName::GroupCreateStarted
        | ProductAnalyticsEventName::GroupDataUpdated => false,
        ProductAnalyticsEventName::IdentityCreated | ProductAnalyticsEventName::LoginStarted => {
            key == "account_type"
        }
        ProductAnalyticsEventName::LoginCompleted => {
            matches!(key, "account_type" | "duration_bucket")
        }
        ProductAnalyticsEventName::LoginFailed => {
            matches!(key, "account_type" | "error_kind" | "duration_bucket")
        }
        ProductAnalyticsEventName::MessageSendStarted => key == "chat_type",
        ProductAnalyticsEventName::MessageSendCompleted => {
            matches!(key, "chat_type" | "duration_bucket")
        }
        ProductAnalyticsEventName::MessageSendFailed => {
            matches!(key, "chat_type" | "error_kind" | "duration_bucket")
        }
        ProductAnalyticsEventName::GroupCreateCompleted => key == "duration_bucket",
        ProductAnalyticsEventName::GroupCreateFailed => {
            matches!(key, "error_kind" | "duration_bucket")
        }
        ProductAnalyticsEventName::MembersAdded | ProductAnalyticsEventName::MembersRemoved => {
            key == "member_count_bucket"
        }
        ProductAnalyticsEventName::MediaUploadStarted => {
            matches!(key, "media_kind" | "media_size_bucket")
        }
        ProductAnalyticsEventName::MediaUploadCompleted => {
            matches!(key, "media_kind" | "media_size_bucket" | "duration_bucket")
        }
        ProductAnalyticsEventName::MediaUploadFailed => {
            matches!(
                key,
                "media_kind" | "media_size_bucket" | "error_kind" | "duration_bucket"
            )
        }
        ProductAnalyticsEventName::PushRegistrationFailed => {
            matches!(key, "platform" | "error_kind")
        }
        ProductAnalyticsEventName::SettingChanged => matches!(key, "setting" | "value"),
    }
}

fn global_prop_value_allowed(key: &str, value: &str) -> bool {
    match key {
        "platform" => matches!(
            value,
            "ios" | "android" | "macos" | "linux" | "windows" | "web" | "unknown"
        ),
        "account_type" => matches!(value, "local_key" | "external_signer" | "unknown"),
        "chat_type" => matches!(value, "dm" | "group" | "unknown"),
        "media_kind" => matches!(value, "image" | "video" | "audio" | "pdf" | "other"),
        "error_kind" => matches!(
            value,
            "network" | "timeout" | "permission" | "validation" | "storage" | "crypto" | "unknown"
        ),
        "setting" => matches!(value, "theme" | "language" | "notifications" | "analytics"),
        "value" => matches!(
            value,
            "light"
                | "dark"
                | "system"
                | "en"
                | "es"
                | "fr"
                | "de"
                | "it"
                | "pt"
                | "ru"
                | "tr"
                | "enabled"
                | "disabled"
        ),
        "member_count_bucket" => matches!(value, "1" | "2" | "3_5" | "6_10" | "11_25" | "26_plus"),
        "media_size_bucket" => matches!(value, "lt_1mb" | "1_5mb" | "5_25mb" | "25mb_plus"),
        "duration_bucket" => matches!(
            value,
            "lt_250ms" | "250ms_1s" | "1_5s" | "5_30s" | "30s_plus"
        ),
        _ => false,
    }
}

fn contains_sensitive_pattern(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("npub")
        || lower.starts_with("nsec")
        || lower.starts_with("nevent")
        || lower.starts_with("nprofile")
        || lower.starts_with("naddr")
        || lower.starts_with("nrelay")
        || lower.starts_with("wss://")
        || lower.starts_with("ws://")
        || lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("blossom://")
        || lower.starts_with('/')
        || lower.starts_with("~/")
        || lower.contains("\\")
    {
        return true;
    }

    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn analytics_error(message: &'static str) -> WhitenoiseError {
    WhitenoiseError::ProductAnalytics(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_serialize_to_stable_snake_case() {
        let cases = [
            (
                ProductAnalyticsEventName::AnalyticsEnabled,
                "analytics_enabled",
            ),
            (ProductAnalyticsEventName::AppStarted, "app_started"),
            (
                ProductAnalyticsEventName::AppForegrounded,
                "app_foregrounded",
            ),
            (
                ProductAnalyticsEventName::AppBackgrounded,
                "app_backgrounded",
            ),
            (
                ProductAnalyticsEventName::OnboardingStarted,
                "onboarding_started",
            ),
            (
                ProductAnalyticsEventName::OnboardingCompleted,
                "onboarding_completed",
            ),
            (
                ProductAnalyticsEventName::IdentityCreated,
                "identity_created",
            ),
            (ProductAnalyticsEventName::LoginStarted, "login_started"),
            (ProductAnalyticsEventName::LoginCompleted, "login_completed"),
            (ProductAnalyticsEventName::LoginFailed, "login_failed"),
            (
                ProductAnalyticsEventName::MessageSendStarted,
                "message_send_started",
            ),
            (
                ProductAnalyticsEventName::MessageSendCompleted,
                "message_send_completed",
            ),
            (
                ProductAnalyticsEventName::MessageSendFailed,
                "message_send_failed",
            ),
            (
                ProductAnalyticsEventName::GroupCreateStarted,
                "group_create_started",
            ),
            (
                ProductAnalyticsEventName::GroupCreateCompleted,
                "group_create_completed",
            ),
            (
                ProductAnalyticsEventName::GroupCreateFailed,
                "group_create_failed",
            ),
            (ProductAnalyticsEventName::MembersAdded, "members_added"),
            (ProductAnalyticsEventName::MembersRemoved, "members_removed"),
            (
                ProductAnalyticsEventName::GroupDataUpdated,
                "group_data_updated",
            ),
            (
                ProductAnalyticsEventName::MediaUploadStarted,
                "media_upload_started",
            ),
            (
                ProductAnalyticsEventName::MediaUploadCompleted,
                "media_upload_completed",
            ),
            (
                ProductAnalyticsEventName::MediaUploadFailed,
                "media_upload_failed",
            ),
            (
                ProductAnalyticsEventName::PushRegistrationCompleted,
                "push_registration_completed",
            ),
            (
                ProductAnalyticsEventName::PushRegistrationFailed,
                "push_registration_failed",
            ),
            (ProductAnalyticsEventName::SettingChanged, "setting_changed"),
        ];

        for (event_name, expected) in cases {
            assert_eq!(event_name.as_str(), expected);
        }
    }

    #[test]
    fn validator_accepts_first_pass_event_prop_matrix() {
        let cases = [
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::AnalyticsEnabled),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                .with_string_prop("platform", "ios"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppForegrounded)
                .with_string_prop("platform", "android"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppBackgrounded)
                .with_string_prop("platform", "macos"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::OnboardingStarted),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::OnboardingCompleted),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::IdentityCreated)
                .with_string_prop("account_type", "local_key"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::LoginStarted)
                .with_string_prop("account_type", "external_signer"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::LoginCompleted)
                .with_string_prop("account_type", "unknown")
                .with_string_prop("duration_bucket", "lt_250ms"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::LoginFailed)
                .with_string_prop("account_type", "local_key")
                .with_string_prop("error_kind", "timeout")
                .with_string_prop("duration_bucket", "250ms_1s"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::MessageSendStarted)
                .with_string_prop("chat_type", "dm"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::MessageSendCompleted)
                .with_string_prop("chat_type", "group")
                .with_string_prop("duration_bucket", "1_5s"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::MessageSendFailed)
                .with_string_prop("chat_type", "unknown")
                .with_string_prop("error_kind", "crypto")
                .with_string_prop("duration_bucket", "5_30s"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::GroupCreateStarted),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::GroupCreateCompleted)
                .with_string_prop("duration_bucket", "30s_plus"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::GroupCreateFailed)
                .with_string_prop("error_kind", "validation")
                .with_string_prop("duration_bucket", "250ms_1s"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::MembersAdded)
                .with_string_prop("member_count_bucket", "3_5"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::MembersRemoved)
                .with_string_prop("member_count_bucket", "26_plus"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::GroupDataUpdated),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::MediaUploadStarted)
                .with_string_prop("media_kind", "image")
                .with_string_prop("media_size_bucket", "lt_1mb"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::MediaUploadCompleted)
                .with_string_prop("media_kind", "video")
                .with_string_prop("media_size_bucket", "1_5mb")
                .with_string_prop("duration_bucket", "1_5s"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::MediaUploadFailed)
                .with_string_prop("media_kind", "pdf")
                .with_string_prop("media_size_bucket", "25mb_plus")
                .with_string_prop("error_kind", "storage")
                .with_string_prop("duration_bucket", "30s_plus"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::PushRegistrationCompleted)
                .with_string_prop("platform", "ios"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::PushRegistrationFailed)
                .with_string_prop("platform", "android")
                .with_string_prop("error_kind", "permission"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::SettingChanged)
                .with_string_prop("setting", "analytics")
                .with_string_prop("value", "enabled"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::SettingChanged)
                .with_string_prop("setting", "notifications")
                .with_string_prop("value", "disabled"),
        ];

        for event in cases {
            event.validate().unwrap();
        }
    }

    #[test]
    fn validated_props_includes_schema_version_and_string_props() {
        let props = ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
            .with_string_prop("platform", "ios")
            .validated_props()
            .unwrap();

        assert_eq!(
            props.get("platform"),
            Some(&Value::String("ios".to_string()))
        );
        assert_eq!(props.get("schema_version"), Some(&Value::from(1.0)));
    }

    #[test]
    fn validator_accepts_allowed_event_props() {
        let event = ProductAnalyticsEvent::new(ProductAnalyticsEventName::MessageSendFailed)
            .with_string_prop("chat_type", "group")
            .with_string_prop("error_kind", "network")
            .with_string_prop("duration_bucket", "1_5s");

        event.validate().unwrap();
    }

    #[test]
    fn validator_rejects_globally_valid_prop_for_wrong_event() {
        let event = ProductAnalyticsEvent::new(ProductAnalyticsEventName::GroupCreateStarted)
            .with_string_prop("platform", "ios");

        assert!(event.validate().is_err());
    }

    #[test]
    fn validator_rejects_props_for_analytics_enabled() {
        let event = ProductAnalyticsEvent::new(ProductAnalyticsEventName::AnalyticsEnabled)
            .with_string_prop("platform", "ios");

        assert!(event.validate().is_err());
    }

    #[test]
    fn global_prop_values_reject_unknown_keys() {
        assert!(!global_prop_value_allowed("future_key", "value"));
    }

    #[test]
    fn validator_rejects_invalid_prop_keys_and_values() {
        let long_key = "x".repeat(MAX_PROP_KEY_LEN + 1);
        let long_value = "x".repeat(MAX_PROP_VALUE_LEN + 1);
        let cases = [
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                .with_string_prop("", "ios"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                .with_string_prop(&long_key, "ios"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                .with_string_prop("platform", "ios-\u{1f600}"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                .with_string_prop("platform", ""),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                .with_string_prop("platform", &long_value),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                .with_string_prop("platform", "beos"),
            ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
                .with_string_prop("platform", "nrelay1qqt8wumn8ghj7"),
        ];

        for event in cases {
            assert!(event.validate().is_err());
        }
    }

    #[test]
    fn validator_rejects_duplicate_prop_keys() {
        let event = ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
            .with_string_prop("platform", "ios")
            .with_string_prop("platform", "android");

        assert!(event.validate().is_err());
    }

    #[test]
    fn validator_rejects_duplicate_number_prop_keys() {
        let event = ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
            .with_number_prop("schema_version", 1.0);

        assert!(event.validate().is_err());
    }

    #[test]
    fn validator_rejects_sensitive_patterns() {
        let event = ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
            .with_string_prop(
                "platform",
                "https://relay.example/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            );

        assert!(event.validate().is_err());
    }

    #[test]
    fn validator_rejects_extended_nip19_patterns() {
        assert!(contains_sensitive_pattern("naddr1qqxnzdesxqcrqvpsxq"));
        assert!(contains_sensitive_pattern(
            "nrelay1qqt8wumn8ghj7un9d3shjtnyv9kh2uewd9hs"
        ));
    }

    #[test]
    fn validator_rejects_non_finite_numbers() {
        let event = ProductAnalyticsEvent::new(ProductAnalyticsEventName::AppStarted)
            .with_number_prop("schema_version", f64::NAN);

        assert!(event.validate().is_err());
    }
}
