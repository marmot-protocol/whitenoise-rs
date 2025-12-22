use nostr_sdk::prelude::*;
use std::str::FromStr;
use std::time::Duration;

use super::error::{Result, WhitenoiseError};

const ZAPSTORE_RELAY_URL: &str = "wss://relay.zapstore.dev";
const WHITE_NOISE_PUBKEY: &str = "75d737c3472471029c44876b330d2284288a42779b591a2ed4daa1c6c07efaf7";

#[derive(Debug, Clone)]
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

pub async fn check_for_app_update(current_version: &str) -> Result<AppUpdateInfo> {
    let client = Client::default();

    let relay_url = RelayUrl::parse(ZAPSTORE_RELAY_URL)
        .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!(e)))?;

    client
        .add_relay(relay_url.clone())
        .await
        .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!(e)))?;

    client.connect().await;

    let pubkey = PublicKey::from_hex(WHITE_NOISE_PUBKEY)
        .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!(e)))?;

    let filter = Filter::new().author(pubkey).kind(Kind::Custom(1063));

    let events = client
        .fetch_events_from([relay_url.clone()], filter, Duration::from_secs(10))
        .await
        .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!(e)))?;

    client.disconnect().await;

    let event = events
        .into_iter()
        .max_by_key(|e| e.created_at)
        .ok_or_else(|| WhitenoiseError::Other(anyhow::anyhow!("No events found")))?;

    let latest_version_str = event
        .tags
        .iter()
        .find_map(|tag| {
            if tag.as_slice().first().map(|s| s.as_str()) == Some("version") {
                tag.as_slice().get(1).map(|s| s.to_string())
            } else {
                None
            }
        })
        .ok_or_else(|| WhitenoiseError::Other(anyhow::anyhow!("No version tag found")))?;

    let latest = Version::from_str(&latest_version_str)
        .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!(e)))?;

    let current = Version::from_str(current_version)
        .map_err(|e| WhitenoiseError::Other(anyhow::anyhow!(e)))?;

    Ok(AppUpdateInfo {
        version: latest.to_string(),
        update_available: latest > current,
    })
}
