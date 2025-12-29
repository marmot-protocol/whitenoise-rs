use nostr_sdk::prelude::*;
use std::str::FromStr;
use std::time::Duration;

use crate::whitenoise::error::{Result, WhitenoiseError};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Configuration for checking app updates from Zapstore
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppUpdateConfig {
    /// The Zapstore relay url. Defaults to wss://relay.zapstore.dev
    pub relay_url: String,
    /// The app publisher's pubkey
    pub publisher_pubkey: String,
    /// The app identifier (package name). Example: org.parres.whitenoise
    pub app_identifier: String,
}

impl Default for AppUpdateConfig {
    fn default() -> Self {
        Self {
            relay_url: "wss://relay.zapstore.dev".to_string(),
            publisher_pubkey: "75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7"
                .to_string(),
            app_identifier: "org.parres.whitenoise".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppUpdateInfo {
    pub version: String,
    pub update_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

impl FromStr for Version {
    type Err = &'static str;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return Err("Invalid version format (expected x.y.z)");
        }

        let major = parts[0].parse().map_err(|_| "Invalid major")?;
        let minor = parts[1].parse().map_err(|_| "Invalid minor")?;
        let patch = parts[2].parse().map_err(|_| "Invalid patch")?;

        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

fn extract_version_from_event(event: &Event) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        if tag.as_slice().first().map(|s| s.as_str()) == Some("version") {
            tag.as_slice().get(1).map(|s| s.to_string())
        } else {
            None
        }
    })
}

fn compare_versions(latest_version: &str, current_version: &str) -> Result<AppUpdateInfo> {
    let latest = Version::from_str(latest_version)
        .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!(e)))?;

    let current = Version::from_str(current_version)
        .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!(e)))?;

    Ok(AppUpdateInfo {
        version: latest.to_string(),
        update_available: latest > current,
    })
}

