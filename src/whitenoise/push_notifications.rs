//! Push notification registration state and per-group token cache models.

use core::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use mdk_core::prelude::GroupId;
use nostr_sdk::{PublicKey, RelayUrl};
use serde::{Deserialize, Serialize};

use crate::perf_instrument;
use crate::whitenoise::{
    Whitenoise,
    accounts::Account,
    error::{Result, WhitenoiseError},
};

/// Supported native push-token platforms for device registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PushPlatform {
    #[serde(rename = "apns")]
    Apns,
    #[serde(rename = "fcm")]
    Fcm,
}

impl PushPlatform {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Apns => "apns",
            Self::Fcm => "fcm",
        }
    }
}

impl fmt::Display for PushPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PushPlatform {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "apns" => Ok(Self::Apns),
            "fcm" => Ok(Self::Fcm),
            _ => Err(format!("Invalid push platform: {value}")),
        }
    }
}

/// This device's locally persisted push registration for a specific account.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PushRegistration {
    pub account_pubkey: PublicKey,
    pub platform: PushPlatform,
    pub raw_token: String,
    pub server_pubkey: PublicKey,
    pub relay_hint: Option<RelayUrl>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_shared_at: Option<DateTime<Utc>>,
}

/// Cached encrypted push token for a group member leaf.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupPushToken {
    pub account_pubkey: PublicKey,
    pub group_id: GroupId,
    pub leaf_index: u32,
    pub server_pubkey: PublicKey,
    pub relay_hint: Option<RelayUrl>,
    pub encrypted_token: String,
    pub updated_at: DateTime<Utc>,
}

impl Whitenoise {
    /// Returns the locally stored push registration for `account`, if present.
    #[perf_instrument("push_notifications")]
    pub async fn push_registration(&self, account: &Account) -> Result<Option<PushRegistration>> {
        Ok(PushRegistration::find_by_account_pubkey(&account.pubkey, &self.database).await?)
    }

    /// Creates or replaces the locally stored push registration for `account`.
    ///
    /// This only updates local persistence. MLS token sharing and notification
    /// request publication are handled in later MIP-05 PRs.
    #[perf_instrument("push_notifications")]
    pub async fn upsert_push_registration(
        &self,
        account: &Account,
        platform: PushPlatform,
        raw_token: &str,
        server_pubkey: &PublicKey,
        relay_hint: Option<&RelayUrl>,
    ) -> Result<PushRegistration> {
        validate_raw_token(raw_token)?;

        Ok(PushRegistration::upsert(
            &account.pubkey,
            platform,
            raw_token,
            server_pubkey,
            relay_hint,
            &self.database,
        )
        .await?)
    }

    /// Removes the locally stored push registration for `account`.
    #[perf_instrument("push_notifications")]
    pub async fn clear_push_registration(&self, account: &Account) -> Result<()> {
        PushRegistration::delete_by_account_pubkey(&account.pubkey, &self.database).await?;
        Ok(())
    }
}

fn validate_raw_token(raw_token: &str) -> Result<()> {
    if raw_token.trim().is_empty() {
        return Err(WhitenoiseError::InvalidInput(
            "push registration token must not be empty".to_string(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use nostr_sdk::{Keys, RelayUrl};

    use super::*;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    #[tokio::test]
    async fn test_public_push_registration_lifecycle() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let server_pubkey = Keys::generate().public_key();
        let relay_hint = RelayUrl::parse("wss://push.example.com").unwrap();

        assert!(
            whitenoise
                .push_registration(&account)
                .await
                .unwrap()
                .is_none()
        );

        let created = whitenoise
            .upsert_push_registration(
                &account,
                PushPlatform::Apns,
                "token-one",
                &server_pubkey,
                Some(&relay_hint),
            )
            .await
            .unwrap();
        assert_eq!(created.platform, PushPlatform::Apns);
        assert_eq!(created.raw_token, "token-one");
        assert_eq!(created.server_pubkey, server_pubkey);
        assert_eq!(created.relay_hint, Some(relay_hint.clone()));

        let replaced = whitenoise
            .upsert_push_registration(
                &account,
                PushPlatform::Fcm,
                "token-two",
                &server_pubkey,
                None,
            )
            .await
            .unwrap();
        assert_eq!(replaced.platform, PushPlatform::Fcm);
        assert_eq!(replaced.raw_token, "token-two");
        assert_eq!(replaced.relay_hint, None);

        let stored = whitenoise
            .push_registration(&account)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored, replaced);

        whitenoise.clear_push_registration(&account).await.unwrap();
        assert!(
            whitenoise
                .push_registration(&account)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_registration_remains_stored_when_notifications_disabled() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let server_pubkey = Keys::generate().public_key();

        whitenoise
            .upsert_push_registration(
                &account,
                PushPlatform::Apns,
                "device-token",
                &server_pubkey,
                None,
            )
            .await
            .unwrap();

        let settings = whitenoise
            .update_notifications_enabled(&account, false)
            .await
            .unwrap();
        assert!(!settings.notifications_enabled);

        let stored = whitenoise.push_registration(&account).await.unwrap();
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().raw_token, "device-token");
    }

    #[tokio::test]
    async fn test_upsert_push_registration_rejects_blank_token() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let server_pubkey = Keys::generate().public_key();

        let err = whitenoise
            .upsert_push_registration(&account, PushPlatform::Apns, "   ", &server_pubkey, None)
            .await
            .unwrap_err();

        assert!(matches!(err, WhitenoiseError::InvalidInput(_)));
    }
}
