use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use chrono::{DateTime, Utc};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    perf_instrument,
    relay_control::RelayPlane,
    whitenoise::{
        Whitenoise,
        accounts::Account,
        database::relay_status::{RelayStatusLookupKey, RelayStatusRecord},
        error::Result,
    },
};

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
    #[perf_instrument("relays")]
    pub async fn find_or_create_relay_by_url(&self, url: &RelayUrl) -> Result<Relay> {
        Relay::find_or_create_by_url(url, &self.database).await
    }

    /// Get connection status for all of an account's relays.
    ///
    /// This method returns a list of relay statuses for relays that are configured
    /// for the given account. It retrieves relay URLs from the account's relay lists
    /// (NIP-65, inbox, and key package relays) and returns the current connection
    /// status from the relay-control planes that own those relay types.
    ///
    /// # Arguments
    ///
    /// * `account` - The account whose relay statuses should be retrieved.
    ///
    /// # Returns
    ///
    /// Returns a vector of tuples containing relay URLs and their connection status.
    #[perf_instrument("relays")]
    pub async fn get_account_relay_statuses(
        &self,
        account: &Account,
    ) -> Result<Vec<(RelayUrl, RelayStatus)>> {
        fn record_is_connected(record: &RelayStatusRecord) -> bool {
            match (record.last_connect_success_at, record.last_failure_at) {
                (Some(success), Some(failure)) => success > failure,
                (Some(_), None) => true,
                _ => false,
            }
        }

        let mut relay_types_by_url: HashMap<RelayUrl, HashSet<RelayType>> = HashMap::new();
        for (relay_type, relays) in [
            (RelayType::Nip65, account.nip65_relays(self).await?),
            (RelayType::Inbox, account.inbox_relays(self).await?),
            (
                RelayType::KeyPackage,
                account.key_package_relays(self).await?,
            ),
        ] {
            for relay in relays {
                relay_types_by_url
                    .entry(relay.url)
                    .or_default()
                    .insert(relay_type);
            }
        }

        let mut lookup_keys_by_url: HashMap<RelayUrl, Vec<RelayStatusLookupKey>> = HashMap::new();
        let mut all_lookup_keys = Vec::new();

        for (relay_url, relay_types) in &relay_types_by_url {
            let mut lookup_keys = Vec::new();

            if relay_types.contains(&RelayType::Inbox) {
                lookup_keys.push(RelayStatusLookupKey {
                    relay_url: relay_url.clone(),
                    plane: RelayPlane::AccountInbox,
                    account_pubkey: Some(account.pubkey),
                });
            }

            if relay_types.contains(&RelayType::Nip65)
                || relay_types.contains(&RelayType::KeyPackage)
            {
                lookup_keys.push(RelayStatusLookupKey {
                    relay_url: relay_url.clone(),
                    plane: RelayPlane::Ephemeral,
                    account_pubkey: Some(account.pubkey),
                });
                lookup_keys.push(RelayStatusLookupKey {
                    relay_url: relay_url.clone(),
                    plane: RelayPlane::Ephemeral,
                    account_pubkey: None,
                });
            }

            all_lookup_keys.extend(lookup_keys.iter().cloned());
            lookup_keys_by_url.insert(relay_url.clone(), lookup_keys);
        }

        let status_records = RelayStatusRecord::find_many(&all_lookup_keys, &self.database).await?;
        let records_by_key = status_records
            .into_iter()
            .map(|record| (record.lookup_key(), record))
            .collect::<HashMap<_, _>>();

        let mut relay_statuses = Vec::with_capacity(relay_types_by_url.len());

        for relay_url in relay_types_by_url.into_keys() {
            let relevant_records = lookup_keys_by_url
                .remove(&relay_url)
                .unwrap_or_default()
                .into_iter()
                .map(|lookup_key| records_by_key.get(&lookup_key).cloned())
                .collect::<Vec<_>>();

            let status = if relevant_records.iter().flatten().any(record_is_connected) {
                RelayStatus::Connected
            } else {
                RelayStatus::Disconnected
            };
            relay_statuses.push((relay_url, status));
        }

        relay_statuses.sort_unstable_by(|left, right| left.0.as_str().cmp(right.0.as_str()));

        Ok(relay_statuses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        relay_control::observability::{RelayTelemetry, RelayTelemetryKind},
        whitenoise::test_utils::*,
    };

    fn create_test_relay(url: &RelayUrl) -> super::Relay {
        super::Relay {
            id: None,
            url: url.clone(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    async fn persist_connected_status(
        whitenoise: &Whitenoise,
        relay_url: &RelayUrl,
        plane: RelayPlane,
        account_pubkey: Option<PublicKey>,
    ) {
        let telemetry = match account_pubkey {
            Some(account_pubkey) => {
                RelayTelemetry::new(RelayTelemetryKind::Connected, plane, relay_url.clone())
                    .with_account_pubkey(account_pubkey)
            }
            None => RelayTelemetry::new(RelayTelemetryKind::Connected, plane, relay_url.clone()),
        };

        RelayStatusRecord::upsert_from_telemetry(&telemetry, &whitenoise.database)
            .await
            .unwrap();
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

    mod whitenoise_relay_tests {
        use super::*;

        #[tokio::test]
        async fn test_get_account_relay_statuses_empty() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (account, keys) = create_test_account(&whitenoise).await;
            let account = account.save(&whitenoise.database).await.unwrap();
            whitenoise.secrets_store.store_private_key(&keys).unwrap();

            // Account with no relays should return an empty list.
            let statuses = whitenoise
                .get_account_relay_statuses(&account)
                .await
                .unwrap();
            assert!(
                statuses.is_empty(),
                "Expected no relay statuses for a fresh account"
            );
        }

        #[tokio::test]
        async fn test_get_account_relay_statuses_with_relays() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (account, keys) = create_test_account(&whitenoise).await;
            let account = account.save(&whitenoise.database).await.unwrap();
            whitenoise.secrets_store.store_private_key(&keys).unwrap();

            // Add some relays to the account's user.
            let user = account.user(&whitenoise.database).await.unwrap();
            let url1 = RelayUrl::parse("wss://relay1.example.com").unwrap();
            let url2 = RelayUrl::parse("wss://relay2.example.com").unwrap();
            let relay1 = whitenoise.find_or_create_relay_by_url(&url1).await.unwrap();
            let relay2 = whitenoise.find_or_create_relay_by_url(&url2).await.unwrap();
            user.add_relays(&[relay1, relay2], RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            let statuses = whitenoise
                .get_account_relay_statuses(&account)
                .await
                .unwrap();
            assert_eq!(statuses.len(), 2);
            // Relays aren't in the client pool, so they should show as Disconnected.
            for (_url, status) in &statuses {
                assert_eq!(
                    *status,
                    nostr_sdk::RelayStatus::Disconnected,
                    "Non-pooled relays should appear as Disconnected"
                );
            }
        }

        #[tokio::test]
        async fn test_get_account_relay_statuses_deduplicates() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (account, keys) = create_test_account(&whitenoise).await;
            let account = account.save(&whitenoise.database).await.unwrap();
            whitenoise.secrets_store.store_private_key(&keys).unwrap();

            // Add the same relay URL to both NIP-65 and Inbox types.
            let user = account.user(&whitenoise.database).await.unwrap();
            let url = RelayUrl::parse("wss://shared.relay.example.com").unwrap();
            let relay = whitenoise.find_or_create_relay_by_url(&url).await.unwrap();
            user.add_relays(
                std::slice::from_ref(&relay),
                RelayType::Nip65,
                &whitenoise.database,
            )
            .await
            .unwrap();
            user.add_relays(&[relay], RelayType::Inbox, &whitenoise.database)
                .await
                .unwrap();

            let statuses = whitenoise
                .get_account_relay_statuses(&account)
                .await
                .unwrap();
            // Should be deduplicated to 1 entry.
            assert_eq!(
                statuses.len(),
                1,
                "Duplicate relay URLs should be deduplicated"
            );
        }

        #[tokio::test]
        async fn test_get_account_relay_statuses_ignores_unrelated_discovery_status() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (account, keys) = create_test_account(&whitenoise).await;
            let account = account.save(&whitenoise.database).await.unwrap();
            whitenoise.secrets_store.store_private_key(&keys).unwrap();

            let user = account.user(&whitenoise.database).await.unwrap();
            let relay_url = RelayUrl::parse("ws://localhost:7777").unwrap();
            let relay = whitenoise
                .find_or_create_relay_by_url(&relay_url)
                .await
                .unwrap();
            user.add_relays(&[relay], RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            persist_connected_status(&whitenoise, &relay_url, RelayPlane::Discovery, None).await;

            let statuses = whitenoise
                .get_account_relay_statuses(&account)
                .await
                .unwrap();

            assert_eq!(statuses, vec![(relay_url, RelayStatus::Disconnected)]);
        }

        #[tokio::test]
        async fn test_get_account_relay_statuses_reads_account_inbox_plane() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (account, keys) = create_test_account(&whitenoise).await;
            let account = account.save(&whitenoise.database).await.unwrap();
            whitenoise.secrets_store.store_private_key(&keys).unwrap();

            let user = account.user(&whitenoise.database).await.unwrap();
            let relay_url = RelayUrl::parse("ws://localhost:8080").unwrap();
            let relay = whitenoise
                .find_or_create_relay_by_url(&relay_url)
                .await
                .unwrap();
            user.add_relays(&[relay], RelayType::Inbox, &whitenoise.database)
                .await
                .unwrap();

            persist_connected_status(
                &whitenoise,
                &relay_url,
                RelayPlane::AccountInbox,
                Some(account.pubkey),
            )
            .await;

            let statuses = whitenoise
                .get_account_relay_statuses(&account)
                .await
                .unwrap();

            assert_eq!(statuses, vec![(relay_url, RelayStatus::Connected)]);
        }

        #[tokio::test]
        async fn test_get_account_relay_statuses_reads_ephemeral_plane_for_nip65() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (account, keys) = create_test_account(&whitenoise).await;
            let account = account.save(&whitenoise.database).await.unwrap();
            whitenoise.secrets_store.store_private_key(&keys).unwrap();

            let user = account.user(&whitenoise.database).await.unwrap();
            let relay_url = RelayUrl::parse("ws://localhost:7777").unwrap();
            let relay = whitenoise
                .find_or_create_relay_by_url(&relay_url)
                .await
                .unwrap();
            user.add_relays(&[relay], RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            persist_connected_status(
                &whitenoise,
                &relay_url,
                RelayPlane::Ephemeral,
                Some(account.pubkey),
            )
            .await;

            let statuses = whitenoise
                .get_account_relay_statuses(&account)
                .await
                .unwrap();

            assert_eq!(statuses, vec![(relay_url, RelayStatus::Connected)]);
        }

        #[tokio::test]
        async fn test_get_account_relay_statuses_reads_unscoped_ephemeral_plane_for_nip65() {
            let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
            let (account, keys) = create_test_account(&whitenoise).await;
            let account = account.save(&whitenoise.database).await.unwrap();
            whitenoise.secrets_store.store_private_key(&keys).unwrap();

            let user = account.user(&whitenoise.database).await.unwrap();
            let relay_url = RelayUrl::parse("ws://localhost:7777").unwrap();
            let relay = whitenoise
                .find_or_create_relay_by_url(&relay_url)
                .await
                .unwrap();
            user.add_relays(&[relay], RelayType::Nip65, &whitenoise.database)
                .await
                .unwrap();

            persist_connected_status(&whitenoise, &relay_url, RelayPlane::Ephemeral, None).await;

            let statuses = whitenoise
                .get_account_relay_statuses(&account)
                .await
                .unwrap();

            assert_eq!(statuses, vec![(relay_url, RelayStatus::Connected)]);
        }
    }
}
