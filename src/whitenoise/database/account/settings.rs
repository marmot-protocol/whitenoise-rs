//! Per-account repository for account settings.
//!
//! Wraps the existing [`AccountSettings`] DB functions so that callers do not
//! need to thread an `account_pubkey` argument through every call — the pubkey
//! is baked in at construction time.

use std::sync::Arc;

use nostr_sdk::PublicKey;

use crate::whitenoise::account_settings::AccountSettings;
use crate::whitenoise::database::{Database, DatabaseError};
use crate::whitenoise::error::{Result, WhitenoiseError};

/// Repository for account settings scoped to a single account.
#[derive(Clone, Debug)]
pub struct AccountSettingsRepo {
    account_pubkey: PublicKey,
    db: Arc<Database>,
}

impl AccountSettingsRepo {
    /// Construct a new [`AccountSettingsRepo`] for `account_pubkey`.
    pub(crate) fn new(account_pubkey: PublicKey, db: Arc<Database>) -> Self {
        Self { account_pubkey, db }
    }

    /// Return the settings for this account, creating a default row if none
    /// exists.
    ///
    /// Delegates to `AccountSettings::find_or_create_for_pubkey`.
    pub async fn find_or_create(&self) -> Result<AccountSettings> {
        AccountSettings::find_or_create_for_pubkey(&self.account_pubkey, &self.db)
            .await
            .map_err(|e| WhitenoiseError::Database(DatabaseError::Sqlx(e)))
    }

    /// Return whether notifications are enabled for this account.
    ///
    /// Returns `true` (the default) when no settings row exists.
    ///
    /// Delegates to `AccountSettings::notifications_enabled_for_pubkey`.
    pub async fn notifications_enabled(&self) -> Result<bool> {
        AccountSettings::notifications_enabled_for_pubkey(&self.account_pubkey, &self.db)
            .await
            .map_err(|e| WhitenoiseError::Database(DatabaseError::Sqlx(e)))
    }

    /// Set `notifications_enabled` for this account and return the updated
    /// settings.
    ///
    /// Delegates to `AccountSettings::update_notifications_enabled`.
    ///
    /// **Note:** this only updates the database row. It does **not** perform
    /// push-token reconciliation (sharing or removing the local push token
    /// from joined groups). Callers that need the full side-effectful behaviour
    /// should use `Whitenoise::update_notifications_enabled` instead, or handle
    /// push-token sync separately. Phase 5 callers must account for this when
    /// migrating away from the `Whitenoise` facade.
    pub async fn update_notifications_enabled(&self, enabled: bool) -> Result<AccountSettings> {
        AccountSettings::update_notifications_enabled(&self.account_pubkey, enabled, &self.db)
            .await
            .map_err(|e| WhitenoiseError::Database(DatabaseError::Sqlx(e)))
    }
}

#[cfg(test)]
mod tests {
    use nostr_sdk::{Keys, PublicKey};

    use super::AccountSettingsRepo;
    use crate::whitenoise::database::Database;
    use crate::whitenoise::test_utils::create_mock_whitenoise;

    async fn insert_test_account(database: &Database, pubkey: &PublicKey) {
        let user_pubkey = pubkey.to_hex();
        sqlx::query("INSERT INTO users (pubkey, metadata) VALUES (?, '{}')")
            .bind(&user_pubkey)
            .execute(&database.pool)
            .await
            .expect("insert user");
        let (user_id,): (i64,) = sqlx::query_as("SELECT id FROM users WHERE pubkey = ?")
            .bind(&user_pubkey)
            .fetch_one(&database.pool)
            .await
            .expect("get user id");
        sqlx::query("INSERT INTO accounts (pubkey, user_id, last_synced_at) VALUES (?, ?, NULL)")
            .bind(&user_pubkey)
            .bind(user_id)
            .execute(&database.pool)
            .await
            .expect("insert account");
    }

    #[tokio::test]
    async fn find_or_create_returns_defaults_for_new_account() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;

        let repo = AccountSettingsRepo::new(keys.public_key(), wn.database.clone());
        let settings = repo.find_or_create().await.unwrap();

        assert!(settings.id.is_some());
        assert_eq!(settings.account_pubkey, keys.public_key());
        assert!(settings.notifications_enabled);
    }

    #[tokio::test]
    async fn find_or_create_is_idempotent() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;

        let repo = AccountSettingsRepo::new(keys.public_key(), wn.database.clone());
        let first = repo.find_or_create().await.unwrap();
        let second = repo.find_or_create().await.unwrap();

        assert_eq!(first.id, second.id);
    }

    #[tokio::test]
    async fn notifications_enabled_defaults_to_true_when_no_row() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();

        let repo = AccountSettingsRepo::new(keys.public_key(), wn.database.clone());
        assert!(repo.notifications_enabled().await.unwrap());
    }

    #[tokio::test]
    async fn update_notifications_enabled_persists_value() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;

        let repo = AccountSettingsRepo::new(keys.public_key(), wn.database.clone());

        let updated = repo.update_notifications_enabled(false).await.unwrap();
        assert!(!updated.notifications_enabled);

        assert!(!repo.notifications_enabled().await.unwrap());
    }

    #[tokio::test]
    async fn update_notifications_enabled_round_trips() {
        let (wn, _d, _l) = create_mock_whitenoise().await;
        let keys = Keys::generate();
        insert_test_account(&wn.database, &keys.public_key()).await;

        let repo = AccountSettingsRepo::new(keys.public_key(), wn.database.clone());

        let disabled = repo.update_notifications_enabled(false).await.unwrap();
        assert!(!disabled.notifications_enabled);

        let enabled = repo.update_notifications_enabled(true).await.unwrap();
        assert!(enabled.notifications_enabled);
        assert!(enabled.updated_at >= disabled.updated_at);
    }
}
