use std::{collections::HashSet, str::FromStr};

use chrono::{DateTime, Utc};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::whitenoise::{Whitenoise, accounts::Account, error::Result};

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Hash)]
pub struct Relay {
    pub id: Option<i64>,
    pub url: RelayUrl,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum RelayType {
    Nip65,
    Inbox,
    KeyPackage,
}

impl FromStr for RelayType {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "nip65" => Ok(Self::Nip65),
            "inbox" => Ok(Self::Inbox),
            "key_package" => Ok(Self::KeyPackage),
            _ => Err(format!("Invalid relay type: {}", s)),
        }
    }
}

impl From<RelayType> for u16 {
    fn from(relay_type: RelayType) -> Self {
        match relay_type {
            RelayType::Nip65 => 10002,
            RelayType::Inbox => 10050,
            RelayType::KeyPackage => 10051,
        }
    }
}

impl From<RelayType> for String {
    fn from(relay_type: RelayType) -> Self {
        match relay_type {
            RelayType::Nip65 => "nip65".to_string(),
            RelayType::Inbox => "inbox".to_string(),
            RelayType::KeyPackage => "key_package".to_string(),
        }
    }
}

impl From<RelayType> for Kind {
    fn from(relay_type: RelayType) -> Self {
        match relay_type {
            RelayType::Nip65 => Kind::RelayList,
            RelayType::Inbox => Kind::InboxRelays,
            RelayType::KeyPackage => Kind::MlsKeyPackageRelays,
        }
    }
}

impl From<Kind> for RelayType {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::RelayList => RelayType::Nip65,
            Kind::InboxRelays => RelayType::Inbox,
            Kind::MlsKeyPackageRelays => RelayType::KeyPackage,
            _ => RelayType::Nip65, // Default fallback
        }
    }
}