/// Checks for available application updates by querying the store relay.
///
/// This function connects to the provided store relay (defaults to wss://relay.zapstore.dev) and fetches the latest version
/// information for the app, using the app identifier from the config. It compares the latest available version against
/// the provided current version to determine if an update is available.
///
/// ## Note
///
/// This relies on Kind 1063 from NIP-94 (File Metadata). **However, the NIP does not
/// explicitly define a "version" tag. We expect the version to be included as a tag
/// with key "version", e.g., `["version", "1.2.3"]`.** This convention is used by Zapstore
/// but may not be universal across all app stores.
///
/// ## Arguments
///
/// * `current_version` - The current application version string in semver format (e.g., "1.2.3")
/// * `config` - Configuration for the update check
pub async fn check_for_app_update(
    current_version: &str,
    config: &AppUpdateConfig,
) -> Result<AppUpdateInfo> {
    let client = Client::default();

    let relay_url = RelayUrl::parse(&config.relay_url)?;

    client.add_relay(relay_url.clone()).await?;

    tokio::time::timeout(TIMEOUT, client.connect())
        .await
        .map_err(|_| {
            WhitenoiseError::Other(anyhow::anyhow!("Timeout connecting to the store relay"))
        })?;

    let pubkey = PublicKey::from_hex(&config.publisher_pubkey)?;

    let filter = Filter::new()
        .author(pubkey)
        .kind(Kind::Custom(1063))
        .identifier(&config.app_identifier);

    let events = client
        .fetch_events_from([relay_url.clone()], filter, TIMEOUT)
        .await?;

    client.disconnect().await;

    let event = events
        .into_iter()
        .max_by_key(|e| e.created_at)
        .ok_or_else(|| WhitenoiseError::Other(anyhow::anyhow!("No events found")))?;

    let latest_version_str = extract_version_from_event(&event)
        .ok_or_else(|| WhitenoiseError::Other(anyhow::anyhow!("No version tag found")))?;

    compare_versions(&latest_version_str, current_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Version Parsing Tests (FromStr implementation)
    #[test]
    fn test_version_parse_valid() {
        // Test parsing a standard semantic version string "1.2.3"
        let version = Version::from_str("1.2.3").unwrap();

        assert_eq!(version.major, 1);
        assert_eq!(version.minor, 2);
        assert_eq!(version.patch, 3);
    }

    #[test]
    fn test_version_parse_zeros() {
        // Test parsing version with all zeros
        let version = Version::from_str("0.0.0").unwrap();

        assert_eq!(version.major, 0);
        assert_eq!(version.minor, 0);
        assert_eq!(version.patch, 0);
    }

    #[test]
    fn test_version_parse_large_numbers() {
        // Test parsing version with large numbers to ensure no overflow issues
        let version = Version::from_str("100.200.300").unwrap();

        assert_eq!(version.major, 100);
        assert_eq!(version.minor, 200);
        assert_eq!(version.patch, 300);
    }

    #[test]
    fn test_version_parse_invalid_format_too_few_parts() {
        // Test that parsing fails when version has fewer than 3 parts
        let result = Version::from_str("1.2");

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "Invalid version format (expected x.y.z)"
        );
    }

    #[test]
    fn test_version_parse_invalid_format_too_many_parts() {
        // Test that parsing fails when version has more than 3 parts
        let result = Version::from_str("1.2.3.4");

        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            "Invalid version format (expected x.y.z)"
        );
    }

    #[test]
    fn test_version_parse_invalid_components() {
        let cases = [
            ("abc.2.3", "Invalid major"),
            ("1.xyz.3", "Invalid minor"),
            ("1.2.abc", "Invalid patch"),
        ];

        for (input, expected_err) in cases {
            let result = Version::from_str(input);
            assert_eq!(result.unwrap_err(), expected_err, "input: {input}");
        }
    }

    #[test]
    fn test_version_parse_empty_string() {
        // Test that parsing fails for an empty string
        let result = Version::from_str("");

        assert!(result.is_err());
    }

    #[test]
    fn test_version_parse_negative_numbers() {
        // Test that parsing fails for negative numbers (u32 can't be negative)
        let result = Version::from_str("-1.2.3");

        assert!(result.is_err());
    }

    /// Version Comparison Tests (Ord and PartialOrd implementations)

    #[test]
    fn test_version_compare_equal() {
        // Test that two identical versions are equal
        let v1 = Version::from_str("1.2.3").unwrap();
        let v2 = Version::from_str("1.2.3").unwrap();

        assert_eq!(v1, v2);
        assert_eq!(v1.cmp(&v2), std::cmp::Ordering::Equal);
        assert_eq!(v1.partial_cmp(&v2), Some(std::cmp::Ordering::Equal));
    }

    #[test]
    fn test_version_ordering() {
        let mut versions: Vec<_> = ["2.0.0", "1.2.3", "1.3.0", "1.9.9", "1.2.4", "1.2.9"]
            .into_iter()
            .map(|s| Version::from_str(s).unwrap())
            .collect();

        versions.sort();

        let sorted: Vec<_> = versions.iter().map(|v| v.to_string()).collect();
        assert_eq!(
            sorted,
            ["1.2.3", "1.2.4", "1.2.9", "1.3.0", "1.9.9", "2.0.0"]
        );
    }

    /// Version Display Tests (Display implementation)
    #[test]
    fn test_version_display() {
        for input in ["1.2.3", "0.0.0", "100.200.300"] {
            let version = Version::from_str(input).unwrap();
            assert_eq!(version.to_string(), input);
        }
    }

    /// Constants Tests
    #[test]
    fn test_default_config_pubkey_is_valid() {
        let config = AppUpdateConfig::default();
        PublicKey::from_hex(&config.publisher_pubkey).expect("default pubkey should be valid");
    }

    /// Tests for compare_versions helper function
    #[test]
    fn test_compare_versions_update_available() {
        let result = compare_versions("2.0.0", "1.0.0").unwrap();
        assert_eq!(result.version, "2.0.0");
        assert!(result.update_available);
    }

    #[test]
    fn test_compare_versions_no_update() {
        let result = compare_versions("1.0.0", "1.0.0").unwrap();
        assert_eq!(result.version, "1.0.0");
        assert!(!result.update_available);
    }

    #[test]
    fn test_compare_versions_current_newer() {
        let result = compare_versions("1.0.0", "2.0.0").unwrap();
        assert_eq!(result.version, "1.0.0");
        assert!(!result.update_available);
    }

    #[test]
    fn test_compare_versions_minor_update() {
        let result = compare_versions("1.2.0", "1.1.0").unwrap();
        assert_eq!(result.version, "1.2.0");
        assert!(result.update_available);
    }

    #[test]
    fn test_compare_versions_patch_update() {
        let result = compare_versions("1.0.2", "1.0.1").unwrap();
        assert_eq!(result.version, "1.0.2");
        assert!(result.update_available);
    }

    #[test]
    fn test_compare_versions_invalid_latest() {
        let result = compare_versions("invalid", "1.0.0");
        assert!(result.is_err());
    }

    #[test]
    fn test_compare_versions_invalid_current() {
        let result = compare_versions("1.0.0", "invalid");
        assert!(result.is_err());
    }

    /// Tests for extract_version_from_event helper function
    #[test]
    fn test_extract_version_from_event_valid() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(1063), "test content")
            .tag(Tag::custom(
                TagKind::Custom("version".into()),
                vec!["1.2.3"],
            ))
            .sign_with_keys(&keys)
            .unwrap();

        let version = extract_version_from_event(&event);
        assert_eq!(version, Some("1.2.3".to_string()));
    }

    #[test]
    fn test_extract_version_from_event_no_version_tag() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(1063), "test content")
            .tag(Tag::custom(TagKind::Custom("other".into()), vec!["value"]))
            .sign_with_keys(&keys)
            .unwrap();

        let version = extract_version_from_event(&event);
        assert!(version.is_none());
    }

    #[test]
    fn test_extract_version_from_event_empty_tags() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(1063), "test content")
            .sign_with_keys(&keys)
            .unwrap();

        let version = extract_version_from_event(&event);
        assert!(version.is_none());
    }

    #[test]
    fn test_extract_version_from_event_version_tag_no_value() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(1063), "test content")
            .tag(Tag::custom(
                TagKind::Custom("version".into()),
                Vec::<String>::new(),
            ))
            .sign_with_keys(&keys)
            .unwrap();

        let version = extract_version_from_event(&event);
        assert!(version.is_none());
    }

    #[test]
    fn test_extract_version_from_event_multiple_tags() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(1063), "test content")
            .tag(Tag::custom(
                TagKind::Custom("name".into()),
                vec!["whitenoise"],
            ))
            .tag(Tag::custom(
                TagKind::Custom("version".into()),
                vec!["2.0.0"],
            ))
            .tag(Tag::custom(TagKind::Custom("hash".into()), vec!["abc123"]))
            .sign_with_keys(&keys)
            .unwrap();

        let version = extract_version_from_event(&event);
        assert_eq!(version, Some("2.0.0".to_string()));
    }
}
