//! Per-account settings (notification preferences, etc.).

use chrono::{DateTime, Utc};
use nostr_sdk::PublicKey;
use serde::{Deserialize, Serialize};

use crate::whitenoise::{Whitenoise, accounts::Account, error::WhitenoiseError};

/// User-configurable settings scoped to a single account.
///
/// A default row is created lazily on first access. When no row exists in the
/// database the default behaviour is notifications enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSettings {
    pub id: Option<i64>,
    pub account_pubkey: PublicKey,
    pub notifications_enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Whitenoise {
    /// Returns the settings for `account`, creating a default row if none exists.
    pub async fn account_settings(
        &self,
        account: &Account,
    ) -> Result<AccountSettings, WhitenoiseError> {
        Ok(AccountSettings::find_or_create_for_pubkey(&account.pubkey, &self.database).await?)
    }

    /// Sets the notification preference for `account` and returns the updated settings.
    pub async fn update_notifications_enabled(
        &self,
        account: &Account,
        enabled: bool,
    ) -> Result<AccountSettings, WhitenoiseError> {
        Ok(
            AccountSettings::update_notifications_enabled(&account.pubkey, enabled, &self.database)
                .await?,
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::whitenoise::test_utils::*;

    #[tokio::test]
    async fn test_account_settings_default_and_update() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();

        let settings = whitenoise.account_settings(&account).await.unwrap();
        assert!(settings.notifications_enabled);
        assert_eq!(settings.account_pubkey, account.pubkey);

        let settings = whitenoise
            .update_notifications_enabled(&account, false)
            .await
            .unwrap();
        assert!(!settings.notifications_enabled);

        let settings = whitenoise
            .update_notifications_enabled(&account, true)
            .await
            .unwrap();
        assert!(settings.notifications_enabled);
    }
}