impl Relay {
    pub(crate) fn new(url: &RelayUrl) -> Self {
        Relay {
            id: None,
            url: url.clone(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    pub(crate) fn defaults() -> Vec<Relay> {
        let urls: &[&str] = if cfg!(debug_assertions) {
            &["ws://localhost:8080", "ws://localhost:7777"]
        } else {
            &[
                "wss://relay.damus.io",
                "wss://relay.primal.net",
                "wss://nos.lol",
            ]
        };

        urls.iter()
            .filter_map(|&url_str| RelayUrl::parse(url_str).ok())
            .map(|url| Relay::new(&url))
            .collect()
    }

    pub(crate) fn urls<'a, I>(relays: I) -> Vec<RelayUrl>
    where
        I: IntoIterator<Item = &'a Relay>,
    {
        relays.into_iter().map(|r| r.url.clone()).collect()
    }
}

impl Whitenoise {
    pub async fn find_or_create_relay_by_url(&self, url: &RelayUrl) -> Result<Relay> {
        Relay::find_or_create_by_url(url, &self.database).await
    }

    /// Get connection status for all of an account's relays.
    ///
    /// This method returns a list of relay statuses for relays that are configured
    /// for the given account. It retrieves relay URLs from the account's relay lists
    /// (NIP-65, inbox, and key package relays) and returns the current connection
    /// status from the Nostr client.
    ///
    /// # Arguments
    ///
    /// * `account` - The account whose relay statuses should be retrieved.
    ///
    /// # Returns
    ///
    /// Returns a vector of tuples containing relay URLs and their connection status.
    pub async fn get_account_relay_statuses(
        &self,
        account: &Account,
    ) -> Result<Vec<(RelayUrl, RelayStatus)>> {
        // Get all relay URLs for this user across all types
        let mut all_relays = Vec::new();
        all_relays.extend(account.nip65_relays(self).await?);
        all_relays.extend(account.inbox_relays(self).await?);
        all_relays.extend(account.key_package_relays(self).await?);

        // Remove duplicates by collecting unique relay URLs
        let mut unique_relay_urls = HashSet::new();
        for relay in all_relays {
            unique_relay_urls.insert(relay.url);
        }

        // Get current relay statuses from the Nostr client
        let mut relay_statuses = Vec::new();

        for relay_url in unique_relay_urls {
            // Try to get relay status from NostrManager
            match self.nostr.get_relay_status(&relay_url).await {
                Ok(status) => {
                    relay_statuses.push((relay_url, status));
                }
                Err(_) => {
                    // If we can't get the relay status, it's likely not connected
                    relay_statuses.push((relay_url, RelayStatus::Disconnected));
                }
            }
        }

        Ok(relay_statuses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_relay(url: &RelayUrl) -> super::Relay {
        super::Relay {
            id: None,
            url: url.clone(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    mod relay_tests {
        use super::*;

        #[test]
        fn test_urls_empty_list() {
            let relays: Vec<super::super::Relay> = vec![];
            let urls = super::super::Relay::urls(&relays);
            assert_eq!(urls.len(), 0);
        }

        #[test]
        fn test_urls_extracts_and_preserves_order() {
            let url1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
            let url2 = RelayUrl::parse("wss://relay2.example.com").unwrap();
            let url3 = RelayUrl::parse("wss://relay3.example.com").unwrap();

            let relays = vec![
                create_test_relay(&url1),
                create_test_relay(&url2),
                create_test_relay(&url3),
            ];

            let urls = super::super::Relay::urls(&relays);

            assert_eq!(urls, vec![url1, url2, url3]);
        }

        #[test]
        fn test_relay_new() {
            let url = RelayUrl::parse("wss://test.relay.com").unwrap();
            let relay = Relay::new(&url);

            assert!(relay.id.is_none());
            assert_eq!(relay.url, url);
        }

        #[test]
        fn test_relay_defaults_not_empty() {
            let defaults = Relay::defaults();
            assert!(!defaults.is_empty());
        }

        #[test]
        fn test_relay_equality() {
            let url = RelayUrl::parse("wss://test.relay.com").unwrap();
            let now = Utc::now();

            let relay1 = Relay {
                id: Some(1),
                url: url.clone(),
                created_at: now,
                updated_at: now,
            };

            let relay2 = Relay {
                id: Some(1),
                url: url.clone(),
                created_at: now,
                updated_at: now,
            };

            assert_eq!(relay1, relay2);
        }

        #[test]
        fn test_relay_hash() {
            let url = RelayUrl::parse("wss://test.relay.com").unwrap();
            let now = Utc::now();

            let relay = Relay {
                id: Some(1),
                url: url.clone(),
                created_at: now,
                updated_at: now,
            };

            let mut set = HashSet::new();
            set.insert(relay.clone());
            assert!(set.contains(&relay));
        }
    }

    mod relay_type_tests {
        use super::*;

        #[test]
        fn test_relay_type_from_str_nip65() {
            assert_eq!(RelayType::from_str("nip65").unwrap(), RelayType::Nip65);
            assert_eq!(RelayType::from_str("NIP65").unwrap(), RelayType::Nip65);
            assert_eq!(RelayType::from_str("Nip65").unwrap(), RelayType::Nip65);
        }

        #[test]
        fn test_relay_type_from_str_inbox() {
            assert_eq!(RelayType::from_str("inbox").unwrap(), RelayType::Inbox);
            assert_eq!(RelayType::from_str("INBOX").unwrap(), RelayType::Inbox);
            assert_eq!(RelayType::from_str("Inbox").unwrap(), RelayType::Inbox);
        }

        #[test]
        fn test_relay_type_from_str_key_package() {
            assert_eq!(
                RelayType::from_str("key_package").unwrap(),
                RelayType::KeyPackage
            );
            assert_eq!(
                RelayType::from_str("KEY_PACKAGE").unwrap(),
                RelayType::KeyPackage
            );
        }

        #[test]
        fn test_relay_type_from_str_invalid() {
            let result = RelayType::from_str("invalid");
            assert!(result.is_err());
            assert_eq!(result.unwrap_err(), "Invalid relay type: invalid");
        }

        #[test]
        fn test_relay_type_to_u16() {
            assert_eq!(u16::from(RelayType::Nip65), 10002);
            assert_eq!(u16::from(RelayType::Inbox), 10050);
            assert_eq!(u16::from(RelayType::KeyPackage), 10051);
        }

        #[test]
        fn test_relay_type_to_string() {
            assert_eq!(String::from(RelayType::Nip65), "nip65");
            assert_eq!(String::from(RelayType::Inbox), "inbox");
            assert_eq!(String::from(RelayType::KeyPackage), "key_package");
        }

        #[test]
        fn test_relay_type_to_kind() {
            assert_eq!(Kind::from(RelayType::Nip65), Kind::RelayList);
            assert_eq!(Kind::from(RelayType::Inbox), Kind::InboxRelays);
            assert_eq!(Kind::from(RelayType::KeyPackage), Kind::MlsKeyPackageRelays);
        }

        #[test]
        fn test_kind_to_relay_type() {
            assert_eq!(RelayType::from(Kind::RelayList), RelayType::Nip65);
            assert_eq!(RelayType::from(Kind::InboxRelays), RelayType::Inbox);
            assert_eq!(
                RelayType::from(Kind::MlsKeyPackageRelays),
                RelayType::KeyPackage
            );
        }

        #[test]
        fn test_kind_to_relay_type_fallback() {
            // Unknown kinds should fall back to Nip65
            assert_eq!(RelayType::from(Kind::TextNote), RelayType::Nip65);
            assert_eq!(RelayType::from(Kind::Metadata), RelayType::Nip65);
        }

        #[test]
        fn test_relay_type_roundtrip_via_kind() {
            // Test that RelayType -> Kind -> RelayType preserves the original value
            let types = [RelayType::Nip65, RelayType::Inbox, RelayType::KeyPackage];

            for original in types {
                let kind = Kind::from(original);
                let back = RelayType::from(kind);
                assert_eq!(original, back);
            }
        }

        #[test]
        fn test_relay_type_copy() {
            let relay_type = RelayType::Inbox;
            let copied = relay_type;
            assert_eq!(relay_type, copied);
        }

        #[test]
        fn test_relay_type_hash() {
            let mut set = HashSet::new();
            set.insert(RelayType::Nip65);
            set.insert(RelayType::Inbox);
            set.insert(RelayType::KeyPackage);

            assert_eq!(set.len(), 3);
            assert!(set.contains(&RelayType::Nip65));
            assert!(set.contains(&RelayType::Inbox));
            assert!(set.contains(&RelayType::KeyPackage));
        }
    }
}
