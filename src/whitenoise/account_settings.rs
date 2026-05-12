//! Per-account settings (notification preferences, etc.).

use chrono::{DateTime, Utc};
use nostr_sdk::PublicKey;
use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use crate::whitenoise::test_utils::*;

    #[tokio::test]
    async fn test_account_settings_default_and_update() {
        let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
        let account = whitenoise.create_identity().await.unwrap();
        let session = whitenoise.require_session(&account.pubkey).unwrap();

        let settings = session.settings().get().await.unwrap();
        assert!(settings.notifications_enabled);
        assert_eq!(settings.account_pubkey, account.pubkey);

        let settings = session
            .settings()
            .update_notifications_enabled(false)
            .await
            .unwrap();
        assert!(!settings.notifications_enabled);

        let settings = session
            .settings()
            .update_notifications_enabled(true)
            .await
            .unwrap();
        assert!(settings.notifications_enabled);
    }
}
